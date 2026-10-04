//! Persistent settings + the picture library location.
use std::fs;
use std::path::{Path, PathBuf};

/// Resolution presets offered in the UI (`name`, width, height).
pub const PRESETS: [(&str, u32, u32); 4] = [
    ("720p", 1280, 720),
    ("1080p", 1920, 1080),
    ("1440p", 2560, 1440),
    ("2160p", 3840, 2160),
];

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum Target {
    /// Auto: fit the upscale inside 1080p, upscaling only as far as needed.
    #[default]
    Auto,
    /// One of [`PRESETS`], oriented to the image.
    Preset(&'static str, u32, u32),
    /// A resolution typed by hand, used exactly as given.
    Exact(u32, u32),
}

impl Target {
    pub fn key(self) -> String {
        match self {
            Target::Auto => "auto".into(),
            Target::Preset(name, _, _) => name.to_string(),
            Target::Exact(w, h) => format!("{w}x{h}"),
        }
    }

    pub fn parse(raw: &str) -> Option<Target> {
        let t = raw.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("auto") {
            return Some(Target::Auto);
        }
        for (name, w, h) in PRESETS {
            if t.eq_ignore_ascii_case(name) {
                return Some(Target::Preset(name, w, h));
            }
        }
        let (a, b) = t.split_once(['x', 'X'])?;
        let w = a.trim().parse().ok()?;
        let h = b.trim().parse().ok()?;
        if w < 16 || h < 16 {
            return None;
        }
        Some(Target::Exact(w, h))
    }

    pub fn label(self) -> String {
        match self {
            Target::Auto => "Auto — match 1080p".into(),
            Target::Preset(name, w, h) => format!("{name} — {w}×{h}"),
            Target::Exact(w, h) => format!("Custom — {w}×{h}"),
        }
    }

    /// The box the result has to fit in. Auto/preset boxes are turned to
    /// match the image (a portrait pin gets 1080×1920), a custom box is used
    /// exactly as typed.
    pub fn box_for(self, w: u32, h: u32) -> (u32, u32) {
        let (bw, bh) = match self {
            Target::Auto => (1920, 1080),
            Target::Preset(_, w, h) => (w, h),
            Target::Exact(w, h) => return (w, h),
        };
        if h > w { (bh, bw) } else { (bw, bh) }
    }
}

/// Upscaling factor: the smallest of 4x/3x/2x that still reaches the target.
pub fn auto_scale(target: Target, w: u32, h: u32) -> u32 {
    let (bw, bh) = target.box_for(w.max(1), h.max(1));
    let need = (bw as f64 / w.max(1) as f64).min(bh as f64 / h.max(1) as f64);
    let mut scale = 4;
    for cand in [2u32, 3, 4] {
        if cand as f64 >= need {
            scale = cand;
            break;
        }
    }
    while scale > 2 && w.max(h) as u64 * scale as u64 > 8192 {
        scale -= 1;
    }
    scale
}

/// Size to hand over to the caller, or `None` to keep the upscaled size.
pub fn fit_size(target: Target, w: u32, h: u32) -> Option<(u32, u32)> {
    if w < 2 || h < 2 {
        return None;
    }
    let (bw, bh) = target.box_for(w, h);
    if w <= bw && h <= bh {
        return None;
    }
    let s = (bw as f64 / w as f64).min(bh as f64 / h as f64);
    let nw = (((w as f64 * s).round() as u32).max(16)) & !1;
    let nh = (((h as f64 * s).round() as u32).max(16)) & !1;
    if nw == w && nh == h {
        None
    } else {
        Some((nw, nh))
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub delete_orig: bool,
    pub target: Target,
    /// Brought back on the next start, so the app opens on the same search.
    pub last_query: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self { delete_orig: true, target: Target::Auto, last_query: String::new() }
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// Every downloaded picture lands directly in here, no sub-folders.
pub fn library_dir() -> PathBuf {
    match std::env::var_os("RUSTPIN_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => home().join("Pictures").join("rustpin"),
    }
}

pub fn config_path() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d).join("rustpin").join("settings.json"),
        _ => home().join(".config").join("rustpin").join("settings.json"),
    }
}

fn legacy_config() -> PathBuf {
    library_dir().join("settings.json")
}

pub fn load() -> Settings {
    let mut s = Settings::default();
    let mut paths = vec![config_path(), legacy_config()];
    paths.dedup();
    for path in paths {
        let Ok(raw) = fs::read_to_string(&path) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else { continue };
        if let Some(b) = v.get("delete_orig").and_then(|b| b.as_bool()) {
            s.delete_orig = b;
        }
        if let Some(t) = v.get("target").and_then(|t| t.as_str()).and_then(Target::parse) {
            s.target = t;
        }
        if let Some(q) = v.get("last_query").and_then(|q| q.as_str()) {
            s.last_query = q.trim().chars().take(120).collect();
        }
        break;
    }
    s
}

/// Settings are written the moment they change, so nothing gets lost.
pub fn save(s: &Settings) {
    let path = config_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let body = serde_json::json!({
        "delete_orig": s.delete_orig,
        "target": s.target.key(),
        "last_query": s.last_query,
    });
    if let Ok(text) = serde_json::to_string_pretty(&body) {
        let _ = fs::write(&path, text);
    }
}

pub fn ensure_library() -> PathBuf {
    let dir = library_dir();
    let _ = fs::create_dir_all(&dir);
    dir
}

/// A free filename: `name.png`, then `name-2.png`, `name-3.png`, …
pub fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let mut out = dir.join(format!("{stem}.{ext}"));
    if !out.exists() {
        return out;
    }
    for n in 2..1000 {
        out = dir.join(format!("{stem}-{n}.{ext}"));
        if !out.exists() {
            return out;
        }
    }
    dir.join(format!("{stem}-{}.{ext}", now_nanos()))
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_round_trips_through_text() {
        for t in [Target::Auto, Target::Exact(2560, 1440)] {
            assert_eq!(Target::parse(&t.key()), Some(t));
        }
        for (name, w, h) in PRESETS {
            assert_eq!(Target::parse(name), Some(Target::Preset(name, w, h)));
        }
        assert_eq!(Target::parse(""), Some(Target::Auto));
        assert_eq!(Target::parse("nonsense"), None);
        assert_eq!(Target::parse("12x12"), None);
    }

    #[test]
    fn boxes_turn_with_the_image() {
        assert_eq!(Target::Auto.box_for(736, 1300), (1080, 1920));
        assert_eq!(Target::Auto.box_for(1300, 736), (1920, 1080));
        assert_eq!(Target::Preset("1080p", 1920, 1080).box_for(736, 1300), (1080, 1920));
        assert_eq!(Target::Exact(800, 600).box_for(736, 1300), (800, 600));
    }

    #[test]
    fn scale_is_the_smallest_one_that_reaches_the_target() {
        // 736x1300 portrait needs 1080/736 = 1.47x, so 2x is enough
        assert_eq!(auto_scale(Target::Auto, 736, 1300), 2);
        // 474x840 needs 1080/474 = 2.28x
        assert_eq!(auto_scale(Target::Auto, 474, 840), 3);
        // a 4K portrait box needs 2160/736 = 2.93x of the 736x1300 pin
        assert_eq!(auto_scale(Target::Preset("2160p", 3840, 2160), 736, 1300), 3);
        // a target no factor can reach still gets the biggest one
        assert_eq!(auto_scale(Target::Preset("2160p", 3840, 2160), 200, 200), 4);
        // already huge: still upscales, but never past the 8192px guard
        assert_eq!(auto_scale(Target::Auto, 6000, 4000), 2);
    }

    #[test]
    fn fit_size_only_shrinks_and_stays_even() {
        assert_eq!(fit_size(Target::Auto, 1920, 1080), None);
        assert_eq!(fit_size(Target::Auto, 2944, 5200), Some((1080, 1908)));
        assert_eq!(fit_size(Target::Preset("720p", 1280, 720), 3840, 2160), Some((1280, 720)));
        let (w, h) = fit_size(Target::Auto, 4001, 2001).unwrap();
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
        assert!(w <= 1920 && h <= 1080);
    }
}
