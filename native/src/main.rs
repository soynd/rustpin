//! rustpin — browse Pinterest, download originals, auto-upscale with Upscayl.
mod jobs;
mod pinterest;
mod settings;
mod upscayl;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;

use jobs::{Engine, Job};
use pinterest::{Client, Pin};
use settings::Target;

const GRID_GAP: f32 = 10.0;
const GRID_CELL_WANT: f32 = 200.0;
const SIM_GAP: f32 = 8.0;
const SIM_CELL_WANT: f32 = 104.0;
const MAX_INFLIGHT: usize = 16;

// ---------- data ----------

struct ActiveDl {
    job_id: String,
    title: String,
    start: Instant,
    hidden: bool,
    announced: bool,
}

enum Msg {
    Status(String),
    Session(Arc<Client>),
    Ready(Result<(), String>),
    Pins {
        pins: Vec<Pin>,
        bookmarks: Vec<String>,
    },
    Similar {
        pin_id: String,
        pins: Vec<Pin>,
        error: String,
    },
    Img {
        key: String,
        img: egui::ColorImage,
    },
    ImgFail {
        key: String,
    },
    Icon {
        key: String,
        img: Option<egui::ColorImage>,
    },
}

// ---------- gap-free masonry layout ----------

/// Column count that makes the columns fill `width` exactly.
fn columns_for(width: f32, cell_want: f32, gap: f32, max: usize) -> usize {
    let n = ((width + gap) / (cell_want + gap)).floor() as usize;
    n.clamp(1, max)
}

/// Shortest-column masonry: content height plus one rect per item, in item
/// order. Columns are exactly `cell` wide and tiles are exactly as tall as
/// their aspect ratio needs, so the grid never leaves a gap behind — whatever
/// size the window (or the panel) happens to be.
fn masonry(aspects: &[f32], width: f32, cols: usize, gap: f32) -> (f32, Vec<egui::Rect>) {
    let cols = cols.max(1);
    let cell = ((width - gap * (cols as f32 - 1.0)) / cols as f32).max(40.0);
    let mut heights = vec![0.0f32; cols];
    let mut rects = Vec::with_capacity(aspects.len());
    for a in aspects {
        let c = heights
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i)
            .unwrap_or(0);
        let a = if a.is_finite() && *a > 0.05 { *a } else { 1.0 };
        let h = (cell / a).clamp(cell * 0.55, cell * 2.6);
        rects.push(egui::Rect::from_min_size(
            egui::pos2(c as f32 * (cell + gap), heights[c]),
            egui::vec2(cell, h),
        ));
        heights[c] += h + gap;
    }
    let height = heights.iter().copied().fold(0.0f32, f32::max) + 14.0;
    (height, rects)
}

fn decode_bytes(bytes: &[u8]) -> Result<egui::ColorImage, String> {
    let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    Ok(egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba))
}

fn fetch_color_image(url: &str) -> Result<egui::ColorImage, String> {
    use std::io::Read as _;
    let resp = ureq::get(url)
        .timeout(Duration::from_secs(30))
        .call()
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    decode_bytes(&bytes)
}

fn open_with(target: &str) {
    let _ = std::process::Command::new("xdg-open").arg(target).spawn();
}

/// Edge length of [`close_button`], also what a header row reserves for it.
const CLOSE_SIZE: f32 = 18.0;

/// A close button drawn from two white strokes instead of a font glyph, so it
/// always looks like a real ✕ and never like a missing-character box.
fn close_button(ui: &mut egui::Ui, tip: &str) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(CLOSE_SIZE, CLOSE_SIZE), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let hovered = resp.hovered();
        if hovered {
            ui.painter().rect_filled(
                rect,
                CLOSE_SIZE * 0.28,
                egui::Color32::from_white_alpha(28),
            );
        }
        let color = if hovered {
            egui::Color32::WHITE
        } else {
            egui::Color32::from_white_alpha(215)
        };
        let stroke = egui::Stroke::new(1.7_f32, color);
        let c = rect.center();
        let r = CLOSE_SIZE * 0.27;
        let p = ui.painter();
        p.line_segment([c - egui::vec2(r, r), c + egui::vec2(r, r)], stroke);
        p.line_segment([c - egui::vec2(r, -r), c + egui::vec2(r, -r)], stroke);
    }
    resp.on_hover_text(tip)
}

/// Width a [`egui::Ui::small_button`] with this text needs, measured from the
/// font instead of guessed at.
fn small_button_width(ui: &egui::Ui, text: &str) -> f32 {
    let text_w = ui.fonts(|f| {
        f.layout_no_wrap(
            text.to_string(),
            egui::TextStyle::Body.resolve(ui.style()),
            egui::Color32::WHITE,
        )
        .size()
        .x
    });
    (text_w + ui.spacing().button_padding.x * 2.0).max(40.0)
}

/// A window header: the name on the left, buttons on the right. The name only
/// gets the width the buttons leave over, so a long title ends in `…` instead of
/// sliding underneath them. `buttons_w` is the total width of the button strip,
/// gaps included.
fn header_row(
    ui: &mut egui::Ui,
    text: egui::RichText,
    buttons_w: f32,
    add_buttons: impl FnOnce(&mut egui::Ui),
) {
    let gap = ui.spacing().item_spacing.x;
    let name_w = (ui.available_width() - buttons_w - gap).max(40.0);
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(name_w, ui.spacing().interact_size.y),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.add(egui::Label::new(text).truncate());
            },
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add_buttons);
    });
}

/// The logo, pulled straight from the project's GitHub so that no image file
/// has to be shipped in the source tree. Only used when nothing is installed
/// locally.
const ICON_URL: &str = "https://raw.githubusercontent.com/soynd/rustpin/main/rustpin.png";

/// An installed `rustpin.png`, then the usual system icon locations.
fn icon_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.push(dir.join("rustpin.png"));
            out.push(dir.join("icons").join("rustpin.png"));
            if let Some(root) = dir.ancestors().nth(2) {
                out.push(root.join("rustpin.png"));
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        out.push(cwd.join("rustpin.png"));
    }
    let home = settings::home();
    out.push(home.join(".local/share/icons/rustpin.png"));
    out.push(home.join(".icons/rustpin.png"));
    out.push(PathBuf::from("/usr/share/icons/hicolor/256x256/apps/rustpin.png"));
    out.push(PathBuf::from("/usr/share/icons/hicolor/128x128/apps/rustpin.png"));
    out.push(PathBuf::from("/usr/share/pixmaps/rustpin.png"));
    out
}

// ---------- app ----------

struct App {
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    client: Option<Arc<Client>>,
    engine: Arc<Engine>,
    query: String,
    pins: Vec<Pin>,
    bookmarks: Vec<String>,
    seen: HashSet<String>,
    loading: bool,
    textures: HashMap<String, egui::TextureHandle>,
    inflight: HashSet<String>,
    selected: Option<String>,
    similar: Vec<Pin>,
    similar_for: String,
    similar_loading: bool,
    similar_error: String,
    jobs: Vec<Job>,
    active_dl: Option<ActiveDl>,
    last_poll: Instant,
    status: String,
    icon_tex: Option<egui::TextureHandle>,
    icon_tried: bool,
    settings_open: bool,
    typing: bool,
    custom_res: String,
    upscayl_line: String,
    library: String,
}

impl App {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, rx) = mpsc::channel();
        let engine = Arc::new(Engine::new());
        let last_query = engine.settings().last_query;
        settings::ensure_library();
        let library = settings::library_dir().to_string_lossy().to_string();
        let upscayl_line = engine.upscayl_info();

        let tx0 = tx.clone();
        std::thread::Builder::new()
            .name("rustpin-net".into())
            .spawn(move || match Client::new() {
                Ok(client) => {
                    tx0.send(Msg::Session(Arc::new(client))).ok();
                }
                Err(e) => {
                    tx0.send(Msg::Ready(Err(format!(
                        "Pinterest is unreachable: {e}"
                    ))))
                    .ok();
                }
            })
            .ok();

        Self {
            tx,
            rx,
            client: None,
            engine,
            query: last_query,
            pins: vec![],
            bookmarks: vec![],
            seen: HashSet::new(),
            loading: true,
            textures: HashMap::new(),
            inflight: HashSet::new(),
            selected: None,
            similar: vec![],
            similar_for: String::new(),
            similar_loading: false,
            similar_error: String::new(),
            jobs: vec![],
            active_dl: None,
            last_poll: Instant::now(),
            status: "Starting…".into(),
            icon_tex: None,
            icon_tried: false,
            settings_open: false,
            typing: false,
            custom_res: String::new(),
            upscayl_line,
            library,
        }
    }

    fn client(&self) -> Result<Arc<Client>, String> {
        self.client
            .clone()
            .ok_or_else(|| "Pinterest session not ready yet".to_string())
    }

    fn do_search(&mut self, more: bool) {
        if self.loading || (more && self.bookmarks.is_empty()) {
            return;
        }
        let Ok(client) = self.client() else { return };
        self.loading = true;
        self.status = if more {
            "Loading more…".into()
        } else {
            let q = self.query.trim().to_string();
            self.engine.update(|s| s.last_query = q.clone());
            format!("Searching “{q}”…")
        };
        let tx = self.tx.clone();
        let q = self.query.clone();
        let bm = self.bookmarks.clone();
        std::thread::Builder::new()
            .name("rustpin-search".into())
            .spawn(move || match client.search(&q, &bm) {
                Ok((pins, bookmarks)) => {
                    tx.send(Msg::Pins { pins, bookmarks }).ok();
                }
                Err(e) => {
                    tx.send(Msg::Status(format!("Search failed: {e}"))).ok();
                }
            })
            .ok();
    }

    fn load_similar(&mut self, id: &str) {
        self.similar_for = id.to_string();
        self.similar.clear();
        self.similar_error.clear();
        self.similar_loading = true;
        let Ok(client) = self.client() else {
            self.similar_loading = false;
            self.similar_error = "Pinterest session not ready".into();
            return;
        };
        let tx = self.tx.clone();
        let tid = id.to_string();
        std::thread::Builder::new()
            .name("rustpin-similar".into())
            .spawn(move || match client.related(&tid, &[]) {
                Ok((pins, _)) => {
                    tx.send(Msg::Similar {
                        pin_id: tid,
                        pins,
                        error: String::new(),
                    })
                    .ok();
                }
                Err(e) => {
                    tx.send(Msg::Similar {
                        pin_id: tid,
                        pins: vec![],
                        error: e,
                    })
                    .ok();
                }
            })
            .ok();
    }

    fn request_img(&mut self, key: String, url: String) {
        if key.is_empty()
            || url.is_empty()
            || self.textures.contains_key(&key)
            || self.inflight.contains(&key)
            || self.inflight.len() >= MAX_INFLIGHT
        {
            return;
        }
        self.inflight.insert(key.clone());
        let tx = self.tx.clone();
        std::thread::spawn(move || match fetch_color_image(&url) {
            Ok(img) => {
                tx.send(Msg::Img { key, img }).ok();
            }
            Err(_) => {
                tx.send(Msg::ImgFail { key }).ok();
            }
        });
    }

    fn request_local_img(&mut self, key: String, path: String) {
        if self.textures.contains_key(&key) || self.inflight.contains(&key) {
            return;
        }
        self.inflight.insert(key.clone());
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let got = std::fs::read(&path)
                .ok()
                .and_then(|b| decode_bytes(&b).ok());
            match got {
                Some(img) => {
                    tx.send(Msg::Img { key, img }).ok();
                }
                None => {
                    tx.send(Msg::ImgFail { key }).ok();
                }
            }
        });
    }

    fn enqueue(&mut self, upscale: bool) {
        let Some(id) = self.selected.clone() else { return };
        let Some(pin) = self.pins.iter().find(|p| p.id == id).cloned() else {
            return;
        };
        if upscale && !self.engine.has_upscayl() {
            self.status = "Upscayl not found — install it first.".into();
            return;
        }
        if self.engine.pending() > 0 {
            self.status = "Still working on the last one — queued.".into();
        }
        let job_id = self.engine.enqueue(&pin, upscale);
        self.active_dl = Some(ActiveDl {
            job_id,
            title: pin.title.clone(),
            start: Instant::now(),
            hidden: false,
            announced: false,
        });
        self.status = "Saving…".into();
    }

    fn pump(&mut self, ctx: &egui::Context) {
        let mut repaint = false;
        let msgs: Vec<Msg> = self.rx.try_iter().collect();
        for msg in msgs {
            match msg {
                Msg::Status(s) => {
                    self.status = s;
                    self.loading = false;
                }
                Msg::Session(client) => {
                    self.client = Some(client);
                    // no search of its own: either the one from last time, or
                    // an empty grid waiting for the user
                    if !self.query.trim().is_empty() && self.pins.is_empty() {
                        self.loading = false;
                        self.do_search(false);
                    } else {
                        self.loading = false;
                        if self.query.trim().is_empty() {
                            self.status = "Type something and hit Search.".into();
                        }
                    }
                }
                Msg::Ready(Err(e)) => {
                    self.loading = false;
                    self.status = e;
                }
                Msg::Ready(Ok(())) => {
                    self.loading = false;
                }
                Msg::Pins { pins, bookmarks } => {
                    self.loading = false;
                    self.bookmarks = bookmarks;
                    let mut added = 0;
                    for p in pins {
                        if self.seen.insert(p.id.clone()) {
                            self.pins.push(p);
                            added += 1;
                        }
                    }
                    let more = !self.bookmarks.is_empty();
                    self.status = format!(
                        "{} wallpapers (+{added} new){}",
                        self.pins.len(),
                        if more { " · scroll for more" } else { " · end" }
                    );
                    repaint = true;
                }
                Msg::Similar {
                    pin_id,
                    pins,
                    error,
                } => {
                    if self.selected.as_deref() == Some(pin_id.as_str()) {
                        self.similar = pins;
                        self.similar_error = error;
                        self.similar_loading = false;
                        repaint = true;
                    }
                }
                Msg::Img { key, img } => {
                    self.inflight.remove(&key);
                    self.textures.insert(
                        key.clone(),
                        ctx.load_texture(key, img, egui::TextureOptions::LINEAR),
                    );
                    repaint = true;
                }
                Msg::ImgFail { key } => {
                    self.inflight.remove(&key);
                }
                Msg::Icon { key, img } => {
                    self.inflight.remove(&key);
                    self.icon_tried = true;
                    if let Some(img) = img {
                        self.icon_tex =
                            Some(ctx.load_texture(key, img, egui::TextureOptions::LINEAR));
                    }
                    repaint = true;
                }
            }
        }
        if repaint {
            ctx.request_repaint();
        }
    }

    fn poll_jobs(&mut self) {
        if self.active_dl.is_some() && self.last_poll.elapsed() >= Duration::from_millis(400) {
            self.last_poll = Instant::now();
            self.jobs = self.engine.jobs();
            let done = self
                .active_dl
                .as_ref()
                .filter(|a| !a.announced)
                .and_then(|a| self.jobs.iter().find(|j| j.id == a.job_id))
                .filter(|j| j.status == "done")
                .map(|j| j.output_path.clone());
            if let Some(name) = done {
                if let Some(a) = self.active_dl.as_mut() {
                    a.announced = true;
                }
                self.status = format!("Saved → {}/{}", self.library, name);
            }
        }
        if self.upscayl_line.is_empty() || self.upscayl_line.contains("not found") {
            let line = self.engine.upscayl_info();
            if line != self.upscayl_line {
                self.upscayl_line = line;
            }
        }
    }

    fn load_app_icon(&mut self) {
        let key = "app-icon".to_string();
        if self.icon_tried || self.inflight.contains(&key) {
            return;
        }
        self.inflight.insert(key.clone());
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let img = icon_candidates()
                .into_iter()
                .find_map(|p| std::fs::read(&p).ok().and_then(|b| decode_bytes(&b).ok()))
                .or_else(|| fetch_color_image(ICON_URL).ok());
            tx.send(Msg::Icon { key, img }).ok();
        });
    }

    fn active_job(&self) -> Option<Job> {
        let id = self.active_dl.as_ref()?.job_id.as_str();
        self.jobs.iter().find(|j| j.id == id).cloned()
    }

    fn overlay_visible(&self) -> bool {
        matches!(&self.active_dl, Some(a) if !a.hidden)
    }

    /// Aspect ratio to reserve space with: a loaded texture wins, so a tile is
    /// always exactly the shape of the picture inside it.
    fn aspect_of(&self, pin: &Pin, key: &str) -> f32 {
        if let Some(tex) = self.textures.get(key) {
            let s = tex.size();
            if s[0] > 0 && s[1] > 0 {
                return s[0] as f32 / s[1] as f32;
            }
        }
        pin.aspect()
    }

    /// Paint one tile at `rect`, request its image if needed, handle clicks.
    fn draw_cell(
        &mut self,
        ui: &egui::Ui,
        pin: &Pin,
        key: &str,
        url: &str,
        rect: egui::Rect,
        radius: f32,
    ) {
        let tex = self.textures.get(key).cloned();
        if tex.is_none() {
            self.request_img(key.to_string(), url.to_string());
        }
        match tex {
            Some(t) => {
                egui::Image::new(&t)
                    .fit_to_exact_size(rect.size())
                    .maintain_aspect_ratio(false)
                    .corner_radius(radius)
                    .paint_at(ui, rect);
            }
            None => {
                ui.painter()
                    .rect_filled(rect, radius, egui::Color32::from_gray(30));
            }
        }
        let resp = ui.interact(rect, egui::Id::new(("cell", key)), egui::Sense::click());
        let selected = self.selected.as_deref() == Some(pin.id.as_str());
        if selected || resp.hovered() {
            let (w, a) = if selected { (2.5_f32, 255_u8) } else { (1.5_f32, 140_u8) };
            ui.painter().rect_stroke(
                rect.expand(1.5),
                radius + 1.0,
                egui::Stroke::new(w, egui::Color32::from_white_alpha(a)),
                egui::StrokeKind::Outside,
            );
        }
        if resp.clicked() {
            self.select_pin(pin.clone());
        }
    }

    /// Open a pin in the detail panel. Similar pins are not in the grid yet,
    /// so they are added to it first — otherwise the panel would look the pin
    /// up, find nothing and close itself.
    fn select_pin(&mut self, pin: Pin) {
        if !self.pins.iter().any(|p| p.id == pin.id) {
            self.seen.insert(pin.id.clone());
            self.pins.push(pin.clone());
        }
        self.selected = Some(pin.id);
    }
}

// ---------- settings ----------

impl App {
    fn show_settings(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let saved = self.engine.settings();
        let mut close = false;
        let mut chosen = saved.target;
        let mut custom = self.custom_res.clone();
        let mut apply_custom = false;

        egui::Window::new("⚙ Settings")
            .collapsible(false)
            .resizable(false)
            .title_bar(false)
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-8.0, 46.0))
            .show(ctx, |ui| {
                ui.set_min_width(320.0);

                // own header: name on the left, ✕ on the right
                header_row(ui, egui::RichText::new("⚙ Settings").heading(), CLOSE_SIZE, |ui| {
                    if close_button(ui, "Close settings (Esc)").clicked() {
                        close = true;
                    }
                });
                ui.separator();

                ui.label(egui::RichText::new("Resolution").strong());
                egui::ComboBox::from_id_salt("target")
                    .selected_text(saved.target.label())
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut chosen, Target::Auto, Target::Auto.label());
                        for (name, w, h) in settings::PRESETS {
                            let t = Target::Preset(name, w, h);
                            ui.selectable_value(&mut chosen, t, t.label());
                        }
                        if let Target::Exact(w, h) = saved.target {
                            let t = Target::Exact(w, h);
                            ui.selectable_value(&mut chosen, t, t.label());
                        }
                    });
                ui.horizontal(|ui| {
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut custom)
                            .desired_width(140.0)
                            .hint_text("or 2560x1440"),
                    );
                    self.typing |= resp.has_focus();
                    apply_custom = resp.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.small(egui::RichText::new("custom W×H + Enter").weak());
                });
                ui.small(
                    egui::RichText::new(format!(
                        "upscayl runs at {}× and the picture is then fitted inside {}×{} \
                         — aspect ratio kept, nothing cropped.",
                        settings::auto_scale(saved.target, 736, 1300),
                        saved.target.box_for(16, 9).0,
                        saved.target.box_for(16, 9).1
                    ))
                    .weak(),
                );

                ui.add_space(4.0);
                ui.separator();
                let mut delete_orig = saved.delete_orig;
                if ui
                    .checkbox(&mut delete_orig, "Delete original after upscaling")
                    .changed()
                {
                    self.engine.update(|s| s.delete_orig = delete_orig);
                    self.status = "Saved automatically.".into();
                }

                ui.add_space(4.0);
                ui.separator();
                ui.label(egui::RichText::new(&self.upscayl_line).small());
                ui.small(
                    egui::RichText::new("Upscaler: upscayl-standard-4x · settings save themselves")
                        .weak(),
                );
                ui.add_space(4.0);
                ui.small(egui::RichText::new("Esc closes this").weak());
            });

        if apply_custom {
            match Target::parse(&custom) {
                Some(t) => {
                    chosen = t;
                    self.status = "Saved automatically.".into();
                }
                None => self.status = format!("Not a resolution: {custom}"),
            }
        }
        self.custom_res = custom;
        if chosen != saved.target {
            if let Target::Exact(w, h) = chosen {
                self.custom_res = format!("{w}x{h}");
            }
            self.engine.update(|s| s.target = chosen);
            self.status = "Saved automatically.".into();
        }
        if close {
            self.settings_open = false;
        }
    }
}

// ---------- panels ----------

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump(ctx);
        self.poll_jobs();
        self.load_app_icon();
        self.typing = false;
        self.show_settings(ctx);
        ctx.request_repaint_after(Duration::from_millis(
            if self.overlay_visible() { 400 } else { 1500 },
        ));

        // ---- top bar ----
        egui::TopBottomPanel::top("bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                match self.icon_tex.clone() {
                    Some(icon) => {
                        ui.add(egui::Image::new(&icon).max_height(24.0));
                    }
                    None => {
                        ui.label("📌");
                    }
                }
                ui.heading("rustpin");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .hint_text("Search wallpapers…")
                        .desired_width(230.0),
                );
                self.typing |= resp.has_focus();
                let go = ui.button("🔍 Search").clicked()
                    || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                if go {
                    self.pins.clear();
                    self.seen.clear();
                    self.bookmarks.clear();
                    self.textures.clear();
                    self.selected = None;
                    self.similar.clear();
                    self.similar_for.clear();
                    self.loading = false;
                    self.do_search(false);
                }
                if ui.button("📁 Folder").clicked() {
                    open_with(&self.library);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⚙ Settings").clicked() {
                        self.settings_open = !self.settings_open;
                    }
                });
            });
        });

        // ---- status bar ----
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some(active) = &self.active_dl {
                    let (stage, title) = match self.active_job() {
                        Some(j) => (j.status.clone(), j.title.clone()),
                        None => ("starting".to_string(), active.title.clone()),
                    };
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        format!("⏳ {stage} — {title}"),
                    );
                    if active.hidden {
                        if ui.button("Show ⤢").clicked() {
                            if let Some(a) = self.active_dl.as_mut() {
                                a.hidden = false;
                            }
                        }
                    } else if ui.button("Hide").clicked() {
                        if let Some(a) = self.active_dl.as_mut() {
                            a.hidden = true;
                        }
                    }
                } else {
                    ui.label(format!(
                        "{}  •  {} wallpapers",
                        self.status,
                        self.pins.len()
                    ));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let pending = self.engine.pending();
                    if pending > 0 {
                        ui.colored_label(
                            egui::Color32::YELLOW,
                            format!("{pending} in queue"),
                        );
                    }
                    ui.small(egui::RichText::new(&self.upscayl_line).weak());
                });
            });
        });

        // ---- detail panel ----
        if let Some(id) = self.selected.clone() {
            if let Some(pin) = self.pins.iter().find(|p| p.id == id).cloned() {
                egui::SidePanel::right("detail")
                    .resizable(true)
                    .default_width(340.0)
                    .show(ctx, |ui| self.show_detail(ui, &pin));
            }
        }

        // ---- grid ----
        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(&ctx.style()).inner_margin(10.0))
            .show(ctx, |ui| self.show_grid(ui));

        if self.overlay_visible() {
            self.show_overlay(ctx);
        }

        // Esc backs out of whatever is open on top: the settings popup first,
        // then the wallpaper being looked at. Not while typing (Esc in the
        // search box must not throw the open wallpaper away) and not while a
        // dropdown is open — that one closes itself on Esc.
        let dropdown_open = ctx.memory(|m| m.any_popup_open());
        if !self.typing
            && !dropdown_open
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            if self.settings_open {
                self.settings_open = false;
            } else if self.selected.is_some() {
                self.selected = None;
            }
        }
    }
}

impl App {
    fn show_grid(&mut self, ui: &mut egui::Ui) {
        if self.pins.is_empty() {
            if self.loading {
                ui.centered_and_justified(|ui| {
                    ui.spinner();
                    ui.label("Loading Pinterest…");
                });
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("Search something above to browse wallpapers.");
                });
            }
            return;
        }

        let width = ui.available_width();
        let cols = columns_for(width, GRID_CELL_WANT, GRID_GAP, 10);
        let aspects: Vec<f32> = self.pins.iter().map(|p| self.aspect_of(p, &p.id)).collect();
        let (content_h, rects) = masonry(&aspects, width, cols, GRID_GAP);

        let out = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let clip = ui.clip_rect().expand(GRID_GAP * 3.0);
                let (area, _) =
                    ui.allocate_exact_size(egui::vec2(width, content_h), egui::Sense::hover());
                let pins = std::mem::take(&mut self.pins);
                for (i, r) in rects.iter().enumerate() {
                    let r = r.translate(area.min.to_vec2());
                    if !clip.intersects(r) {
                        continue;
                    }
                    let p = &pins[i];
                    let key = p.id.clone();
                    let url = p.thumb.clone();
                    self.draw_cell(ui, p, &key, &url, r, 12.0);
                }
                self.pins = pins;
            });

        let remaining = out.content_size.y - (out.state.offset.y + out.inner_rect.height());
        if !self.loading
            && !self.bookmarks.is_empty()
            && !self.pins.is_empty()
            && remaining < 900.0
        {
            self.do_search(true);
        }
    }

    fn show_detail(&mut self, ui: &mut egui::Ui, pin: &Pin) {
        if self.similar_for != pin.id {
            self.load_similar(&pin.id);
        }
        let target = self.engine.target();
        let scale = settings::auto_scale(target, pin.w, pin.h);
        let (bw, bh) = target.box_for(pin.w.max(1), pin.h.max(1));

        // header row pinned above the scroll area: wallpaper name on the left,
        // Pinterest + close on the right
        let page_w = small_button_width(ui, "↗ Pinterest");
        header_row(
            ui,
            egui::RichText::new(&pin.title).strong().size(16.0),
            CLOSE_SIZE + ui.spacing().item_spacing.x + page_w,
            |ui| {
                let close = close_button(ui, "Close (Esc)");
                let page_url = pin.page_url.clone();
                if ui.small_button("↗ Pinterest").clicked() {
                    open_with(&page_url);
                }
                if close.clicked() {
                    self.selected = None;
                }
            },
        );
        ui.separator();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let width = ui.available_width();
                let key = format!("{}#p", pin.id);
                let aspect = self.aspect_of(pin, &key);
                let height = (width / aspect.clamp(0.2, 5.0)).clamp(120.0, width * 2.6);
                let rect = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(width, height));
                ui.allocate_space(egui::vec2(width, height));
                let url = pin.preview.clone();
                self.draw_cell(ui, pin, &key, &url, rect, 12.0);

                ui.add_space(6.0);
                ui.small(
                    egui::RichText::new(format!(
                        "upscayl-standard-4x at {scale}× → fits {bw}×{bh}"
                    ))
                    .weak(),
                );

                ui.add_space(6.0);
                let button_w = ui.available_width();
                if ui
                    .add_sized(
                        [button_w, 34.0],
                        egui::Button::new("⬇ Download + Upscale"),
                    )
                    .clicked()
                {
                    self.enqueue(true);
                }
                if ui
                    .add_sized(
                        [button_w, 26.0],
                        egui::Button::new("Original only (no upscale)"),
                    )
                    .clicked()
                {
                    self.enqueue(false);
                }

                ui.separator();
                ui.heading("More like this ✨");
                if self.similar_loading && self.similar.is_empty() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Finding similar…");
                    });
                } else if !self.similar_error.is_empty() && self.similar.is_empty() {
                    ui.label(format!("Similar failed: {}", self.similar_error));
                } else if self.similar.is_empty() {
                    ui.label("No similar pins found.");
                } else {
                    let sims = std::mem::take(&mut self.similar);
                    let width = ui.available_width();
                    let cols = columns_for(width, SIM_CELL_WANT, SIM_GAP, 6);
                    let aspects: Vec<f32> =
                        sims.iter().map(|p| self.aspect_of(p, &p.id)).collect();
                    let (content_h, rects) = masonry(&aspects, width, cols, SIM_GAP);
                    let (area, _) = ui.allocate_exact_size(
                        egui::vec2(width, content_h),
                        egui::Sense::hover(),
                    );
                    for (i, r) in rects.iter().enumerate() {
                        let r = r.translate(area.min.to_vec2());
                        let p = sims[i].clone();
                        let key = p.id.clone();
                        let url = p.thumb.clone();
                        self.draw_cell(ui, &p, &key, &url, r, 10.0);
                    }
                    self.similar = sims;
                }
            });
    }

    fn show_overlay(&mut self, ctx: &egui::Context) {
        let rect = ctx.screen_rect();
        egui::Area::new(egui::Id::new("dl_overlay"))
            .fixed_pos(rect.min)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.painter()
                    .rect_filled(rect, 0.0, egui::Color32::from_black_alpha(235));
                ui.allocate_new_ui(egui::UiBuilder::new().max_rect(rect), |ui| {
                    ui.add_space(rect.height() * 0.15);
                    ui.vertical_centered(|ui| {
                        ui.set_max_width(460.0);
                        let (title, elapsed) = self
                            .active_dl
                            .as_ref()
                            .map(|a| (a.title.clone(), a.start.elapsed()))
                            .unwrap_or_default();
                        ui.heading("⬇ Getting your wallpaper");
                        ui.label(egui::RichText::new(title).small());
                        ui.add_space(10.0);
                        let job = self.active_job();
                        let stage = job.as_ref().map(|j| j.status.clone()).unwrap_or_default();
                        match stage.as_str() {
                            "" => {
                                ui.spinner();
                                ui.label("Starting…");
                            }
                            "done" => {
                                let j = job.as_ref().unwrap();
                                ui.colored_label(egui::Color32::GREEN, "✅ Saved");
                                let full = jobs::library_file(&j.output_path);
                                if !j.output_path.is_empty() {
                                    let key = format!("result:{}", j.id);
                                    self.request_local_img(
                                        key.clone(),
                                        full.to_string_lossy().to_string(),
                                    );
                                    match self.textures.get(&key).cloned() {
                                        Some(t) => {
                                            ui.add(
                                                egui::Image::new(&t)
                                                    .max_size(egui::vec2(420.0, 280.0)),
                                            );
                                        }
                                        None => {
                                            ui.spinner();
                                        }
                                    }
                                    ui.small(egui::RichText::new(&j.output_path).weak());
                                }
                                ui.add_space(8.0);
                                if ui.button("📂 Open file").clicked() {
                                    open_with(&full.to_string_lossy());
                                }
                                if ui.button("📁 Open folder").clicked() {
                                    open_with(&self.library);
                                }
                                ui.add_space(8.0);
                                if ui.button("✕ Close").clicked() {
                                    self.active_dl = None;
                                }
                            }
                            "error" => {
                                let j = job.as_ref().unwrap();
                                ui.colored_label(egui::Color32::RED, "❌ Failed");
                                ui.label(j.error.chars().take(300).collect::<String>());
                                if !j.orig_path.is_empty() {
                                    ui.colored_label(
                                        egui::Color32::YELLOW,
                                        format!("original kept: {}/{}", self.library, j.orig_path),
                                    );
                                }
                                if ui.button("✕ Close").clicked() {
                                    self.active_dl = None;
                                }
                            }
                            _ => {
                                let j = job.as_ref().unwrap();
                                if stage == "downloading" && j.total > 0 {
                                    ui.add(
                                        egui::ProgressBar::new(j.progress.clamp(0.0, 1.0))
                                            .show_percentage()
                                            .desired_width(380.0),
                                    );
                                    ui.label(format!(
                                        "Downloading original… {} KB / {} KB",
                                        j.downloaded / 1024,
                                        j.total / 1024
                                    ));
                                } else {
                                    ui.spinner();
                                    let secs = elapsed.as_secs();
                                    let text = match stage.as_str() {
                                        "downloading" => "Downloading original…".to_string(),
                                        "saving" => "Saving your wallpaper…".to_string(),
                                        _ => format!(
                                            "Upscaling with {} at {}x… {:02}:{:02}",
                                            j.model,
                                            j.scale,
                                            secs / 60,
                                            secs % 60
                                        ),
                                    };
                                    ui.label(text);
                                }
                                ui.add_space(8.0);
                                if ui.button("Hide (keeps running)").clicked() {
                                    if let Some(a) = self.active_dl.as_mut() {
                                        a.hidden = true;
                                    }
                                }
                            }
                        }
                    });
                });
            });
    }
}

fn main() -> eframe::Result<()> {
    // RUSTPIN_SIZE=1600x900 and RUSTPIN_FULLSCREEN=1 are handy for checking
    // the layout at other window sizes.
    let (mut w, mut h) = (1180.0_f32, 800.0_f32);
    if let Some(spec) = std::env::var("RUSTPIN_SIZE").ok() {
        if let Some((a, b)) = spec.split_once(['x', 'X']) {
            if let (Ok(a), Ok(b)) = (a.trim().parse(), b.trim().parse()) {
                w = a;
                h = b;
            }
        }
    }
    let fullscreen = std::env::var("RUSTPIN_FULLSCREEN").is_ok_and(|v| v != "0");
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([w, h])
            .with_app_id("rustpin")
            .with_fullscreen(fullscreen),
        ..Default::default()
    };
    eframe::run_native(
        "rustpin",
        opts,
        Box::new(|cc| Ok(Box::new(App::new(cc)) as Box<dyn eframe::App>)),
    )
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Tiles fill their column edge to edge: every tile sits exactly on the
    /// column grid, so no horizontal gap can open up whatever the width is.
    #[test]
    fn masonry_tiles_the_width_without_gaps() {
        for width in [180.0, 317.0, 640.0, 1013.0, 1180.0, 1920.0, 2560.0] {
            for want in [GRID_CELL_WANT, SIM_CELL_WANT] {
                let cols = columns_for(width, want, GRID_GAP, 10);
                let cell = (width - GRID_GAP * (cols as f32 - 1.0)) / cols as f32;
                let aspects: Vec<f32> = (0..60).map(|i| 0.6 + (i % 7) as f32 * 0.22).collect();
                let (height, rects) = masonry(&aspects, width, cols, GRID_GAP);
                assert_eq!(rects.len(), aspects.len());
                assert!((cols as f32 * cell + GRID_GAP * (cols as f32 - 1.0) - width).abs() < 0.01);
                let mut used = vec![0usize; cols];
                for (i, r) in rects.iter().enumerate() {
                    let c = ((r.min.x / (cell + GRID_GAP)).round() as usize).min(cols - 1);
                    used[c] += 1;
                    assert!((r.min.x - c as f32 * (cell + GRID_GAP)).abs() < 0.01);
                    assert!((r.width() - cell).abs() < 0.01, "tile is not cell-wide");
                    assert!(r.max.x <= width + 0.01, "tile sticks out of the panel");
                    assert!(r.max.y <= height + 0.01, "tile below the content box");
                    if r.height() > cell * 0.56 {
                        assert!((r.height() - cell / aspects[i]).abs() < 0.01);
                    }
                }
                assert!(used.iter().all(|n| *n > 0), "an empty column leaves a gap");
            }
        }
    }

    /// Tiles in the same column never overlap, so the shortest-column packing
    /// cannot stack two pictures on top of each other.
    #[test]
    fn masonry_columns_do_not_overlap() {
        let aspects: Vec<f32> = (0..120)
            .map(|i| if i % 3 == 0 { 0.5 } else { 1.8 })
            .collect();
        let (height, rects) = masonry(&aspects, 900.0, 4, GRID_GAP);
        let cell = (900.0 - GRID_GAP * 3.0) / 4.0;
        let mut cols: Vec<Vec<egui::Rect>> = vec![Vec::new(); 4];
        for r in &rects {
            let c = ((r.min.x / (cell + GRID_GAP)).round() as usize).min(3);
            cols[c].push(*r);
        }
        for col in &cols {
            assert!(!col.is_empty());
            let mut sorted = col.clone();
            sorted.sort_by(|a, b| a.min.y.total_cmp(&b.min.y));
            for pair in sorted.windows(2) {
                assert!(
                    pair[1].min.y >= pair[0].max.y - 0.01,
                    "tiles overlap inside a column"
                );
            }
        }
        assert!(height > 0.0);
    }

    #[test]
    fn one_column_when_there_is_no_room() {
        assert_eq!(columns_for(90.0, GRID_CELL_WANT, GRID_GAP, 10), 1);
        assert_eq!(columns_for(1900.0, GRID_CELL_WANT, GRID_GAP, 10), 9);
    }
}
