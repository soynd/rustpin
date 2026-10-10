//! Persistent settings + the picture library location.
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// The desktop's own resolution, remembered so "auto" can mean "whatever this
/// screen is". Physical pixels, not logical ones: a HiDPI 4K panel really is
/// 3840×2160 and a wallpaper for it should be too.
///
/// `Auto` is stored in the settings file as the word "auto", and this is filled
/// in once the window is up, so the default survives being loaded before any
/// monitor has been seen.
static SCREEN_W: AtomicU32 = AtomicU32::new(1920);
static SCREEN_H: AtomicU32 = AtomicU32::new(1080);

/// The resolution the automatic target follows. Returns true when it changed.
pub fn set_screen(w: u32, h: u32) -> bool {
    let w = w.clamp(640, 16384);
    let h = h.clamp(480, 16384);
    // Both swaps have to happen: `a || b` would skip the height store whenever
    // the width changed, and leave a half-updated screen behind.
    let w_changed = SCREEN_W.swap(w, Ordering::Relaxed) != w;
    let h_changed = SCREEN_H.swap(h, Ordering::Relaxed) != h;
    w_changed || h_changed
}

pub fn screen() -> (u32, u32) {
    (
        SCREEN_W.load(Ordering::Relaxed),
        SCREEN_H.load(Ordering::Relaxed),
    )
}

/// What a resolution reads as in the settings box.
pub fn box_text(w: u32, h: u32) -> String {
    format!("{w}x{h}")
}

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum Target {
    /// Auto: whatever resolution the screen itself is.
    #[default]
    Auto,
    /// A resolution typed by hand, used exactly as given.
    Exact(u32, u32),
}

impl Target {
    pub fn key(self) -> String {
        match self {
            Target::Auto => "auto".into(),
            Target::Exact(w, h) => format!("{w}x{h}"),
        }
    }

    pub fn parse(raw: &str) -> Option<Target> {
        let t = raw.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("auto") {
            return Some(Target::Auto);
        }
        let (a, b) = t.split_once(['x', 'X'])?;
        let w = a.trim().parse().ok()?;
        let h = b.trim().parse().ok()?;
        if w < 16 || h < 16 {
            return None;
        }
        Some(Target::Exact(w, h))
    }

    /// What the settings box shows: the screen's own resolution while it is
    /// still automatic, otherwise exactly what was typed.
    pub fn box_text(self) -> String {
        match self {
            Target::Auto => {
                let (w, h) = screen();
                box_text(w, h)
            }
            Target::Exact(w, h) => box_text(w, h),
        }
    }

    /// The box the result has to fit in. The automatic box is turned to match
    /// the image (a portrait pin gets 1080×1920), a typed box is used exactly
    /// as given.
    pub fn box_for(self, w: u32, h: u32) -> (u32, u32) {
        let (bw, bh) = match self {
            Target::Auto => screen(),
            Target::Exact(w, h) => return (w, h),
        };
        if h > w {
            (bh, bw)
        } else {
            (bw, bh)
        }
    }
}

/// Upscaling factor: the smallest of 4x/3x/2x that still reaches the target.
///
/// 2x is the floor because Upscayl has no 1x model. A source that is already
/// larger than the target therefore still gets doubled — `fit_size` is what
/// brings the result back down to the box afterwards, so an oversized original
/// costs time rather than producing a wrong size.
pub fn auto_scale(target: Target, w: u32, h: u32) -> u32 {
    let (bw, bh) = target.box_for(w.max(1), h.max(1));
    let need = (bw as f64 / w.max(1) as f64).min(bh as f64 / h.max(1) as f64);
    for cand in [2u32, 3, 4] {
        if cand as f64 >= need {
            return cand;
        }
    }
    4
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

/// A wallpaper the user has downloaded, remembered so rustpin can put more
/// like it in front of them next time it opens. The picture itself is in the
/// library; this is just enough to draw the tile and to ask Pinterest what goes
/// with it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Saved {
    pub id: String,
    pub title: String,
    pub thumb: String,
    pub w: u32,
    pub h: u32,
}

/// The record of what has been downloaded is capped, so years of poking about
/// cannot grow the settings file for ever.
pub const MAX_SAVES: usize = 200;

#[derive(Clone, Debug)]
pub struct Settings {
    pub delete_orig: bool,
    pub target: Target,
    /// Which Upscayl model to upscale with. Empty means "decide for me" — see
    /// [`upscayl::pick_model`], which prefers the standard 4x and falls back to
    /// whatever else the models folder actually has.
    pub model: String,
    /// What has been downloaded, newest first, which is what the suggestions
    /// on start-up are built out of.
    pub saves: Vec<Saved>,
}

impl Settings {
    /// Record a download. Re-downloading the same wallpaper moves the old entry
    /// to the front instead of adding it twice.
    pub fn remember_save(&mut self, saved: Saved) {
        if saved.id.is_empty() || saved.thumb.is_empty() {
            return;
        }
        self.saves.retain(|s| s.id != saved.id);
        self.saves.insert(0, saved);
        self.saves.truncate(MAX_SAVES);
    }

}

impl Default for Settings {
    fn default() -> Self {
        Self {
            delete_orig: true,
            target: Target::Auto,
            model: String::new(),
            saves: Vec::new(),
        }
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
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
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        if let Some(b) = v.get("delete_orig").and_then(|b| b.as_bool()) {
            s.delete_orig = b;
        }
        if let Some(t) = v
            .get("target")
            .and_then(|t| t.as_str())
            .and_then(Target::parse)
        {
            s.target = t;
        }
        if let Some(m) = v.get("model").and_then(|m| m.as_str()) {
            s.model = m.trim().chars().take(80).collect();
        }
        // An old file has no "saves" key, which has to mean "nothing saved yet"
        // rather than an error: opening the app on an older settings file is
        // the normal case, not a failure.
        if let Some(list) = v.get("saves").and_then(|s| s.as_array()) {
            s.saves = serde_json::from_value::<Vec<Saved>>(serde_json::Value::Array(list.clone()))
                .unwrap_or_default()
                .into_iter()
                .filter(|s| !s.id.is_empty() && !s.thumb.is_empty())
                .take(MAX_SAVES)
                .collect();
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
        "model": s.model,
        "saves": s.saves,
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

/// Is `p` one of the picture formats rustpin saves?
pub fn is_picture(p: &Path) -> bool {
    p.extension().is_some_and(|e| {
        let e = e.to_string_lossy().to_ascii_lowercase();
        matches!(e.as_str(), "jpg" | "jpeg" | "png" | "webp")
    })
}

/// Every picture sitting in the library right now, sorted by name.
///
/// Used by "delete all photos", which is why only pictures are counted: the
/// folder can hold a `settings.json` and a stray `.rustpin-*` scratch file, and
/// neither is a wallpaper the user would miss.
pub fn library_pictures() -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(library_dir()) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_picture(p))
        .collect();
    out.sort();
    out
}

/// Delete every picture in the library. Returns how many went, or the first
/// refusal — stopping at the first error means a locked file cannot leave the
/// user with a half-cleared folder and no idea which half.
pub fn delete_all_pictures() -> Result<usize, String> {
    let files = library_pictures();
    let mut done = 0usize;
    for path in files {
        fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        done += 1;
    }
    Ok(done)
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

    /// `RUSTPIN_DIR` and `XDG_CONFIG_HOME` are process-global, so every test
    /// that swaps one takes this lock; the ones below would otherwise race
    /// each other over which library folder is in effect.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Run `f` against a throwaway library folder, so the real picture
    /// library is never touched by a test.
    fn with_library(files: &[&str], f: impl FnOnce(&Path)) {
        let _guard = env_lock();
        let dir = std::env::temp_dir().join(format!("rustpin-lib-{}-{n}", std::process::id(), n = next_id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for name in files {
            fs::write(dir.join(name), b"x").unwrap();
        }
        let old = std::env::var_os("RUSTPIN_DIR");
        std::env::set_var("RUSTPIN_DIR", &dir);
        f(&dir);
        match old {
            Some(v) => std::env::set_var("RUSTPIN_DIR", v),
            None => std::env::remove_var("RUSTPIN_DIR"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// "Delete all photos" has to mean photos. The library folder can also hold
    /// a settings.json and in-flight `.rustpin-*` scratch files, and neither is
    /// something the user would want gone.
    #[test]
    fn delete_all_spares_non_pictures() {
        with_library(
            &[
                "a.png",
                "b.jpg",
                "c.webp",
                "settings.json",
                ".rustpin-download-1",
            ],
            |dir| {
                let found = library_pictures();
                assert_eq!(found.len(), 3, "{found:?}");
                assert!(found.iter().all(|p| is_picture(p)));

                assert_eq!(delete_all_pictures().expect("delete failed"), 3);
                assert!(library_pictures().is_empty());
                assert!(
                    dir.join("settings.json").is_file(),
                    "the settings file must survive"
                );
                assert!(
                    dir.join(".rustpin-download-1").is_file(),
                    "a scratch file must survive"
                );
            },
        );
    }

    #[test]
    fn deleting_an_empty_library_is_not_an_error() {
        with_library(&[], |_| {
            assert_eq!(delete_all_pictures().expect("empty is not a failure"), 0);
        });
    }

    /// The formats rustpin actually saves all count as pictures.
    #[test]
    fn every_saved_format_is_recognised() {
        assert!(is_picture(Path::new("a.png")));
        assert!(is_picture(Path::new("a.PNG")));
        assert!(is_picture(Path::new("a.jpg")));
        assert!(is_picture(Path::new("a.jpeg")));
        assert!(is_picture(Path::new("a.webp")));
        assert!(!is_picture(Path::new("settings.json")));
        assert!(!is_picture(Path::new("a.png.log")));
        assert!(!is_picture(Path::new("no-extension")));
    }

    fn next_id() -> u32 {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        COUNTER.fetch_add(1, Ordering::Relaxed)
    }

    /// A model setting has to survive the round trip through the settings
    /// file, and an old file without the key has to mean "auto".
    #[test]
    fn the_model_setting_is_saved_and_reloaded() {
        let _guard = env_lock();
        let mut s = Settings::default();
        assert_eq!(s.model, "", "an unset model means auto");
        s.model = "upscayl-lite-4x".into();
        s.target = Target::Exact(2560, 1440);

        let path = std::env::temp_dir().join(format!("rustpin-settings-{}.json", std::process::id()));
        std::env::set_var("XDG_CONFIG_HOME", path.parent().unwrap());
        std::env::set_var("RUSTPIN_DIR", &path);
        save(&s);
        let back = load();
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("RUSTPIN_DIR");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());

        assert_eq!(back.model, "upscayl-lite-4x");
        assert_eq!(back.target, Target::Exact(2560, 1440));
    }

    /// The record of what has been downloaded is what the start-up
    /// suggestions are built from, so it has to survive a restart, newest
    /// first, and without a repeat of the same wallpaper.
    #[test]
    fn saves_survive_a_restart() {
        let _guard = env_lock();
        let dir = std::env::temp_dir().join(format!("rustpin-saves-{}", std::process::id()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        let _ = fs::remove_dir_all(&dir);

        let one = |id: &str| Saved {
            id: id.into(),
            title: "dark".into(),
            thumb: format!("https://t/{id}"),
            w: 736,
            h: 1300,
        };
        let mut s = Settings::default();
        s.remember_save(one("2"));
        s.remember_save(one("1"));
        // The same wallpaper again moves up instead of being kept twice.
        s.remember_save(one("2"));

        save(&s);
        let back = load();
        std::env::remove_var("XDG_CONFIG_HOME");
        let _ = fs::remove_dir_all(&dir);

        let ids: Vec<&str> = back.saves.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["2", "1"]);
        assert_eq!(back.saves[0].h, 1300);
    }

    /// The screen is process-global, so anything that reads or writes it takes
    /// this lock. Tests that never touch the screen do not need it.
    fn with_screen<T>(w: u32, h: u32, f: impl FnOnce() -> T) -> T {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_screen(w, h);
        f()
    }

    #[test]
    fn target_round_trips_through_text() {
        for t in [Target::Auto, Target::Exact(2560, 1440)] {
            assert_eq!(Target::parse(&t.key()), Some(t));
        }
        assert_eq!(Target::parse(""), Some(Target::Auto));
        assert_eq!(Target::parse("auto"), Some(Target::Auto));
        assert_eq!(
            Target::parse(" 3840X2160 "),
            Some(Target::Exact(3840, 2160))
        );
        assert_eq!(Target::parse("nonsense"), None);
        assert_eq!(Target::parse("12x12"), None);
    }

    /// The whole point of the default: a 4K screen gets 4K, a 1080p screen gets
    /// 1920×1080, with nothing typed in.
    #[test]
    fn auto_follows_the_screen() {
        with_screen(3840, 2160, || {
            assert_eq!(Target::Auto.box_text(), "3840x2160");
            assert_eq!(Target::Auto.box_for(1300, 736), (3840, 2160));
        });
        with_screen(1920, 1080, || {
            assert_eq!(Target::Auto.box_text(), "1920x1080");
            assert_eq!(Target::Auto.box_for(1300, 736), (1920, 1080));
        });
        with_screen(2560, 1440, || {
            assert_eq!(Target::Auto.box_text(), "2560x1440");
        });
        // A portrait pin turns the box with it.
        with_screen(1920, 1080, || {
            assert_eq!(Target::Auto.box_for(736, 1300), (1080, 1920));
        });
        // A typed box ignores the screen completely.
        with_screen(3840, 2160, || {
            assert_eq!(Target::Exact(800, 600).box_for(736, 1300), (800, 600));
            assert_eq!(Target::Exact(800, 600).box_text(), "800x600");
        });
    }

    #[test]
    fn set_screen_reports_a_change_only_when_it_is_one() {
        with_screen(3840, 2160, || {
            assert!(!set_screen(3840, 2160), "same screen is not a change");
            assert!(set_screen(1920, 1080));
            assert_eq!(screen(), (1920, 1080));
        });
    }

    #[test]
    fn scale_is_the_smallest_one_that_reaches_the_target() {
        with_screen(1920, 1080, || {
            // 736x1300 portrait needs 1080/736 = 1.47x, so 2x is enough
            assert_eq!(auto_scale(Target::Auto, 736, 1300), 2);
            // 474x840 needs 1080/474 = 2.28x
            assert_eq!(auto_scale(Target::Auto, 474, 840), 3);
            // already huge: still upscales (2x is the floor), and fit_size
            // brings the result back to the box afterwards
            assert_eq!(auto_scale(Target::Auto, 6000, 4000), 2);
            assert_eq!(auto_scale(Target::Auto, 9000, 100), 2);
            // a target nothing reaches still gets 4x
            assert_eq!(auto_scale(Target::Exact(9999, 9999), 200, 200), 4);
        });
        // A 4K screen asks for more. The box turns with a portrait image, so a
        // 736×1300 pin is measured against 2160×3840 and needs 2.9x.
        with_screen(3840, 2160, || {
            assert_eq!(auto_scale(Target::Auto, 736, 1300), 3);
            // 1280x720 stays landscape, against 3840×2160, and needs 3x
            assert_eq!(auto_scale(Target::Auto, 1280, 720), 3);
            // and a big one still only reaches 2x
            assert_eq!(auto_scale(Target::Auto, 1920, 1080), 2);
        });
    }

    #[test]
    fn fit_size_only_shrinks_and_stays_even() {
        with_screen(1920, 1080, || {
            assert_eq!(fit_size(Target::Auto, 1920, 1080), None);
            assert_eq!(fit_size(Target::Auto, 2944, 5200), Some((1080, 1908)));
            assert_eq!(
                fit_size(Target::Exact(1280, 720), 3840, 2160),
                Some((1280, 720))
            );
            let (w, h) = fit_size(Target::Auto, 4001, 2001).unwrap();
            assert_eq!(w % 2, 0);
            assert_eq!(h % 2, 0);
            assert!(w <= 1920 && h <= 1080);
        });
        // On a 4K screen the same download is fitted to 4K instead. The box
        // turns with the portrait image, so it is 2160×3840.
        with_screen(3840, 2160, || {
            assert_eq!(fit_size(Target::Auto, 2944, 5200), Some((2160, 3814)));
            assert_eq!(fit_size(Target::Auto, 1920, 1080), None);
        });
    }
}
