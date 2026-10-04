//! Finding the Upscayl CLI and running it with the standard 4x model.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The only model rustpin uses.
pub const MODEL: &str = "upscayl-standard-4x";

const BIN_NAMES: [&str; 3] = ["upscayl-bin", "upscayl-ncnn", "realesrgan-ncnn-vulkan"];
const MODEL_SUBS: [&str; 3] = ["models", "resources/models", "upscayl/resources/models"];

#[derive(Clone, Debug)]
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
    let Ok(rd) = fs::read_dir(d) else { return false };
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
        (
            home.join(".local/share/flatpak/app/org.upscayl.Upscayl"),
            8,
        ),
        (
            PathBuf::from("/var/lib/flatpak/app/org.upscayl.Upscayl"),
            8,
        ),
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
            best = Some(Install { bin, models: models.clone() });
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
    let Ok(img) = image::open(path) else { return false };
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

/// Upscale `src` into `dst`. Returns the model that actually ran.
pub fn run(install: &Install, src: &Path, dst: &Path, scale: u32) -> Result<String, String> {
    let Some(models) = install.models.as_ref() else {
        return Err("Upscayl models folder not found — reinstall Upscayl.".into());
    };
    let model = pick_model(models);
    let param = models.join(format!("{model}.param"));
    let weights = models.join(format!("{model}.bin"));
    if !param.is_file() || !weights.is_file() {
        return Err(format!(
            "model '{model}' is incomplete in {} (need {model}.param and {model}.bin)",
            models.display()
        ));
    }

    let out = Command::new(&install.bin)
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
        .output()
        .map_err(|e| format!("cannot run {}: {e}", install.bin.display()))?;

    let mut log = String::from_utf8_lossy(&out.stdout).to_string();
    log.push_str(&String::from_utf8_lossy(&out.stderr));
    let low = log.to_lowercase();

    let mut failed = !out.status.success();
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
        let code = out
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| out.status.to_string());
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