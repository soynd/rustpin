//! Finding the Upscayl CLI and running it with the standard 4x model.
use crate::jobs::Cancel;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// The model used when the setting is "auto".
pub const MODEL: &str = "upscayl-standard-4x";

/// The models Upscayl ships, for the settings picker. Shown even when they are
/// not on disk yet, so the choice can be made before the models are there; a
/// name that is missing from the models folder falls back to [`pick_model`].
pub const KNOWN_MODELS: [&str; 6] = [
    "upscayl-standard-4x",
    "upscayl-lite-4x",
    "upscayl-standard-2x",
    "upscayl-lite-2x",
    "realesrgan-x4plus",
    "ultramix-balanced-4x",
];

/// What a stored model name should resolve to right now.
///
/// An empty choice means "decide for me", and so does a choice whose files are
/// not in the models folder: picking a model that cannot load would fail every
/// single upscale, so an unavailable pick quietly falls back instead.
pub fn resolve_model(dir: &Path, choice: &str) -> String {
    let choice = choice.trim();
    if choice.is_empty() {
        return pick_model(dir);
    }
    if list_models(dir).iter().any(|m| m == choice) {
        return choice.to_string();
    }
    pick_model(dir)
}

const BIN_NAMES: [&str; 3] = ["upscayl-bin", "upscayl-ncnn", "realesrgan-ncnn-vulkan"];
const MODEL_SUBS: [&str; 3] = ["models", "resources/models", "upscayl/resources/models"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Install {
    pub bin: PathBuf,
    pub models: Option<PathBuf>,
}

fn is_exec(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// True if `d` holds `*.param` model files.
fn has_models(d: &Path) -> bool {
    let Ok(rd) = fs::read_dir(d) else {
        return false;
    };
    rd.flatten().any(|e| {
        e.path()
            .extension()
            .map(|x| x.eq_ignore_ascii_case("param"))
            .unwrap_or(false)
    })
}

/// The models folder next to a binary, following the usual install layouts.
pub fn models_near(bin: &Path) -> Option<PathBuf> {
    let canonical = fs::canonicalize(bin).unwrap_or_else(|_| bin.to_path_buf());
    let mut bases: Vec<PathBuf> = Vec::new();
    for start in [canonical, bin.to_path_buf()] {
        if let Some(parent) = start.parent() {
            bases.push(parent.to_path_buf());
            if let Some(grand) = parent.parent() {
                bases.push(grand.to_path_buf());
            }
        }
    }
    for base in bases {
        for sub in MODEL_SUBS {
            let cand = base.join(sub);
            let cand = fs::canonicalize(&cand).unwrap_or(cand);
            if has_models(&cand) {
                return Some(cand);
            }
        }
    }
    None
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, depth - 1, out);
        } else if BIN_NAMES
            .iter()
            .any(|n| path.file_name().map(|f| f == *n).unwrap_or(false))
            && is_exec(&path)
            && !out.contains(&path)
        {
            out.push(path);
        }
    }
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|c| c.is_file() && is_exec(c))
}

/// Best working Upscayl install: a binary that already found its models wins.
pub fn detect() -> Option<Install> {
    let home = crate::settings::home();
    let mut roots: Vec<(PathBuf, usize)> = vec![
        (PathBuf::from("/usr/share/upscayl"), 3),
        (PathBuf::from("/usr/local/share/upscayl"), 3),
        (PathBuf::from("/opt/Upscayl"), 3),
        (PathBuf::from("/opt/upscayl"), 3),
        (PathBuf::from("/snap/upscayl/current"), 4),
        (home.join(".local/share/flatpak/app/org.upscayl.Upscayl"), 8),
        (PathBuf::from("/var/lib/flatpak/app/org.upscayl.Upscayl"), 8),
        (home.join(".local/share/upscayl"), 3),
        (home.join(".local/share/Upscayl"), 3),
        (home.join("Upscayl"), 3),
        (home.join(".local/bin"), 1),
    ];
    if let Some(upscayl_dir) = std::env::var_os("UPSCAYL_DIR") {
        roots.insert(0, (PathBuf::from(upscayl_dir), 4));
    }

    let mut found: Vec<PathBuf> = Vec::new();
    let add = |p: PathBuf, found: &mut Vec<PathBuf>| {
        if p.is_file() && is_exec(&p) && !found.contains(&p) {
            found.push(p);
        }
    };
    for (root, depth) in &roots {
        if !root.is_dir() {
            continue;
        }
        for sub in ["bin", "resources/bin", "upscayl/resources/bin", "."] {
            for name in BIN_NAMES {
                let cand = if *sub == *"." {
                    root.join(name)
                } else {
                    root.join(sub).join(name)
                };
                add(cand, &mut found);
            }
        }
        walk(root, *depth, &mut found);
    }
    for name in BIN_NAMES {
        if let Some(p) = which(name) {
            add(p, &mut found);
        }
    }

    let mut best: Option<Install> = None;
    let mut best_score = -1i32;
    for bin in found {
        let models = models_near(&bin);
        let score = i32::from(models.is_some())
            + i32::from(
                bin.file_name()
                    .map(|f| f.to_string_lossy().starts_with("upscayl-bin"))
                    .unwrap_or(false),
            );
        if score > best_score {
            best = Some(Install {
                bin,
                models: models.clone(),
            });
            best_score = score;
        }
        if models.is_some() {
            break;
        }
    }
    best
}

pub fn list_models(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path
                .extension()
                .map(|x| x.eq_ignore_ascii_case("param"))
                .unwrap_or(false)
            {
                if let Some(stem) = path.file_stem() {
                    out.push(stem.to_string_lossy().to_string());
                }
            }
        }
    }
    out.sort();
    out
}

/// The standard 4x model when installed, otherwise whatever is there.
pub fn pick_model(dir: &Path) -> String {
    let avail = list_models(dir);
    if avail.iter().any(|m| m == MODEL) {
        return MODEL.to_string();
    }
    for want in [
        "upscayl-standard-4x",
        "realesrgan-x4plus",
        "upscayl-lite-4x",
        "ultramix-balanced-4x",
    ] {
        if avail.iter().any(|m| m == want) {
            return want.to_string();
        }
    }
    avail.first().cloned().unwrap_or_else(|| MODEL.to_string())
}

/// upscayl-bin happily exits 0 with a flat black picture when the model
/// didn't load, so check the result before calling it a success.
fn looks_blank(path: &Path) -> bool {
    let Ok(img) = image::open(path) else {
        return false;
    };
    let luma = img.to_luma8();
    if luma.width() < 8 || luma.height() < 8 {
        return true;
    }
    let (mut lo, mut hi) = (255u8, 0u8);
    for px in luma.pixels() {
        let v = px[0];
        lo = lo.min(v);
        hi = hi.max(v);
        if i32::from(hi) - i32::from(lo) > 2 {
            return false;
        }
    }
    true
}

/// Mean brightness of `bands` horizontal strips, sampled every fourth column.
///
/// Cheap enough to run on a 6000×4000 result, and enough resolution to see
/// which *part* of a picture went wrong.
fn row_means(img: &image::GrayImage, bands: u32) -> Vec<f32> {
    let (w, h) = img.dimensions();
    let raw = img.as_raw();
    let stride = w as usize;
    (0..bands.max(1))
        .map(|b| {
            let y0 = b * h / bands;
            let y1 = (((b + 1) * h) / bands).max(y0 + 1).min(h);
            let mut sum = 0f64;
            let mut n = 0f64;
            for y in y0 as usize..y1 as usize {
                let row = &raw[y * stride..y * stride + stride];
                for px in row.iter().step_by(4) {
                    sum += f64::from(*px);
                    n += 1.0;
                }
            }
            (sum / n.max(1.0)) as f32
        })
        .collect()
}

/// How much darker a strip has to get before it counts as lost.
const BAND_DROP: f32 = 40.0;
/// How much of the picture has to be lost in one unbroken run. A wallpaper can
/// legitimately have half its rows near-black, so the test is against the
/// source, not against an absolute brightness.
const BAND_RUN: f32 = 0.25;

/// Check the result really is the source, scaled.
///
/// upscayl-bin reports success and exits 0 even when the GPU gave up part way
/// through: a Vulkan `vkQueueSubmit failed` writes the tiles it managed and
/// leaves the rest black, and the tool still prints "Upscayled Successfully!".
/// A flat-image check cannot catch that, because the part that *did* run is
/// perfectly good — the picture is half real, half black.
///
/// Comparing the two does catch it: strips that went dark while the source was
/// not dark mean the result is not the source at 4x. Measured against the
/// source rather than an absolute brightness, so a genuinely dark wallpaper
/// still passes.
pub fn result_is_sane(src: &Path, dst: &Path, scale: u32) -> Result<(), String> {
    let (sw, sh) = image::image_dimensions(src).unwrap_or((0, 0));
    let (dw, dh) = image::image_dimensions(dst).unwrap_or((0, 0));
    if sw == 0 || sh == 0 {
        return Err("the original has no size".into());
    }
    if dw != sw * scale || dh != sh * scale {
        return Err(format!(
            "upscayl produced {dw}x{dh}, expected {}x{}",
            sw * scale,
            sh * scale
        ));
    }

    let Ok(before) = image::open(src) else {
        return Err("the original could not be read".into());
    };
    let Ok(after) = image::open(dst) else {
        return Err("the result could not be read".into());
    };
    let bands = 64u32.min(sh);
    let want = row_means(&before.to_luma8(), bands);
    let got = row_means(&after.to_luma8(), bands);

    let mut run = 0u32;
    let mut worst = 0u32;
    for (b, g) in want.iter().zip(got.iter()) {
        if (g - b).abs() > BAND_DROP {
            run += 1;
            worst = worst.max(run);
        } else {
            run = 0;
        }
    }
    if worst as f32 / bands as f32 >= BAND_RUN {
        return Err(format!(
            "the GPU only finished part of it \u{2014} {}/{} strips came out wrong",
            worst, bands
        ));
    }
    Ok(())
}

/// The factors `-s` accepts: 2x, 3x and 4x. upscayl-bin resizes for anything
/// that is not the model's native factor, so a 3x job on a 4x model is a real
/// thing rather than a mistake; anything outside this range is not something
/// the tool can do.
pub fn model_scales(name: &str) -> &[u32] {
    let _ = name;
    &[2, 3, 4]
}

/// Upscale `src` into `dst`. Returns the model that actually ran.
///
/// `choice` is the user's model setting: empty means "pick for me", and a name
/// that is not installed falls back the same way (see [`resolve_model`]).
///
/// The child's output goes to files next to `dst` rather than to pipes: an
/// upscaler prints progress for minutes at a time and would fill a pipe nobody
/// is draining, which deadlocks. Watching for the cancel flag means polling
/// instead of a single blocking `output()`.
pub fn run(
    install: &Install,
    src: &Path,
    dst: &Path,
    scale: u32,
    choice: &str,
    cancel: &Cancel,
) -> Result<String, String> {
    let Some(models) = install.models.as_ref() else {
        return Err("Upscayl models folder not found — reinstall Upscayl.".into());
    };
    let model = resolve_model(models, choice);
    if !model_scales(&model).contains(&scale) {
        return Err(format!(
            "model '{model}' cannot upscale at {scale}x — it does {:?}",
            model_scales(&model)
        ));
    }
    let param = models.join(format!("{model}.param"));
    let weights = models.join(format!("{model}.bin"));
    if !param.is_file() || !weights.is_file() {
        return Err(format!(
            "model '{model}' is incomplete in {} (need {model}.param and {model}.bin)",
            models.display()
        ));
    }

    let out_log = dst.with_extension("log");
    let err_log = dst.with_extension("err");
    let mut child = {
        let out = fs::File::create(&out_log)
            .map_err(|e| format!("cannot write {}: {e}", out_log.display()))?;
        let err = fs::File::create(&err_log)
            .map_err(|e| format!("cannot write {}: {e}", err_log.display()))?;
        Command::new(&install.bin)
            .arg("-i")
            .arg(src)
            .arg("-o")
            .arg(dst)
            .arg("-n")
            .arg(&model)
            .arg("-s")
            .arg(scale.to_string())
            .arg("-m")
            .arg(models)
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .map_err(|e| format!("cannot run {}: {e}", install.bin.display()))?
    };

    let status = loop {
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(&out_log);
            let _ = fs::remove_file(&err_log);
            let _ = fs::remove_file(dst);
            return Err("Cancelled.".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(80)),
            Err(e) => {
                return Err(format!("cannot run {}: {e}", install.bin.display()));
            }
        }
    };

    let mut log = fs::read_to_string(&out_log).unwrap_or_default();
    log.push_str(&fs::read_to_string(&err_log).unwrap_or_default());
    let _ = fs::remove_file(&out_log);
    let _ = fs::remove_file(&err_log);
    let low = log.to_lowercase();

    let mut failed = !status.success();
    let tiny = fs::metadata(dst).map(|m| m.len()).unwrap_or(0) < 128;
    if tiny || !dst.is_file() {
        failed = true;
    }
    if low.contains("couldn't read the image") || (low.contains("fopen") && low.contains("failed"))
    {
        failed = true;
    }
    if !failed && looks_blank(dst) {
        failed = true;
        log.push_str(&format!(
            "\n[detector] the result is a flat image — model '{model}' probably failed to load (-m {})",
            models.display()
        ));
    }
    if failed {
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| status.to_string());
        let tail: String = log
            .trim()
            .chars()
            .rev()
            .take(1200)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        return Err(format!("upscayl failed (code {code}): {tail}"));
    }
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway folder holding just `files` as `*.param` stubs. A fresh
    /// directory per call, because two tests sharing one would see each
    /// other's models and pass for the wrong reason.
    fn model_dir(files: &[&str]) -> PathBuf {
        use std::sync::Mutex;
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("rustpin-models-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in files {
            std::fs::write(dir.join(format!("{f}.param")), b"x").unwrap();
        }
        dir
    }

    /// The setting is stored by name, so an empty one has to mean "decide for
    /// me" rather than "use a model called \"\"" — that is also what an older
    /// settings file decodes to.
    #[test]
    fn an_empty_choice_picks_the_standard_model() {
        let dir = model_dir(&["upscayl-standard-4x", "upscayl-lite-4x"]);
        assert_eq!(resolve_model(&dir, ""), "upscayl-standard-4x");
        assert_eq!(resolve_model(&dir, "   "), "upscayl-standard-4x");
    }

    /// The point of the picker: a model that is actually installed is used.
    #[test]
    fn a_chosen_model_is_the_one_that_runs() {
        let dir = model_dir(&["upscayl-standard-4x", "upscayl-lite-4x"]);
        assert_eq!(resolve_model(&dir, "upscayl-lite-4x"), "upscayl-lite-4x");
        // Surrounding whitespace from a hand-edited settings file is ignored.
        assert_eq!(
            resolve_model(&dir, " upscayl-lite-4x "),
            "upscayl-lite-4x"
        );
    }

    /// A model that was chosen but has since been deleted must not fail every
    /// upscale: it falls back to whatever is installed instead.
    #[test]
    fn a_missing_model_falls_back_instead_of_failing() {
        let dir = model_dir(&["upscayl-lite-4x"]);
        assert_eq!(
            resolve_model(&dir, "upscayl-standard-4x"),
            "upscayl-lite-4x",
            "a model that is not on disk must fall back, not break every upscale"
        );
        // Nothing installed at all still yields a usable name, so the error
        // that comes back is about the files rather than about an empty name.
        let empty = model_dir(&[]);
        assert_eq!(resolve_model(&empty, "upscayl-standard-4x"), MODEL);
    }

    #[test]
    fn an_installed_model_is_found_by_stem() {
        let dir = model_dir(&["upscayl-lite-4x", "upscayl-standard-4x"]);
        assert_eq!(
            list_models(&dir),
            vec!["upscayl-lite-4x", "upscayl-standard-4x"]
        );
    }
    use image::{ImageBuffer, Rgb};

    /// Build a test picture: `dark_below` makes everything under `split`
    /// black, which is what a GPU that gave up half way leaves behind.
    fn make(path: &Path, w: u32, h: u32, split: f32, dark_below: bool) {
        let mut img = ImageBuffer::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            let band = ((x * 37 + y * 17) % 200) as u8;
            let dark = dark_below && y as f32 > h as f32 * split;
            *px = if dark {
                Rgb([0, 0, 0])
            } else {
                Rgb([band, band / 2 + 40, 255 - band])
            };
        }
        img.save(path).unwrap();
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rustpin-upscayl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// The bug this exists for: upscayl-bin exits 0 on a result that is half
    /// real and half black, and a whole-image flatness check calls that fine.
    #[test]
    fn a_half_black_result_is_rejected() {
        let src = scratch("src.png");
        let dst = scratch("dst.png");
        make(&src, 400, 300, 0.1, false);
        // Only the top 10% survived.
        make(&dst, 800, 600, 0.1, true);
        let err = result_is_sane(&src, &dst, 2).expect_err("a half-black result must be caught");
        assert!(err.contains("part"), "{err}");
    }

    #[test]
    fn a_wholly_black_result_is_rejected() {
        let src = scratch("src2.png");
        let dst = scratch("dst2.png");
        make(&src, 400, 300, 0.1, false);
        make(&dst, 800, 600, 0.0, true);
        assert!(result_is_sane(&src, &dst, 2).is_err());
    }

    /// The false positive that matters: a genuinely dark wallpaper has dark
    /// strips too, so the test has to be against the source, not against an
    /// absolute brightness.
    #[test]
    fn a_genuinely_dark_picture_passes() {
        let src = scratch("src3.png");
        let dst = scratch("dst3.png");
        // Bottom 80% black in *both*, so the upscale is faithful.
        make(&src, 400, 300, 0.2, true);
        make(&dst, 800, 600, 0.2, true);
        result_is_sane(&src, &dst, 2).expect("a dark wallpaper is not a broken one");
    }

    #[test]
    fn a_flat_vertical_gradient_passes() {
        let src = scratch("src4.png");
        let dst = scratch("dst4.png");
        for (path, (w, h)) in [(&src, (400u32, 300u32)), (&dst, (800u32, 600u32))] {
            let mut img = ImageBuffer::new(w, h);
            for (_, y, px) in img.enumerate_pixels_mut() {
                let v = (y * 255 / h) as u8;
                *px = Rgb([v, v, v]);
            }
            img.save(path).unwrap();
        }
        result_is_sane(&src, &dst, 2).expect("a smooth gradient is fine");
    }

    #[test]
    fn a_result_at_the_wrong_size_is_rejected() {
        let src = scratch("src5.png");
        let dst = scratch("dst5.png");
        make(&src, 400, 300, 0.1, false);
        make(&dst, 800, 600, 0.1, false);
        // Asked for 4x, handed 2x.
        assert!(result_is_sane(&src, &dst, 4).is_err());
        result_is_sane(&src, &dst, 2).expect("2x of 400x300 is 800x600");
    }
}
