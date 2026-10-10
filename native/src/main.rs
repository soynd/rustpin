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
use settings::{Saved, Target};

const GRID_GAP: f32 = 10.0;
const GRID_CELL_WANT: f32 = 200.0;
const SIM_GAP: f32 = 8.0;
const SIM_CELL_WANT: f32 = 104.0;
/// How tall the "More like this" list is when the panel is too short to
/// divide the height up; on a taller one it grows into the space left over.
const SIM_SECTION_H: f32 = 420.0;
/// Roughly what the heading, its separator and the gaps around them take off
/// the bottom of the panel before the list starts.
const SIM_HEADING_H: f32 = 44.0;
const MAX_INFLIGHT: usize = 48;
/// Points a wheel notch moves the content by. 100 is what Chrome and Firefox
/// use, and egui's own 40 is what made this feel so much slower than a browser.
const BROWSER_NOTCH: f32 = 100.0;
/// `RUSTPIN_NOTCH=160` moves a notch that far instead, for anyone whose mouse
/// or taste wants more or less. Read once, so it is a cheap lookup.
static NOTCH: std::sync::LazyLock<f32> = std::sync::LazyLock::new(|| {
    std::env::var("RUSTPIN_NOTCH")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(BROWSER_NOTCH)
});
/// What egui calls a notch (points), see `egui::Options::line_scroll_speed`.
const EGUI_NOTCH: f32 = 40.0;
/// Below this the wheel is not a notch but a piece of a trackpad gesture, which
/// is passed through unscaled so fine scrolling stays fine.
const FINE_WHEEL: f32 = 8.0;
/// How long a freshly downloaded picture takes to fade into its cube.
const FADE: f32 = 0.18;
/// How many of the remembered downloads are asked on Pinterest for more like
/// them.
const SUGGEST_SEEDS: usize = 3;
/// Longest edge, in pixels, that a downloaded picture is kept at. Pinterest
/// serves previews and originals far larger than any tile here needs, and
/// shrinking once on the worker thread is much cheaper than uploading (and
/// keeping in VRAM) a 2000px texture for a 200px square.
const MAX_IMAGE_EDGE: u32 = 1024;

// ---------- data ----------

#[derive(Clone)]
enum Msg {
    Status(String),
    /// The session is up, or it never came. One variant, because the success
    /// case used to also carry a `Ready(Ok)` that nothing ever sent.
    Boot(Result<Arc<Client>, String>),
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
    /// Pictures worth showing because they go with what has been downloaded.
    Suggested {
        pins: Vec<Pin>,
    },
    Icon {
        key: String,
        img: Option<egui::ColorImage>,
    },
}

/// Start a worker thread. The `bool` is false when the OS refused, which the
/// callers have to react to: without a thread there is no message coming, so
/// any spinner or in-flight marker would hang forever.
fn spawn_worker(name: &str, f: impl FnOnce() + Send + 'static) -> bool {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .is_ok()
}

// ---------- smooth scrolling ----------

/// A notch moves the content by one notch, right now, and then it is over:
/// no velocity to coast on, so what you see is where the wheel left it.
/// egui 0.31 only coasts after a touch drag, and it has no `smooth_scroll`
/// (that arrived in 0.32), so the notch is taken here and handed to whichever
/// area the pointer is actually over.
///
/// The area's own offset is read back after `show` so scrollbar drags and touch
/// drags keep working: they land in the state, and the read-back is what moves
/// them in here.
struct SmoothScroll {
    /// How far the content is drawn from the top.
    offset: f32,
    /// How far the content can move, kept from the previous frame's layout.
    max_offset: f32,
    /// The part of the screen this one owns, so a notch only reaches the area
    /// actually under the pointer.
    viewport: egui::Rect,
}

impl SmoothScroll {
    fn new() -> Self {
        Self {
            offset: 0.0,
            max_offset: 0.0,
            viewport: egui::Rect::NOTHING,
        }
    }

    /// `wheel` is the notch this area was given, or `None` to take it only when
    /// the pointer is over this area. Both are needed: siblings share the
    /// screen, and a nested area has to yield to its parent at the ends.
    fn step(&mut self, ui: &egui::Ui, wheel: Option<f32>) {
        let dt = ui.input(|i| i.stable_dt).min(0.05);
        let wheel = match wheel {
            Some(w) => w,
            None if self.hovered(ui) => ui.input(|i| i.smooth_scroll_delta.y),
            None => 0.0,
        };
        self.advance(dt, wheel);
    }

    fn hovered(&self, ui: &egui::Ui) -> bool {
        ui.input(|i| i.pointer.hover_pos())
            .is_some_and(|p| self.viewport.contains(p))
    }

    /// Whether there is anywhere left to go in the direction `wheel` points.
    /// A nested area uses this to hand the notch back to its parent once it
    /// has reached the end, which is what makes the two chain into one scroll.
    fn can_scroll(&self, wheel: f32) -> bool {
        if wheel > 0.0 {
            self.offset > 0.5
        } else if wheel < 0.0 {
            self.offset < self.max_offset - 0.5
        } else {
            false
        }
    }

    /// One wheel event: move the content by what it is worth and stop.
    ///
    /// egui already turns the wheel into points, but a plain wheel notch is
    /// only 40 of them (`Options::line_scroll_speed`), which is less than half
    /// of what a browser moves per notch and is why scrolling here felt like
    /// dragging through treacle. A notch is therefore scaled up to
    /// [`BROWSER_NOTCH`] points. High-resolution wheels and trackpads send the
    /// same gesture as a stream of small pieces, and those are left at 1:1 so a
    /// touchpad still glides instead of lurching a whole notch at a time.
    fn advance(&mut self, dt: f32, wheel: f32) {
        let _ = dt;
        if wheel != 0.0 {
            let gain = if wheel.abs() < FINE_WHEEL {
                1.0
            } else {
                *NOTCH / EGUI_NOTCH
            };
            log::trace!("wheel {wheel} x{gain}");
            self.offset -= wheel * gain;
        }
        self.offset = self.offset.clamp(0.0, self.max_offset.max(0.0));
    }

    /// Pick up what the `ScrollArea` did on its own (drag, scrollbar, its own
    /// clamping) and re-learn how far the content can move. `offset` and
    /// `max_offset` come out of the area after `show`.
    fn sync(&mut self, offset: f32, max_offset: f32, viewport: egui::Rect) {
        self.offset = offset;
        self.max_offset = max_offset.max(0.0);
        self.viewport = viewport;
    }

    fn reset(&mut self) {
        self.offset = 0.0;
    }
}

// ---------- gap-free masonry layout ----------

/// Column count that makes the columns fill `width` exactly.
fn columns_for(width: f32, cell_want: f32, gap: f32, max: usize) -> usize {
    let n = ((width + gap) / (cell_want + gap)).floor() as usize;
    n.clamp(1, max)
}

/// How tall the "More like this" list should be.
///
/// A fixed height left a hand's width of blank panel under it on a tall
/// screen, so it takes whatever is left of the panel below everything above
/// it instead \\u{2014} measured in content coordinates, which do not move as the
/// panel scrolls, so the list does not resize halfway through a scroll. On a
/// short window there is not enough to go round and it falls back to
/// [`SIM_SECTION_H`], where the panel scrolls instead.
fn similar_height(list_top: f32, viewport: egui::Rect) -> f32 {
    let room = viewport.height() - (list_top + SIM_HEADING_H);
    if !room.is_finite() || room < SIM_SECTION_H {
        SIM_SECTION_H
    } else {
        room
    }
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

/// Decode a downloaded picture and shrink it to something a tile can use.
///
/// The resize is the whole reason this is on a worker thread: Pinterest hands
/// over previews several times wider than any tile here, and uploading one of
/// those as a texture (and holding it in VRAM for the rest of the session) costs
/// far more than the few milliseconds the shrink takes.
fn decode_bytes(bytes: &[u8]) -> Result<egui::ColorImage, String> {
    let img = image::load_from_memory(bytes)
        .map_err(|e| e.to_string())?
        .into_rgba8();
    let (w, h) = (img.width(), img.height());
    let rgba = if w.max(h) > MAX_IMAGE_EDGE {
        let scale = MAX_IMAGE_EDGE as f32 / w.max(h) as f32;
        let nw = ((w as f32 * scale).round() as u32).max(1);
        let nh = ((h as f32 * scale).round() as u32).max(1);
        let started = Instant::now();
        let small = image::DynamicImage::ImageRgba8(img)
            .resize_exact(nw, nh, image::imageops::FilterType::Triangle)
            .into_rgba8();
        log::debug!(
            "shrunk {w}x{h} to {}x{} in {:?}",
            small.width(),
            small.height(),
            started.elapsed()
        );
        small
    } else {
        img
    };
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    Ok(egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba))
}

/// Thumbnails and icons are small; this only trips on a misdirected response.
const MAX_FETCH: u64 = 64 * 1024 * 1024;

fn fetch_color_image(url: &str) -> Result<egui::ColorImage, String> {
    use std::io::Read as _;
    let resp = ureq::get(url)
        .timeout(Duration::from_secs(30))
        .call()
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .take(MAX_FETCH)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    decode_bytes(&bytes)
}

fn open_with(target: &str) {
    let _ = std::process::Command::new("xdg-open").arg(target).spawn();
}

/// The desktop's own resolution in real pixels, so a 4K panel means a 4K
/// wallpaper even when it is running at a 2x UI scale.
///
/// egui reports the monitor in points and the scale separately, and in 0.31
/// there is no richer monitor API than this to reach for.
fn primary_monitor(ctx: &egui::Context) -> Option<(u32, u32)> {
    let size = ctx.input(|i| i.viewport().monitor_size)?;
    if !size.is_finite() || size.x < 1.0 || size.y < 1.0 {
        return None;
    }
    let scale = ctx.pixels_per_point().max(1.0);
    Some((
        (size.x * scale).round() as u32,
        (size.y * scale).round() as u32,
    ))
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
            ui.painter()
                .rect_filled(rect, CLOSE_SIZE * 0.28, egui::Color32::from_white_alpha(28));
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

/// Pin the button down for this long to arm a destructive action. Long enough
/// that a click cannot set it off, short enough not to feel broken.

/// `upscayl-standard-4x` -> `Upscayl Standard 4x`. Model names are lowercase
/// and hyphenated for the filesystem, which reads badly in a menu.
fn model_label(name: &str) -> String {
    let mut out = String::new();
    for (i, word) in name.split(['-', '_']).filter(|w| !w.is_empty()).enumerate() {
        if i > 0 {
            out.push(' ');
        }
        // Model names are lowercase for the filesystem, but parts of them are
        // not prose: `x4plus`, `4x` and `2x` are the model's own name and stay
        // exactly as written, while the rest gets a capital.
        let lower = word.to_ascii_lowercase();
        let bytes = lower.as_bytes();
        let starts_with_x_digit = bytes.len() > 1
            && bytes[0] == b'x'
            && bytes[1].is_ascii_digit();
        let starts_with_digit = bytes.first().is_some_and(u8::is_ascii_digit);
        if starts_with_x_digit || starts_with_digit {
            out.push_str(&lower);
        } else {
            let mut cs = word.chars();
            if let Some(first) = cs.next() {
                out.extend(first.to_uppercase());
                out.push_str(cs.as_str());
            }
        }
    }
    out
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
        ui.with_layout(
            egui::Layout::right_to_left(egui::Align::Center),
            add_buttons,
        );
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
    out.push(PathBuf::from(
        "/usr/share/icons/hicolor/256x256/apps/rustpin.png",
    ));
    out.push(PathBuf::from(
        "/usr/share/icons/hicolor/128x128/apps/rustpin.png",
    ));
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
    /// Pictures whose url turned out to be dead. Without this a tile with a
    /// broken url asks for it again on every single frame, for as long as it
    /// stays on screen.
    failed: HashSet<String>,
    selected: Option<String>,
    similar: Vec<Pin>,
    similar_for: String,
    similar_loading: bool,
    similar_error: String,
    jobs: Vec<Job>,
    /// Job ids already reported in the status line, so a finished download is
    /// announced exactly once.
    announced: HashSet<String>,
    last_poll: Instant,
    status: String,
    icon_tex: Option<egui::TextureHandle>,
    icon_tried: bool,
    settings_open: bool,
    typing: bool,
    custom_res: String,
    /// Models to offer in settings: what is on disk now, topped up with the
    /// known Upscayl set so the list is never empty on a fresh install.
    models: Vec<String>,
    /// Photos in the library, for the delete button's label. Re-reading the
    /// folder every frame would be one stat per file at 60fps, so it is
    /// counted on a timer instead \u{2014} see [`Self::note_library_count`].
    lib_count: usize,
    lib_counted: Instant,
    upscayl_line: String,
    library: String,
    grid_scroll: SmoothScroll,
    detail_scroll: SmoothScroll,
    similar_scroll: SmoothScroll,
    last_detect: Instant,
    screen_seen: Instant,
    search_at: Option<Instant>,
    /// This frame's wheel notch, taken away from egui before any panel sees it
    /// and handed to whichever scroll area the pointer is actually over.
    wheel: f32,
    /// Whether the jobs window is open. It opens on an empty queue too.
    jobs_popup: bool,
    /// Whether the grid is showing the start-up suggestions rather than the
    /// result of a search, which is what stops "scroll for more" asking for
    /// page two of something that has no pages.
    suggested: bool,
    /// Whether a fresh round of suggestions is on its way.
    suggesting: bool,
    /// When each texture was first drawn, for the fade-in.
    shown_at: HashMap<String, Instant>,
    /// Whether anything on screen is still fading in, so the frames for it
    /// get asked for.
    fading: bool,
}

impl App {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, rx) = mpsc::channel();
        let engine = Arc::new(Engine::new());
        settings::ensure_library();
        let library = settings::library_dir().to_string_lossy().to_string();
        let upscayl_line = engine.upscayl_info();

        let tx0 = tx.clone();
        let ok = spawn_worker("rustpin-net", move || match Client::new() {
            Ok(client) => {
                tx0.send(Msg::Boot(Ok(Arc::new(client)))).ok();
            }
            Err(e) => {
                tx0.send(Msg::Boot(Err(format!("Pinterest is unreachable: {e}"))))
                    .ok();
            }
        });
        if !ok {
            // Nothing will ever arrive, so say so rather than spin forever.
            Self {
                loading: false,
                status: "Could not start the network thread.".into(),
                ..Self::blank(tx, rx, engine, library, upscayl_line)
            }
        } else {
            Self::blank(tx, rx, engine, library, upscayl_line)
        }
    }

    fn blank(
        tx: Sender<Msg>,
        rx: Receiver<Msg>,
        engine: Arc<Engine>,
        library: String,
        upscayl_line: String,
    ) -> Self {
        let query = String::new();
        let models = engine.models();
        // The box starts empty every time, and the grid does not open on the
        // downloads either: rustpin asks Pinterest for more like them and
        // shows that, which is what is actually worth looking at.
        let pins: Vec<Pin> = vec![];
        let suggested = false;
        let status = "Type something above to browse wallpapers.".into();
        let seen = HashSet::new();
        Self {
            tx,
            rx,
            client: None,
            engine,
            query,
            pins,
            bookmarks: vec![],
            seen,
            loading: true,
            textures: HashMap::new(),
            inflight: HashSet::new(),
            failed: HashSet::new(),
            selected: None,
            similar: vec![],
            similar_for: String::new(),
            similar_loading: false,
            similar_error: String::new(),
            jobs: vec![],
            announced: HashSet::new(),
            last_poll: Instant::now(),
            status,
            icon_tex: None,
            icon_tried: false,
            settings_open: false,
            typing: false,
            custom_res: Target::Auto.box_text(),
            models,
            lib_count: settings::library_pictures().len(),
            lib_counted: Instant::now(),
            upscayl_line,
            library,
            grid_scroll: SmoothScroll::new(),
            detail_scroll: SmoothScroll::new(),
            similar_scroll: SmoothScroll::new(),
            last_detect: Instant::now(),
            screen_seen: Instant::now(),
            search_at: None,
            wheel: 0.0,
            jobs_popup: false,
            suggested,
            suggesting: false,
            shown_at: HashMap::new(),
            fading: false,
        }
    }

    fn client(&self) -> Result<Arc<Client>, String> {
        self.client
            .clone()
            .ok_or_else(|| "Pinterest session not ready yet".to_string())
    }

    /// Only true when an Upscayl that can actually run is installed.
    fn upscayl_ok(&self) -> bool {
        self.engine.has_upscayl()
    }

    /// Wait a moment after the last keystroke, so "a" then "b" searches "ab"
    /// once instead of "a" and then "ab".
    fn arm_search(&mut self) {
        self.search_at = Some(Instant::now() + Duration::from_millis(350));
    }

    fn disarm_search(&mut self) {
        self.search_at = None;
    }

    /// Throw the old results away and go. An empty box just empties the grid.
    fn run_search(&mut self) {
        self.suggested = false;
        self.suggesting = false;
        self.pins.clear();
        self.seen.clear();
        self.bookmarks.clear();
        self.textures.clear();
        self.shown_at.clear();
        self.inflight.clear();
        self.failed.clear();
        self.selected = None;
        self.similar.clear();
        self.similar_for.clear();
        self.loading = false;
        // New results start at the top, mid-glide or not.
        self.grid_scroll.reset();
        self.detail_scroll.reset();
        self.similar_scroll.reset();
        if self.query.trim().is_empty() {
            self.status = "Type something to search.".into();
            return;
        }
        self.do_search(false);
    }

    /// Feed the desktop's own resolution in, so the automatic target matches
    /// whatever is on the wall right now: 4K screen, 4K wallpaper.
    fn note_screen(&mut self, ctx: &egui::Context) {
        if self.screen_seen.elapsed() < Duration::from_secs(5) {
            return;
        }
        self.screen_seen = Instant::now();
        let Some((w, h)) = primary_monitor(ctx) else {
            return;
        };
        if settings::set_screen(w, h) {
            // The box in settings shows the automatic value, so it has to
            // follow along when the screen is swapped or first measured.
            if matches!(self.engine.target(), Target::Auto) {
                self.custom_res = Target::Auto.box_text();
            }
        }
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
            format!("Searching “{q}”…")
        };
        let tx = self.tx.clone();
        let q = self.query.clone();
        let bm = self.bookmarks.clone();
        log::info!("searching for {q:?}{}", if more { " (more)" } else { "" });
        let started = spawn_worker("rustpin-search", move || match client.search(&q, &bm) {
            Ok((pins, bookmarks)) => {
                tx.send(Msg::Pins { pins, bookmarks }).ok();
            }
            Err(e) => {
                tx.send(Msg::Status(format!("Search failed: {e}"))).ok();
            }
        });
        if !started {
            log::error!("could not start the search thread");
            self.loading = false;
            self.status = "Could not start the search thread.".into();
        }
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
        let started = spawn_worker("rustpin-similar", move || {
            let result = client.related(&tid, &[]);
            let msg = match result {
                Ok((pins, _)) => Msg::Similar {
                    pin_id: tid,
                    pins,
                    error: String::new(),
                },
                Err(e) => Msg::Similar {
                    pin_id: tid,
                    pins: vec![],
                    error: e,
                },
            };
            tx.send(msg).ok();
        });
        if !started {
            self.similar_loading = false;
            self.similar_error = "Could not start the network thread.".into();
        }
    }

    /// Ask Pinterest for what goes with the wallpapers downloaded most
    /// recently \u{2014} its own "more like this" behind the scenes. The
    /// downloads themselves are never shown here: they are already on disk,
    /// and what is wanted is something new.
    fn suggest_more(&mut self) {
        if !self.client.is_some() {
            // Before the session is up there is nothing to ask; the boot
            // message does this again the moment it is.
            return;
        }
        let Ok(client) = self.client() else { return };
        let saves = self.engine.settings().saves;
        if saves.is_empty() {
            self.suggested = false;
            self.suggesting = false;
            if self.pins.is_empty() {
                self.status = "Type something and hit Search.".into();
            }
            return;
        }
        // A fresh set every time: the last one is thrown away, so home never
        // shows the same wallpapers twice.
        self.suggested = true;
        self.suggesting = true;
        self.pins.clear();
        self.seen.clear();
        self.bookmarks.clear();
        self.textures.clear();
        self.shown_at.clear();
        let mut skip: HashSet<String> = saves.iter().map(|s| s.id.clone()).collect();
        for p in &self.pins {
            skip.insert(p.id.clone());
        }
        let ids: Vec<String> = saves
            .iter()
            .take(SUGGEST_SEEDS)
            .map(|s| s.id.clone())
            .filter(|id| !id.is_empty())
            .collect();
        let tx = self.tx.clone();
        let started = spawn_worker("rustpin-suggest", move || {
            let mut out: Vec<Pin> = Vec::new();
            let mut added: HashSet<String> = HashSet::new();
            for id in ids {
                match client.related(&id, &[]) {
                    Ok((pins, _)) => {
                        for p in pins {
                            if added.insert(p.id.clone()) && skip.insert(p.id.clone()) {
                                out.push(p);
                            }
                        }
                    }
                    Err(e) => log::warn!("no suggestions for {id}: {e}"),
                }
            }
            log::info!("{} suggestions fetched", out.len());
            tx.send(Msg::Suggested { pins: out }).ok();
        });
        if !started {
            self.suggesting = false;
            self.status = "Could not start the suggestion thread.".into();
        }
    }

    fn request_img(&mut self, key: String, url: String) {
        if key.is_empty()
            || url.is_empty()
            || self.textures.contains_key(&key)
            || self.inflight.contains(&key)
            || self.failed.contains(&key)
        {
            return;
        }
        if self.inflight.len() >= MAX_INFLIGHT {
            // Too many pictures are already on their way. Nothing to do: the
            // tile asks again next frame and by then a slot has come free.
            log::debug!("{} pictures in flight, skipping {key}", self.inflight.len());
            return;
        }
        self.inflight.insert(key.clone());
        let tx = self.tx.clone();
        let k = key.clone();
        log::debug!("fetching {key}");
        let started = spawn_worker("rustpin-img", move || match fetch_color_image(&url) {
            Ok(img) => {
                tx.send(Msg::Img { key: k, img }).ok();
            }
            Err(e) => {
                log::debug!("picture failed: {k} ({e})");
                tx.send(Msg::ImgFail { key: k }).ok();
            }
        });
        if !started {
            // The key would never clear and the tile could never load again.
            self.inflight.remove(&key);
        }
    }

    fn request_local_img(&mut self, key: String, path: String) {
        if self.textures.contains_key(&key) || self.inflight.contains(&key) {
            return;
        }
        self.inflight.insert(key.clone());
        let tx = self.tx.clone();
        let k = key.clone();
        let started = spawn_worker("rustpin-local-img", move || {
            let got = std::fs::read(&path)
                .ok()
                .and_then(|b| decode_bytes(&b).ok());
            match got {
                Some(img) => {
                    tx.send(Msg::Img { key: k, img }).ok();
                }
                None => {
                    tx.send(Msg::ImgFail { key: k }).ok();
                }
            }
        });
        if !started {
            self.inflight.remove(&key);
        }
    }

    fn enqueue(&mut self, upscale: bool) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        let Some(pin) = self.pins.iter().find(|p| p.id == id).cloned() else {
            return;
        };
        if upscale && !self.engine.has_upscayl() {
            self.status = "Upscayl not found — install it first.".into();
            return;
        }
        let queued = self.engine.pending() > 0;
        log::info!("queueing {:?} (upscale: {upscale})", pin.title);
        // Remembered the moment it is asked for: this is what the start-up
        // suggestions and "more like this" are built out of next time.
        self.engine.update(|s| {
            s.remember_save(Saved {
                id: pin.id.clone(),
                title: pin.title.clone(),
                thumb: pin.thumb.clone(),
                w: pin.w,
                h: pin.h,
            })
        });
        self.engine.enqueue(&pin, upscale);
        // Say which one this is: with more than one waiting, the download that
        // is actually running is the oldest one, not this new request.
        self.status = if queued {
            "Queued — the one before it is still going.".into()
        } else {
            "Saving…".into()
        };
        self.jobs_popup = true;
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
                Msg::Boot(Ok(client)) => {
                    self.client = Some(client);
                    self.loading = false;
                    // The last search is not run. What opens instead is what
                    // Pinterest says goes with the wallpapers downloaded
                    // before, which is a fresh set every time.
                    if self.engine.settings().saves.is_empty() {
                        self.status = "Type something and hit Search.".into();
                    } else {
                        self.suggest_more();
                    }
                }
                Msg::Boot(Err(e)) => {
                    self.loading = false;
                    self.status = e;
                }
                Msg::Pins { pins, bookmarks } => {
                    self.loading = false;
                    self.bookmarks = bookmarks;
                    let mut added = 0;
                    let got = pins.len();
                    for p in pins {
                        if self.seen.insert(p.id.clone()) {
                            self.pins.push(p);
                            added += 1;
                        }
                    }
                    let more = !self.bookmarks.is_empty();
                    log::info!(
                        "got {got} pins, {added} new, {} more pages, {} pictures in flight",
                        self.bookmarks.len(),
                        self.inflight.len()
                    );
                    self.status = format!(
                        "{} wallpapers (+{added} new){}",
                        self.pins.len(),
                        if more {
                            " · scroll for more"
                        } else {
                            " · end"
                        }
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
                    self.shown_at.entry(key.clone()).or_insert_with(Instant::now);
                    log::debug!(
                        "texture {key} {}x{}, {} textures, {} still loading",
                        img.width(),
                        img.height(),
                        self.textures.len(),
                        self.inflight.len()
                    );
                    self.textures.insert(
                        key.clone(),
                        ctx.load_texture(key, img, egui::TextureOptions::LINEAR),
                    );
                    repaint = true;
                }
                Msg::ImgFail { key } => {
                    self.inflight.remove(&key);
                    self.failed.insert(key);
                }
                Msg::Suggested { pins } => {
                    self.suggesting = false;
                    // Only while the suggestions are still what is on screen:
                    // a search that has happened since must not be diluted by
                    // pictures chosen from an older download.
                    if !self.suggested {
                        return;
                    }
                    let mut added = 0;
                    for p in pins {
                        if self.seen.insert(p.id.clone()) {
                            self.pins.push(p);
                            added += 1;
                        }
                    }
                    log::info!("added {added} suggestions");
                    if added == 0 && self.pins.is_empty() {
                        // Nothing came back: an empty grid with a spinner on it
                        // for ever would look broken.
                        self.suggested = false;
                        self.status =
                            "Nothing to suggest yet \u{2014} search something above.".into();
                    } else if added > 0 {
                        self.status = format!(
                            "{} wallpapers \u{2014} picked for you, from the {} you have downloaded",
                            self.pins.len(),
                            self.engine.settings().saves.len()
                        );
                    }
                    repaint = true;
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
        // Poll slowly while the queue is idle, quickly whenever something is
        // moving or the window is open and showing it off.
        let busy = self.engine.pending() > 0;
        let due =
            self.jobs_popup || (busy && self.last_poll.elapsed() >= Duration::from_millis(400));
        if due {
            self.last_poll = Instant::now();
            let jobs = self.engine.jobs();
            // Report each finished download once, however the UI is looking.
            for j in &jobs {
                if j.status == "done" && self.announced.insert(j.id.clone()) {
                    log::info!("job {} done -> {}", j.id, j.output_path);
                    self.status = format!("Saved \u{2192} {}/{}", self.library, j.output_path);
                }
            }
            // Never let the list grow for ever.
            if jobs.len() > 40 {
                let keep: HashSet<&String> =
                    jobs.iter().skip(jobs.len() - 40).map(|j| &j.id).collect();
                self.announced.retain(|id| keep.contains(id));
            }
            self.jobs = if jobs.len() > 40 {
                jobs[jobs.len() - 40..].to_vec()
            } else {
                jobs
            };
        }
        if self.upscayl_line.contains("not found")
            && self.last_detect.elapsed() >= Duration::from_secs(5)
        {
            // `detect` walks the filesystem, so only do it now and then, and
            // only while it is missing something \u{2014} this is what notices an
            // Upscayl installed while rustpin is already open.
            self.last_detect = Instant::now();
            if self.engine.rediscover() {
                self.upscayl_line = self.engine.upscayl_info();
                // A newly-found models folder may hold models the picker was
                // only guessing at, so re-read what is actually there.
                self.models = self.engine.models();
                if !self.upscayl_line.contains("not found") {
                    self.status = self.upscayl_line.clone();
                }
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
        let k = key.clone();
        let started = spawn_worker("rustpin-icon", move || {
            let img = icon_candidates()
                .into_iter()
                .find_map(|p| std::fs::read(&p).ok().and_then(|b| decode_bytes(&b).ok()))
                .or_else(|| fetch_color_image(ICON_URL).ok());
            tx.send(Msg::Icon { key: k, img }).ok();
        });
        if !started {
            self.inflight.remove(&key);
            self.failed.insert(key);
            self.icon_tried = true;
        }
    }

    /// Aspect ratio a tile is laid out with.
    ///
    /// Always the shape Pinterest says the picture has, never the shape of a
    /// texture that happens to have arrived: a tile that resized itself as its
    /// picture landed would drag the whole grid out from under the pointer
    /// halfway down the list. The cube is drawn at the final size from the
    /// start, and the picture fades into it.
    fn aspect_of(&self, pin: &Pin, _key: &str) -> f32 {
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
                // A picture that has just arrived fades into its cube over
                // FADE rather than snapping in, so a screenful of thumbnails
                // arrives as a shimmer instead of a jump.
                let since = self
                    .shown_at
                    .get(key)
                    .map(|at| at.elapsed().as_secs_f32())
                    .unwrap_or(FADE);
                let alpha = (since / FADE).clamp(0.0, 1.0);
                if alpha < 1.0 {
                    self.fading = true;
                }
                egui::Image::new(&t)
                    .fit_to_exact_size(rect.size())
                    // The cube is the shape Pinterest promised, and this is
                    // what it turns out to be: fitted, not stretched.
                    .maintain_aspect_ratio(true)
                    .tint(egui::Color32::from_white_alpha((alpha * 255.0) as u8))
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
            let (w, a) = if selected {
                (2.5_f32, 255_u8)
            } else {
                (1.5_f32, 140_u8)
            };
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
    /// The settings popup: resolution, the model to upscale with, the one
    /// checkbox, and the two buttons about the library itself \u{2014} reset
    /// the suggestions and delete every photo.
    fn show_settings(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let saved = self.engine.settings();
        let mut close = false;
        let mut typed = self.custom_res.clone();
        let mut apply = false;
        let mut picked: Option<String> = None;
        let mut delete_now = false;
        let mut reset_suggestions = false;
        // Cloned out before the window's closure borrows `self`: the combo
        // needs the list by value to draw, and `self` is already borrowed.
        let model_choices = self.models.clone();

        egui::Window::new("Settings")
            .collapsible(false)
            .resizable(false)
            .title_bar(false)
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-8.0, 46.0))
            .show(ctx, |ui| {
                ui.set_min_width(230.0);
                header_row(
                    ui,
                    egui::RichText::new("Settings").heading(),
                    CLOSE_SIZE,
                    |ui| {
                        if close_button(ui, "Close settings (Esc)").clicked() {
                            close = true;
                        }
                    },
                );
                ui.separator();

                let resp = ui.add(
                    egui::TextEdit::singleline(&mut typed)
                        .desired_width(ui.available_width())
                        .hint_text("1920x1080"),
                );
                self.typing |= resp.has_focus();
                apply = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                ui.add_space(8.0);

                // ---- model picker ----
                // "Auto" is the empty setting, which is also what an older
                // settings file decodes to, so nothing has to be migrated.
                let current = if saved.model.is_empty() {
                    "Auto (best available)".to_string()
                } else {
                    model_label(&saved.model)
                };
                egui::ComboBox::from_id_salt("model")
                    .selected_text(current)
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        ui.add(egui::SelectableLabel::new(
                            saved.model.is_empty(),
                            "Auto (best available)",
                        ))
                        .on_hover_text("Upscayl picks: standard 4x, else whatever is installed");
                        for m in &model_choices {
                            let sel = saved.model == *m;
                            if ui
                                .add(egui::SelectableLabel::new(sel, model_label(m)))
                                .on_hover_text(format!("{m}\nUpscayl model"))
                                .clicked()
                            {
                                picked = Some(m.clone());
                            }
                        }
                    });

                ui.add_space(8.0);
                let mut delete_orig = saved.delete_orig;
                if ui
                    .checkbox(&mut delete_orig, "Delete original after upscaling")
                    .changed()
                {
                    self.engine.update(|s| s.delete_orig = delete_orig);
                }

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
                // ---- what the suggestions are built from ----
                // Every download is remembered, and the suggestions on the home
                // screen are Pinterest's "more like this" for the three most
                // recent. Forgetting them starts that over from nothing, which
                // is what "reset algorithm" means: the photos stay, only what
                // was remembered about them goes.
                let learned = saved.saves.len();
                ui.label(
                    egui::RichText::new(format!("Learned from {learned} download(s)"))
                        .weak(),
                );
                // Reset on the left, delete on the right, in one row: both are
                // about the library, so they belong next to each other.
                ui.horizontal(|ui| {
                    let count = self.lib_count;
                    if ui
                        .add_enabled(learned > 0, egui::Button::new("\u{1F504} Reset algorithm"))
                        .on_hover_text(
                            "Forget which wallpapers were downloaded, so the suggestions start over. The photos themselves stay.",
                        )
                        .clicked()
                    {
                        reset_suggestions = true;
                    }
                    if ui
                        .add_enabled(
                            count > 0,
                            egui::Button::new(format!("\u{1F5D1} Delete all {count} photo(s)")),
                        )
                        .on_hover_text("Delete every wallpaper in the library folder")
                        .clicked()
                    {
                        delete_now = true;
                    }
                });
            });

        if apply {
            match Target::parse(&typed) {
                Some(t) => {
                    self.engine.update(|s| s.target = t);
                }
                None => {
                    // Leave it on the screen so it can be corrected, but do not
                    // store something that is not a resolution.
                    self.status = format!("Not a resolution: {typed}");
                    typed = saved.target.box_text();
                }
            }
        }
        self.custom_res = typed;
        if let Some(m) = picked {
            self.engine.update(|s| s.model = m);
            self.upscayl_line = self.engine.upscayl_info();
            self.status = format!("Upscaling with {}", self.engine.model_choice());
        }
        if reset_suggestions {
            let n = saved.saves.len();
            self.engine.update(|s| s.saves.clear());
            self.suggested = false;
            self.suggesting = false;
            self.status = format!("Forgot {n} download(s) \u{2014} the suggestions start over.");
            log::info!("algorithm reset: forgot {n} download(s)");
        }
        if delete_now {
            self.delete_all_photos();
        }
        if close {
            self.settings_open = false;
        }
    }

    /// Keep the photo count on the delete button roughly current without
    /// re-reading the folder on every frame.
    fn note_library_count(&mut self) {
        if !self.settings_open || self.lib_counted.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.lib_counted = Instant::now();
        self.lib_count = settings::library_pictures().len();
    }

    /// Empty the library. Every downloaded wallpaper goes; the jobs list is
    /// dropped too, because its finished rows point at files that are no longer
    /// there and would draw as broken pictures.
    fn delete_all_photos(&mut self) {
        if self.engine.pending() > 0 {
            log::warn!("delete-all refused: {} job(s) running", self.engine.pending());
            self.status = "Wait for the running jobs to finish before deleting.".into();
            return;
        }
        match settings::delete_all_pictures() {
            Ok(n) => {
                log::info!("deleted every photo ({n}) from {}", self.library);
                self.status = format!("Deleted {n} photo(s) from {}", self.library);
                self.lib_count = 0;
                self.jobs.clear();
                self.announced.clear();
                self.textures
                    .retain(|k, _| !k.starts_with("result:"));
                self.shown_at.retain(|k, _| !k.starts_with("result:"));
                self.failed.retain(|k| !k.starts_with("result:"));
            }
            Err(e) => {
                log::error!("delete-all failed: {e}");
                self.status = format!("Could not delete everything: {e}");
            }
        }
    }
}

// ---------- panels ----------

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.render(ctx);
    }
}

impl App {
    /// The whole UI, one frame.
    fn render(&mut self, ctx: &egui::Context) {
        // Cleared here and set again by every tile that is mid-fade, so the
        // end of the frame can tell whether any fading is left to animate.
        self.fading = false;
        // Take the notch before any ScrollArea can see it. egui applies a
        // wheel notch to every area it thinks it belongs to and only clears it
        // for the parent it already handled, which is what made one turn of
        // the wheel move the grid and the panel at the same time.
        self.wheel = ctx.input(|i| i.smooth_scroll_delta.y);
        ctx.input_mut(|i| i.smooth_scroll_delta.y = 0.0);

        self.pump(ctx);
        self.note_screen(ctx);
        self.poll_jobs();
        self.load_app_icon();
        self.typing = false;
        self.note_library_count();
        self.show_settings(ctx);
        // The search box searches for itself; this is where the wait runs out.
        if let Some(at) = self.search_at {
            if Instant::now() >= at {
                self.search_at = None;
                self.run_search();
            }
        }
        // A running download or a pending auto-search needs frames now rather
        // than in 1.5s. Scrolling does not: the wheel moves the content and
        // that is the end of it.
        if self.jobs_popup || self.engine.pending() > 0 || self.search_at.is_some() {
            ctx.request_repaint();
        }
        ctx.request_repaint_after(Duration::from_millis(if self.engine.pending() > 0 {
            300
        } else {
            1500
        }));

        // ---- top bar ----
        egui::TopBottomPanel::top("bar")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(10, 6)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    match self.icon_tex.clone() {
                        Some(icon) => {
                            ui.add(egui::Image::new(&icon).max_height(24.0));
                        }
                        None => {
                            ui.label("\u{1F4CC}");
                        }
                    }
                    // Tuck the search box up against the logo instead of leaving it
                    // a default item gap away.
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text("Search wallpapers\u{2026}")
                            .desired_width(260.0),
                    );
                    self.typing |= resp.has_focus();
                    // No Search button: the box searches for itself once the typing
                    // settles, and Enter skips the wait.
                    if resp.changed() {
                        self.arm_search();
                    }
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        self.disarm_search();
                        self.run_search();
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("\u{2699} Settings").clicked() {
                            self.settings_open = !self.settings_open;
                        }
                        if ui.button("\u{1F4C1} Folder").clicked() {
                            open_with(&self.library);
                        }
                    });
                });
            });

        // ---- status bar ----
        let jobs_button = egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(10, 6)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let pending = self.engine.pending();
                    let text = if pending > 0 {
                        format!("\u{1F4C1} Jobs ({pending})")
                    } else {
                        "\u{1F4C1} Jobs".to_string()
                    };
                    if ui.button(text).clicked() {
                        self.jobs_popup = !self.jobs_popup;
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Two words either way: Upscayl when it is there, Not found
                        // when it is not. The detail is one hover away.
                        let found = self.upscayl_ok();
                        let label = if found { "Upscayl" } else { "Not found" };
                        ui.small(egui::RichText::new(label).weak())
                            .on_hover_text(self.upscayl_line.clone());
                    });
                });
            });

        // Anchor the window just above the button that opened it.
        if self.jobs_popup {
            let anchor = jobs_button.response.rect;
            self.show_jobs_popup(ctx, anchor.left_top() - egui::vec2(0.0, 6.0));
        }

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

        // A tile that faded in this frame needs a few more frames to finish
        // fading, so ask for them now that the panels have said how many there
        // are.
        if self.fading {
            ctx.request_repaint();
        }

        // Esc backs out of whatever is open on top: the settings popup first,
        // then the wallpaper being looked at. Not while typing (Esc in the
        // search box must not throw the open wallpaper away) and not while a
        // dropdown is open \u{2014} that one closes itself on Esc.
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

    fn show_grid(&mut self, ui: &mut egui::Ui) {
        if self.pins.is_empty() {
            if self.loading || self.suggesting {
                ui.centered_and_justified(|ui| {
                    ui.spinner();
                    ui.label("Finding wallpapers you might like\u{2026}");
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

        // Move the glide on before the area reads it; `sync` below picks up
        // whatever the area did on its own.
        self.grid_scroll.viewport = ui.clip_rect();
        let grid_wheel = if self.grid_scroll.hovered(ui) {
            self.wheel
        } else {
            0.0
        };
        self.grid_scroll.step(ui, Some(grid_wheel));

        let out = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .vertical_scroll_offset(self.grid_scroll.offset)
            .show(ui, |ui| {
                let clip = ui.clip_rect().expand(GRID_GAP * 3.0);
                // One screen further down is fetched but not drawn, so the
                // pictures are already there by the time the grid scrolls onto
                // them instead of grey squares.
                let prefetch = egui::Rect::from_min_max(
                    ui.clip_rect().min,
                    ui.clip_rect().max + egui::vec2(0.0, ui.clip_rect().height() * 0.75),
                );
                let (area, _) =
                    ui.allocate_exact_size(egui::vec2(width, content_h), egui::Sense::hover());
                let pins = std::mem::take(&mut self.pins);
                for (i, r) in rects.iter().enumerate() {
                    let r = r.translate(area.min.to_vec2());
                    let p = &pins[i];
                    if !clip.intersects(r) {
                        if prefetch.intersects(r) {
                            let (key, url) = (p.id.clone(), p.thumb.clone());
                            self.request_img(key, url);
                        }
                        continue;
                    }
                    let key = p.id.clone();
                    let url = p.thumb.clone();
                    self.draw_cell(ui, p, &key, &url, r, 12.0);
                }
                self.pins = pins;
            });
        self.grid_scroll.sync(
            out.state.offset.y,
            out.content_size.y - out.inner_rect.height(),
            out.inner_rect,
        );

        let remaining = out.content_size.y - (out.state.offset.y + out.inner_rect.height());
        if !self.suggested
            && !self.loading
            && !self.bookmarks.is_empty()
            && !self.pins.is_empty()
            && remaining < 900.0
        {
            self.do_search(true);
        }
    }
}

impl App {
    fn show_detail(&mut self, ui: &mut egui::Ui, pin: &Pin) {
        if self.similar_for != pin.id {
            self.load_similar(&pin.id);
        }

        // header row pinned above the scroll area: wallpaper name on the left,
        // Pinterest + close on the right
        let page_w = small_button_width(ui, "↗ Pinterest");
        let strip_w = CLOSE_SIZE + 2.0 * ui.spacing().item_spacing.x + page_w;
        header_row(
            ui,
            egui::RichText::new(&pin.title).strong().size(16.0),
            strip_w,
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

        // One notch, one owner. The detail panel and the "More like this"
        // list inside it are nested, and egui would happily apply a notch to
        // both; here the inner one takes it while it still has room, and hands
        // it back to the panel once it has reached the end, so the two read as
        // one continuous scroll.
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let over_panel = pointer.is_some_and(|p| ui.clip_rect().contains(p));
        let over_inner = pointer.is_some_and(|p| self.similar_scroll.viewport.contains(p));
        let inner_takes = over_inner && self.similar_scroll.can_scroll(self.wheel);

        self.detail_scroll.viewport = ui.clip_rect();
        let detail_wheel = if over_panel && !inner_takes {
            self.wheel
        } else {
            0.0
        };
        self.detail_scroll.step(ui, Some(detail_wheel));
        let out = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .vertical_scroll_offset(self.detail_scroll.offset)
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
                let button_w = ui.available_width();
                if ui
                    .add_sized([button_w, 34.0], egui::Button::new("⬇ Download + Upscale"))
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
                // How far down the panel's content the list starts, in content
                // coordinates: this is what decides how much of the panel is
                // left over for it.
                let list_top = ui.min_rect().top();
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
                    // Its own scroll area, so the wheel carries on past the
                    // boundary instead of stopping dead at the top of this
                    // list.
                    self.similar_scroll
                        .step(ui, Some(if inner_takes { self.wheel } else { 0.0 }));
                    let out = egui::ScrollArea::vertical()
                        .id_salt("similar")
                        .auto_shrink([false, false])
                        .max_height(similar_height(list_top, self.detail_scroll.viewport))
                        .vertical_scroll_offset(self.similar_scroll.offset)
                        .show(ui, |ui| {
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
                        });
                    self.similar_scroll.sync(
                        out.state.offset.y,
                        out.content_size.y - out.inner_rect.height(),
                        out.inner_rect,
                    );
                }
            });
        self.detail_scroll.sync(
            out.state.offset.y,
            out.content_size.y - out.inner_rect.height(),
            out.inner_rect,
        );
    }

    /// The jobs window: a list of everything the queue is doing or has done,
    /// newest first, with a cancel on each one still running. It opens on an
    /// empty queue too \u{2014} an empty list is information as well.
    fn show_jobs_popup(&mut self, ctx: &egui::Context, above: egui::Pos2) {
        if !self.jobs_popup {
            return;
        }
        let mut cancel: Option<String> = None;

        egui::Area::new(egui::Id::new("jobs-popup"))
            .fixed_pos(above)
            // Grow upwards: an area grows downwards from its position by
            // default, which would lay the window over the bar it belongs to.
            .anchor(egui::Align2::LEFT_BOTTOM, egui::vec2(0.0, 0.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(320.0);
                    ui.label(egui::RichText::new("Jobs").heading());

                    if self.jobs.is_empty() {
                        ui.label("No wallpapers yet.");
                    } else {
                        egui::ScrollArea::vertical()
                            .id_salt("jobs")
                            .max_height(300.0)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                // Newest first, and cloned out of `self` because
                                // drawing a row needs `&mut self` for textures.
                                let rows: Vec<Job> =
                                    self.jobs.iter().rev().take(25).cloned().collect();
                                for j in &rows {
                                    self.job_row(ui, j, &mut cancel);
                                    ui.separator();
                                }
                            });
                    }
                    ui.separator();
                    if ui.button("Close").clicked() {
                        self.jobs_popup = false;
                    }
                });
            });

        // Applied outside the closure: cancelling is a call into the engine and
        // cannot happen while the popup's borrow of `self` is live.
        if let Some(id) = cancel {
            self.engine.cancel(&id);
            self.status = "Cancelling…".into();
        }
    }

    /// One wallpaper's row: what it is, what stage it is at, and a way out.
    fn job_row(&mut self, ui: &mut egui::Ui, j: &Job, cancel: &mut Option<String>) {
        let running = !j.done() && j.status != "cancelled";

        ui.horizontal(|ui| {
            // A finished job gets to show its face; the others would only be
            // loading the same thumbnail the grid already has.
            if j.status == "done" && !j.output_path.is_empty() {
                let key = format!("result:{}", j.id);
                let full = jobs::library_file(&j.output_path);
                self.request_local_img(key.clone(), full.to_string_lossy().to_string());
                match self.textures.get(&key).cloned() {
                    Some(t) => {
                        ui.add(egui::Image::new(&t).max_size(egui::vec2(44.0, 44.0)));
                    }
                    None => {
                        ui.allocate_space(egui::vec2(44.0, 44.0));
                    }
                }
            } else {
                ui.allocate_space(egui::vec2(44.0, 44.0));
            }

            ui.vertical(|ui| {
                ui.add(egui::Label::new(egui::RichText::new(&j.title).strong()).truncate());
                let stage = match j.status.as_str() {
                    "queued" => "Queued".to_string(),
                    "downloading" => format!("Downloading \u{2026} {} KB", j.downloaded / 1024),
                    "upscaling" => format!("Upscaling {} at {}x", j.model, j.scale),
                    "saving" => "Saving".into(),
                    "done" => "Saved".into(),
                    "cancelled" => "Cancelled".into(),
                    _ => "Failed".into(),
                };
                let color = match j.status.as_str() {
                    "done" => egui::Color32::GREEN,
                    "error" => egui::Color32::RED,
                    "cancelled" => egui::Color32::GRAY,
                    _ => egui::Color32::YELLOW,
                };
                ui.colored_label(color, stage);
                if running && j.status == "downloading" && j.total > 0 {
                    ui.add(egui::ProgressBar::new(j.progress.clamp(0.0, 1.0)).desired_width(180.0));
                } else if running {
                    ui.add(
                        egui::ProgressBar::new(j.progress.clamp(0.0, 1.0))
                            .show_percentage()
                            .desired_width(180.0),
                    );
                } else if j.status == "done" && !j.note.is_empty() {
                    // Succeeded, but not the way it was asked to: the upscale
                    // was rejected and the original is what got saved.
                    ui.label(egui::RichText::new(&j.note).small().weak());
                } else if j.status == "error" && !j.error.is_empty() {
                    ui.label(
                        egui::RichText::new(j.error.chars().take(120).collect::<String>())
                            .small()
                            .weak(),
                    );
                }
            });

            if running && ui.button("Cancel").clicked() {
                *cancel = Some(j.id.clone());
            }
        });
    }
}

fn main() -> eframe::Result<()> {
    // Logs go to whatever terminal rustpin was started from: `RUST_LOG=debug`
    // for the per-picture chatter, `warn` for just the trouble.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    log::info!(
        "rustpin starting ({})",
        env!("CARGO_PKG_VERSION")
    );

    // RUSTPIN_SIZE=1600x900 and RUSTPIN_FULLSCREEN=1 are handy for checking
    // the layout at other window sizes.
    let (mut w, mut h) = (1180.0_f32, 800.0_f32);
    if let Ok(spec) = std::env::var("RUSTPIN_SIZE") {
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

    /// Names are ugly on disk because they double as filenames; the picker has
    /// to show them readably without mangling the parts that are names already.
    #[test]
    fn model_names_are_shown_readably() {
        assert_eq!(model_label("upscayl-standard-4x"), "Upscayl Standard 4x");
        assert_eq!(model_label("upscayl-lite-4x"), "Upscayl Lite 4x");
        assert_eq!(model_label("realesrgan-x4plus"), "Realesrgan x4plus");
        assert_eq!(model_label("ultramix-balanced-4x"), "Ultramix Balanced 4x");
        // Non-ASCII must not panic or slice mid-character.
        assert!(!model_label("mödel-ü4x").is_empty());
        assert_eq!(model_label(""), "");
    }

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

    /// A tall panel has no room to spare at the bottom: the list takes what is
    /// left of the panel, down to the last pixel.
    #[test]
    fn the_similar_list_fills_the_bottom_of_a_tall_panel() {
        let panel = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(340.0, 1400.0));
        let h = similar_height(500.0, panel);
        assert!(
            (500.0 + SIM_HEADING_H + h - 1400.0).abs() < 0.01,
            "the list ended {}px short of the bottom",
            1400.0 - (500.0 + SIM_HEADING_H + h)
        );
    }

    /// A short panel cannot give the list a height worth having, so it keeps
    /// the fixed one and the panel itself does the scrolling.
    #[test]
    fn a_short_panel_keeps_the_fixed_list_height() {
        let panel = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(340.0, 600.0));
        assert_eq!(similar_height(500.0, panel), SIM_SECTION_H);
    }

    #[test]
    fn one_column_when_there_is_no_room() {
        assert_eq!(columns_for(90.0, GRID_CELL_WANT, GRID_GAP, 10), 1);
        assert_eq!(columns_for(1900.0, GRID_CELL_WANT, GRID_GAP, 10), 9);
    }

    /// One wheel notch, then nothing: the content is exactly where the notch
    /// put it on the very same frame, with no coast and no lag, and it moves
    /// as far as a browser moves it rather than egui's much smaller step.
    #[test]
    fn a_wheel_notch_moves_the_content_by_a_browsers_notch() {
        let mut s = SmoothScroll::new();
        s.max_offset = 5000.0;
        s.advance(1.0 / 60.0, -EGUI_NOTCH);
        assert_eq!(s.offset, BROWSER_NOTCH, "a notch should move 100px");
        s.advance(1.0 / 60.0, 0.0);
        assert_eq!(s.offset, BROWSER_NOTCH, "an idle frame must not drift");
    }

    /// A trackpad arrives as a stream of small pieces, and those stay unscaled
    /// so a gentle drag is still gentle.
    #[test]
    fn a_trackpad_drag_is_not_turned_into_a_notch() {
        let mut s = SmoothScroll::new();
        s.max_offset = 5000.0;
        s.advance(1.0 / 60.0, -4.0);
        assert_eq!(s.offset, 4.0, "a small wheel piece should move just itself");
    }

    #[test]
    fn scrolling_up_at_the_top_and_down_at_the_end_never_escapes() {
        let mut s = SmoothScroll::new();
        s.max_offset = 500.0;
        for _ in 0..300 {
            s.advance(1.0 / 60.0, 50.0); // wheel up, at the very top
        }
        assert_eq!(s.offset, 0.0, "scrolled past the top of the grid");
        for _ in 0..3000 {
            s.advance(1.0 / 60.0, -50.0); // wheel down, a long way past the end
        }
        assert_eq!(s.offset, 500.0, "scrolled past the bottom of the grid");
    }

    /// A stopped app must not fling the grid when it starts up again: egui
    /// reports a zero delta, and zero has to mean zero.
    #[test]
    fn a_still_frame_does_not_move_anything() {
        let mut s = SmoothScroll::new();
        s.max_offset = f32::INFINITY;
        s.advance(1.0 / 60.0, -50.0);
        let stopped_at = s.offset;
        for _ in 0..10 {
            s.advance(0.0, 0.0);
        }
        assert_eq!(s.offset, stopped_at);
    }
}
