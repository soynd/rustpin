//! Download → upscale → save queue, running on its own thread.
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::pinterest::Pin;
use crate::settings::{self, Settings, Target};
use crate::upscayl::{self, Install};

const UA: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0";
const CHUNK: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct Job {
    pub id: String,
    pub pin_id: String,
    pub title: String,
    pub orig_url: String,
    pub upscale: bool,
    pub scale: u32,
    pub model: String,
    /// queued → downloading → upscaling → saving → done | error
    pub status: String,
    pub error: String,
    pub progress: f32,
    pub downloaded: u64,
    pub total: u64,
    pub orig_path: String,
    pub output_path: String,
    pub started: Option<Instant>,
}

impl Job {
    fn new(pin: &Pin, upscale: bool) -> Job {
        Job {
            id: new_id(),
            pin_id: pin.id.clone(),
            title: pin.title.clone(),
            orig_url: pin.orig.clone(),
            upscale,
            scale: 4,
            model: String::new(),
            status: "queued".into(),
            error: String::new(),
            progress: 0.0,
            downloaded: 0,
            total: 0,
            orig_path: String::new(),
            output_path: String::new(),
            started: None,
        }
    }

    pub fn done(&self) -> bool {
        matches!(self.status.as_str(), "done" | "error")
    }
}

pub struct Engine {
    jobs: Arc<Mutex<Vec<Job>>>,
    tx: Sender<Job>,
    settings: Arc<RwLock<Settings>>,
    install: Arc<Mutex<Option<Install>>>,
    queue_len: Arc<Mutex<usize>>,
}

impl Engine {
    pub fn new() -> Engine {
        let settings = Arc::new(RwLock::new(settings::load()));
        let install = Arc::new(Mutex::new(upscayl::detect()));
        let jobs: Arc<Mutex<Vec<Job>>> = Arc::new(Mutex::new(Vec::new()));
        let queue_len = Arc::new(Mutex::new(0usize));
        let (tx, rx) = channel::<Job>();
        {
            let jobs = Arc::clone(&jobs);
            let settings = Arc::clone(&settings);
            let install = Arc::clone(&install);
            let queue_len = Arc::clone(&queue_len);
            std::thread::Builder::new()
                .name("rustpin-jobs".into())
                .spawn(move || worker(rx, jobs, settings, install, queue_len))
                .ok();
        }
        Engine { jobs, tx, settings, install, queue_len }
    }

    pub fn settings(&self) -> Settings {
        self.settings.read().map(|s| s.clone()).unwrap_or_default()
    }

    /// Change a setting, write it out immediately.
    pub fn update(&self, patch: impl FnOnce(&mut Settings)) -> Settings {
        let next = {
            let mut slot = match self.settings.write() {
                Ok(s) => s,
                Err(p) => p.into_inner(),
            };
            patch(&mut slot);
            slot.clone()
        };
        settings::save(&next);
        next
    }

    pub fn target(&self) -> Target {
        self.settings.read().map(|s| s.target).unwrap_or_default()
    }

    /// Short line for the status bar: does upscaling work here?
    pub fn upscayl_info(&self) -> String {
        let found = self.install.lock().map(|i| i.clone()).unwrap_or(None);
        match found {
            None => "Upscayl not found — install it to upscale".into(),
            Some(i) => {
                let name = i
                    .bin
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_else(|| i.bin.display().to_string());
                match i.models.as_ref() {
                    Some(m) => {
                        let model = upscayl::pick_model(m);
                        format!("Upscayl {name} · {model}")
                    }
                    None => format!("Upscayl {name} · no models folder"),
                }
            }
        }
    }

    pub fn has_upscayl(&self) -> bool {
        self.install
            .lock()
            .map(|i| i.as_ref().map(|i| i.models.is_some()).unwrap_or(false))
            .unwrap_or(false)
    }

    pub fn enqueue(&self, pin: &Pin, upscale: bool) -> String {
        let mut job = Job::new(pin, upscale);
        job.scale = settings::auto_scale(self.target(), pin.w, pin.h);
        if let Ok(mut list) = self.jobs.lock() {
            list.push(job.clone());
        }
        if let Ok(mut n) = self.queue_len.lock() {
            *n += 1;
        }
        let id = job.id.clone();
        let _ = self.tx.send(job);
        id
    }

    pub fn jobs(&self) -> Vec<Job> {
        self.jobs.lock().map(|j| j.clone()).unwrap_or_default()
    }

    pub fn pending(&self) -> usize {
        self.jobs
            .lock()
            .map(|j| j.iter().filter(|job| !job.done()).count())
            .unwrap_or(0)
    }
}

fn new_id() -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0) as u64;
    let mixed = stamp
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let hex = format!("{mixed:016x}");
    hex[..10].to_string()
}

fn patch(jobs: &Mutex<Vec<Job>>, id: &str, edit: impl FnOnce(&mut Job)) {
    if let Ok(mut list) = jobs.lock() {
        if let Some(job) = list.iter_mut().find(|j| j.id == id) {
            edit(job);
        }
    }
}

fn worker(
    rx: Receiver<Job>,
    jobs: Arc<Mutex<Vec<Job>>>,
    settings: Arc<RwLock<Settings>>,
    install: Arc<Mutex<Option<Install>>>,
    queue_len: Arc<Mutex<usize>>,
) {
    while let Ok(job) = rx.recv() {
        if let Ok(mut n) = queue_len.lock() {
            *n = n.saturating_sub(1);
        }
        patch(&jobs, &job.id, |j| {
            j.status = "downloading".into();
            j.started = Some(Instant::now());
        });
        if let Err(e) = process(&job, &jobs, &settings, &install) {
            patch(&jobs, &job.id, |j| {
                j.status = "error".into();
                j.error = e.chars().take(500).collect();
            });
        }
    }
}

fn current_settings(settings: &Arc<RwLock<Settings>>) -> Settings {
    settings.read().map(|s| s.clone()).unwrap_or_default()
}

fn process(
    job: &Job,
    jobs: &Arc<Mutex<Vec<Job>>>,
    settings: &Arc<RwLock<Settings>>,
    install: &Arc<Mutex<Option<Install>>>,
) -> Result<(), String> {
    let cfg = current_settings(settings);
    let lib = settings::ensure_library();

    let stem = file_stem(&job.title, &job.pin_id);
    let tmp = lib.join(format!(".rustpin-download-{}", job.id));
    download(&job.orig_url, &tmp, &job.id, jobs)?;

    let ext = image_ext(&tmp);
    let orig_path = settings::unique_path(&lib, &format!("{stem}_orig"), ext);
    fs::rename(&tmp, &orig_path).map_err(|e| format!("save original: {e}"))?;
    let orig_name = orig_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    patch(jobs, &job.id, |j| j.orig_path = orig_name);

    let (ow, oh) = image::image_dimensions(&orig_path).unwrap_or((0, 0));
    if !job.upscale {
        patch(jobs, &job.id, |j| {
            j.status = "done".into();
            j.progress = 1.0;
            j.output_path = j.orig_path.clone();
        });
        return Ok(());
    }

    let target = cfg.target;
    let scale = settings::auto_scale(target, ow, oh);
    patch(jobs, &job.id, |j| {
        j.scale = scale;
        j.status = "upscaling".into();
        j.progress = j.progress.max(0.6);
    });

    let Some(inst) = install.lock().map(|i| i.clone()).unwrap_or(None) else {
        return Err("Upscayl not found — install Upscayl first.".into());
    };

    let raw = lib.join(format!(".rustpin-upscale-{}.png", job.id));
    let model = upscayl::run(&inst, &orig_path, &raw, scale)?;
    patch(jobs, &job.id, |j| {
        j.model = model;
        j.status = "saving".into();
    });

    let (uw, uh) = image::image_dimensions(&raw).unwrap_or((0, 0));
    let stem_final = format!("{stem}_upscayl_{scale}x");
    let final_path = settings::unique_path(&lib, &stem_final, "png");
    if let Some((nw, nh)) = settings::fit_size(target, uw, uh) {
        resize_png(&raw, &final_path, nw, nh)?;
    } else {
        fs::rename(&raw, &final_path).map_err(|e| format!("save: {e}"))?;
    }
    let _ = fs::remove_file(&raw);

    if cfg.delete_orig {
        let _ = fs::remove_file(&orig_path);
        patch(jobs, &job.id, |j| j.orig_path.clear());
    }
    let out_name = final_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    patch(jobs, &job.id, |j| {
        j.status = "done".into();
        j.progress = 1.0;
        j.output_path = out_name;
    });
    Ok(())
}

fn download(
    url: &str,
    dest: &Path,
    id: &str,
    jobs: &Arc<Mutex<Vec<Job>>>,
) -> Result<(), String> {
    let resp = ureq::get(url)
        .timeout(std::time::Duration::from_secs(180))
        .set("User-Agent", UA)
        .set("Referer", "https://www.pinterest.com/")
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let total: u64 = resp
        .header("content-length")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    patch(jobs, id, |j| j.total = total);

    let mut file = fs::File::create(dest).map_err(|e| format!("cannot write: {e}"))?;
    let mut reader = resp.into_reader().take(CHUNK as u64 * 64);
    let mut buf = vec![0u8; CHUNK];
    let mut got = 0u64;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("download failed: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("cannot write: {e}"))?;
        got += n as u64;
        patch(jobs, id, |j| {
            j.downloaded = got;
            if total > 0 {
                j.progress = (0.6 * got as f32 / total as f32).min(0.6);
            }
        });
    }
    if got == 0 {
        return Err("Pinterest returned an empty image".into());
    }
    patch(jobs, id, |j| j.progress = j.progress.max(0.6));
    Ok(())
}

fn resize_png(src: &Path, dst: &Path, w: u32, h: u32) -> Result<(), String> {
    let img = image::open(src).map_err(|e| format!("read upscale result: {e}"))?;
    let out = img.resize_exact(w, h, image::imageops::FilterType::Lanczos3);
    out.save(dst).map_err(|e| format!("save: {e}"))
}

fn image_ext(path: &Path) -> &'static str {
    let Ok(head) = fs::read(path).map(|b| b[..12.min(b.len())].to_vec()) else { return "jpg" };
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png"
    } else if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        "webp"
    } else {
        "jpg"
    }
}

fn file_stem(title: &str, pin_id: &str) -> String {
    let mut clean = String::new();
    let mut dash = false;
    for c in title.chars() {
        let keep = c.is_ascii_alphanumeric() || c == '_' || c == '-';
        if keep {
            clean.push(c);
            dash = false;
        } else if !dash && !clean.is_empty() {
            clean.push('-');
            dash = true;
        }
    }
    while clean.ends_with('-') {
        clean.pop();
    }
    if clean.len() > 60 {
        clean.truncate(60);
        while clean.ends_with('-') {
            clean.pop();
        }
    }
    if clean.is_empty() {
        clean = "pin".into();
    }
    let id: String = pin_id.chars().take(40).collect();
    if id.is_empty() {
        clean
    } else {
        format!("{clean}_{id}")
    }
}

/// Full path of a saved file.
pub fn library_file(name: &str) -> PathBuf {
    settings::library_dir().join(name)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Real run: Pinterest → download → upscayl → resize → ~/Pictures/rustpin.
    /// Costs network + GPU and leaves one picture behind, so it is opt-in:
    /// `cargo test --release -- --ignored --nocapture`
    #[test]
    #[ignore = "uses the network, the GPU and the real picture library"]
    fn downloads_and_upscales_for_real() {
        let engine = Engine::new();
        for _ in 0..50 {
            if engine.has_upscayl() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(engine.has_upscayl(), "upscayl not detected");
        eprintln!("{}", engine.upscayl_info());

        let client = crate::pinterest::Client::new().expect("pinterest session");
        let (pins, bookmarks) =
            client.search("cozy wallpaper", &[]).expect("search");
        eprintln!("search: {} pins, {} bookmarks", pins.len(), bookmarks.len());
        assert!(!pins.is_empty());
        let pin = pins.first().expect("a pin");
        eprintln!(
            "pin {} {}x{} -> scale {}",
            pin.id,
            pin.w,
            pin.h,
            settings::auto_scale(engine.target(), pin.w, pin.h)
        );
        assert!(!pin.orig.is_empty());

        let id = engine.enqueue(pin, true);
        let deadline = Instant::now() + Duration::from_secs(900);
        let job = loop {
            let job = engine
                .jobs()
                .into_iter()
                .find(|j| j.id == id)
                .expect("job");
            eprintln!(
                "{} {:.0}% {} KB/{} KB {}",
                job.status,
                job.progress * 100.0,
                job.downloaded / 1024,
                job.total / 1024,
                job.error
            );
            if job.done() {
                break job;
            }
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_secs(2));
        };

        assert_eq!(job.status, "done", "{}", job.error);
        let path = library_file(&job.output_path);
        assert!(path.is_file(), "{} missing", path.display());
        let (w, h) = image::image_dimensions(&path).expect("readable png");
        eprintln!("saved {} -> {w}x{h}", path.display());
        assert_eq!(job.orig_path, "", "original should be deleted");
        let (bw, bh) = engine.target().box_for(w, h);
        assert!(w <= bw.max(h) && h <= bh.max(bh), "{w}x{h} outside target");
    }
}
