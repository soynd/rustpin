//! Pinterest search + "more like this" through Pinterest's own web API.
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

const UA: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0";
const FALLBACK_APP_VERSION: &str = "bb7a14c";
const PAGE_SIZE: usize = 25;

#[derive(Clone, Debug)]
pub struct Pin {
    pub id: String,
    pub title: String,
    pub thumb: String,
    pub preview: String,
    pub orig: String,
    pub page_url: String,
    pub w: u32,
    pub h: u32,
}

impl Pin {
    pub fn aspect(&self) -> f32 {
        if self.w > 0 && self.h > 0 {
            self.w as f32 / self.h as f32
        } else {
            1.0
        }
    }
}

pub struct Client {
    agent: ureq::Agent,
    cookies: Mutex<Vec<(String, String)>>,
    app_version: Mutex<String>,
}

impl Client {
    /// Opens a session so Pinterest hands out a csrftoken.
    pub fn new() -> Result<Self, String> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .build();
        let me = Self {
            agent,
            cookies: Mutex::new(Vec::new()),
            app_version: Mutex::new(FALLBACK_APP_VERSION.to_string()),
        };
        let resp = me
            .agent
            .get("https://www.pinterest.com/")
            .set("User-Agent", UA)
            .set("Accept", "text/html")
            .call()
            .map_err(|e| e.to_string())?;
        me.remember(&resp);
        let html = resp.into_string().map_err(|e| e.to_string())?;
        if let Some(v) = app_version_of(&html) {
            if let Ok(mut slot) = me.app_version.lock() {
                *slot = v;
            }
        }
        Ok(me)
    }

    fn remember(&self, resp: &ureq::Response) {
        let Ok(mut jar) = self.cookies.lock() else { return };
        for raw in resp.all("set-cookie") {
            let Some(pair) = raw.split(';').next() else { continue };
            let Some((name, value)) = pair.split_once('=') else { continue };
            let (name, value) = (name.trim().to_string(), value.trim().to_string());
            if name.is_empty() {
                continue;
            }
            match jar.iter_mut().find(|(n, _)| *n == name) {
                Some(slot) => slot.1 = value,
                None => jar.push((name, value)),
            }
        }
    }

    fn cookie_header(&self) -> String {
        let Ok(jar) = self.cookies.lock() else { return String::new() };
        jar.iter()
            .map(|(n, v)| format!("{n}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn cookie(&self, name: &str) -> String {
        self.cookies
            .lock()
            .ok()
            .and_then(|jar| {
                jar.iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v.clone())
            })
            .unwrap_or_default()
    }

    fn api_req(
        &self,
        resource: &str,
        options: Value,
        referer: &str,
        handler: &str,
    ) -> Result<Value, String> {
        let source_url = options
            .get("source_url")
            .and_then(|v| v.as_str())
            .unwrap_or("/")
            .to_string();
        let data = json!({ "options": options, "context": {} });
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let url = format!(
            "https://www.pinterest.com/resource/{resource}/get/?source_url={}&data={}&_={stamp}",
            encode(&source_url),
            encode(&data.to_string())
        );
        let cookies = self.cookie_header();
        let csrf = self.cookie("csrftoken");
        let app_version = self
            .app_version
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        let mut req = self
            .agent
            .get(&url)
            .set("User-Agent", UA)
            .set("Accept", "application/json, text/javascript, */*; q=0.01")
            .set("X-Requested-With", "XMLHttpRequest")
            .set("X-Pinterest-AppState", "active")
            .set("X-Pinterest-PWS-Handler", handler)
            .set("Referer", referer);
        if !cookies.is_empty() {
            req = req.set("Cookie", &cookies);
        }
        if !csrf.is_empty() {
            req = req.set("X-CSRFToken", &csrf);
        }
        if !app_version.is_empty() {
            req = req.set("X-Pinterest-App-Version", &app_version);
        }
        let resp = req.call().map_err(|e| e.to_string())?;
        self.remember(&resp);
        resp.into_json().map_err(|e| e.to_string())
    }

    pub fn search(
        &self,
        query: &str,
        bookmarks: &[String],
    ) -> Result<(Vec<Pin>, Vec<String>), String> {
        let source_url = format!("/search/pins/?q={query}rs=typed");
        let options = json!({
            "appliedProductFilters": "---",
            "auto_correction_disabled": false,
            "bookmarks": bookmarks,
            "page_size": PAGE_SIZE,
            "query": query,
            "redux_normalize_feed": true,
            "rs": "typed",
            "scope": "pins",
            "source_url": source_url,
        });
        let data = self.api_req(
            "BaseSearchResource",
            options,
            &format!("https://www.pinterest.com/search/pins/?q={}", encode(query)),
            "www/search/[scope].js",
        )?;
        let items = data
            .pointer("/resource_response/data/results")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok((parse_pins(&items), bookmarks_of(&data)))
    }

    pub fn related(
        &self,
        pin_id: &str,
        bookmarks: &[String],
    ) -> Result<(Vec<Pin>, Vec<String>), String> {
        let source_url = format!("/pin/{pin_id}/");
        let options = json!({
            "field_set_key": "unauth_react",
            "page_size": PAGE_SIZE,
            "pin_id": pin_id,
            "bookmarks": bookmarks,
        });
        let data = self.api_req(
            "RelatedModulesResource",
            options,
            &format!("https://www.pinterest.com{source_url}"),
            "www/pin/[id].js",
        )?;
        let items = match data.pointer("/resource_response/data") {
            Some(Value::Array(a)) => a.clone(),
            Some(v) => v
                .get("results")
                .and_then(|r| r.as_array())
                .cloned()
                .unwrap_or_default(),
            None => Vec::new(),
        };
        Ok((parse_pins(&items), bookmarks_of(&data)))
    }
}

fn app_version_of(html: &str) -> Option<String> {
    let key = "\"appVersion\":";
    let start = html.find(key)? + key.len();
    let rest = html[start..].trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn url_of(images: &Value, size: &str) -> String {
    images
        .get(size)
        .and_then(|v| v.get("url"))
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string()
}

fn dims_of(images: &Value, size: &str) -> (u32, u32) {
    let node = images.get(size);
    let num = |key: &str| {
        node.and_then(|n| n.get(key))
            .and_then(|v| v.as_f64())
            .map(|v| v.max(1.0) as u32)
            .unwrap_or(0)
    };
    (num("width"), num("height"))
}

fn first_str(item: &Value, keys: &[&str]) -> String {
    for k in keys {
        if let Some(s) = item.get(*k).and_then(|v| v.as_str()) {
            if !s.trim().is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

/// Cut to at most `bytes` on a character boundary, so a title with emoji or
/// accents in it cannot split a char and panic.
fn clip(text: &str, bytes: usize) -> String {
    let mut cut = bytes.min(text.len());
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text[..cut].to_string()
}

fn parse_pins(items: &[Value]) -> Vec<Pin> {
    let mut pins = Vec::new();
    for item in items {
        let Some(images) = item.get("images").filter(|v| v.is_object()) else { continue };
        let orig = url_of(images, "orig");
        if orig.is_empty() {
            continue;
        }
        let (mut w, mut h) = dims_of(images, "orig");
        if w == 0 || h == 0 {
            let (pw, ph) = dims_of(images, "736x");
            w = pw;
            h = ph;
        }
        let id = first_str(item, &["id", "pin_id"]);
        let mut title = first_str(item, &["grid_title", "title", "description"]);
        if title.is_empty() {
            title = "untitled".to_string();
        }
        title = clip(&title, 120);
        let preview = {
            let p = url_of(images, "736x");
            if p.is_empty() { orig.clone() } else { p }
        };
        let thumb = {
            let t = url_of(images, "236x");
            if t.is_empty() { preview.clone() } else { t }
        };
        pins.push(Pin {
            page_url: if id.is_empty() {
                String::new()
            } else {
                format!("https://www.pinterest.com/pin/{id}/")
            },
            id,
            title,
            thumb,
            preview,
            orig,
            w,
            h,
        });
    }
    pins
}

fn bookmarks_of(data: &Value) -> Vec<String> {
    let raw = data
        .pointer("/resource/options/bookmarks")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if raw.len() == 1 && raw[0].as_str() == Some("-end-") {
        return Vec::new();
    }
    raw.into_iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect()
}

pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}