//! Download → upscale → save queue, running on its own thread.
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::pinterest::Pin;
use crate::settings::{self, Settings, Target};
use crate::upscayl::{self, Install};

const UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0";
const CHUNK: usize = 64 * 1024;

/// Runaway guard for a server that never stops sending. 256 MiB is far above
/// any real wallpaper original, so this only ever trips on a broken response.
const MAX_DOWNLOAD: u64 = 256 * 1024 * 1024;

/// A scratch file in the library that deletes itself unless it is kept, so a
/// failed download or a failed upscale never leaves `.rustpin-*` litter behind.
struct TempFile {
    path: PathBuf,
    kept: bool,
}

impl TempFile {
    fn new(path: PathBuf) -> Self {
        Self { path, kept: false }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// The file is now a real library file and must survive.
    fn keep(&mut self) {
        self.kept = true;
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.kept {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// A shared "stop what you are doing" flag, checked by the download loop and by
/// the upscaler while it waits on the child process.
#[derive(Clone, Default)]
pub struct Cancel {
    flag: Arc<AtomicBool>,
}

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

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
    /// Something worth saying about a job that technically succeeded.
    pub note: String,
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
            note: String::new(),
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
    cancels: Arc<Mutex<std::collections::HashMap<String, Cancel>>>,
}

impl Engine {
    pub fn new() -> Engine {
        let settings = Arc::new(RwLock::new(settings::load()));
        let install = Arc::new(Mutex::new(upscayl::detect()));
        let jobs: Arc<Mutex<Vec<Job>>> = Arc::new(Mutex::new(Vec::new()));
        let queue_len = Arc::new(Mutex::new(0usize));
        let cancels = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let (tx, rx) = channel::<Job>();
        {
            let jobs = Arc::clone(&jobs);
            let settings = Arc::clone(&settings);
            let install = Arc::clone(&install);
            let queue_len = Arc::clone(&queue_len);
            let cancels = Arc::clone(&cancels);
            std::thread::Builder::new()
                .name("rustpin-jobs".into())
                .spawn(move || worker(rx, jobs, settings, install, queue_len, cancels))
                .ok();
        }
        Engine {
            jobs,
            tx,
            settings,
            install,
            queue_len,
            cancels,
        }
    }

    /// Ask a job to stop. It finishes whatever it cannot un-send — a chunk of
    /// a download, the kill of the upscaler — and cleans up after itself.
    pub fn cancel(&self, id: &str) {
        if let Ok(map) = self.cancels.lock() {
            if let Some(c) = map.get(id) {
                c.cancel();
            }
        }
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
                    Some(_) => {
                        let model = self.model_choice();
                        format!("Upscayl {name} · {model}")
                    }
                    None => format!("Upscayl {name} · no models folder"),
                }
            }
        }
    }

    /// Every model name the picker should offer: what is in the models folder
    /// now, plus the well-known Upscayl names so an install missing some of
    /// them still shows them. Known names come first so the list is stable.
    pub fn models(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for m in upscayl::KNOWN_MODELS {
            out.push(m.to_string());
        }
        if let Ok(found) = self.install.lock() {
            if let Some(dir) = found.as_ref().and_then(|i| i.models.as_ref()) {
                for m in upscayl::list_models(dir) {
                    if !out.contains(&m) {
                        out.push(m);
                    }
                }
            }
        }
        out.sort_by(|a, b| {
            // Standard 4x first: it is what "auto" would have picked.
            let rank = |m: &String| (m != upscayl::MODEL, m.clone());
            rank(a).cmp(&rank(b))
        });
        out
    }

    /// What the "Models" line in settings shows: the model a new upscale would
    /// use right now, already resolved against the models folder.
    pub fn model_choice(&self) -> String {
        let wanted = self.settings.read().map(|s| s.model.clone()).unwrap_or_default();
        let dir = self
            .install
            .lock()
            .ok()
            .and_then(|i| i.as_ref().and_then(|i| i.models.clone()));
        match dir {
            Some(d) => upscayl::resolve_model(&d, &wanted),
            None => {
                if wanted.is_empty() {
                    upscayl::MODEL.to_string()
                } else {
                    wanted
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

    /// Look for Upscayl again, so installing it while rustpin is open is
    /// noticed. Returns whether the answer changed.
    pub fn rediscover(&self) -> bool {
        let found = upscayl::detect();
        let mut slot = match self.install.lock() {
            Ok(slot) => slot,
            Err(poisoned) => poisoned.into_inner(),
        };
        let changed = *slot != found;
        *slot = found;
        changed
    }

    pub fn enqueue(&self, pin: &Pin, upscale: bool) -> String {
        let mut job = Job::new(pin, upscale);
        job.scale = settings::auto_scale(self.target(), pin.w, pin.h);
        if let Ok(mut list) = self.jobs.lock() {
            list.push(job.clone());
        }
        if let Ok(mut map) = self.cancels.lock() {
            map.insert(job.id.clone(), Cancel::new());
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
    cancels: Arc<Mutex<std::collections::HashMap<String, Cancel>>>,
) {
    while let Ok(job) = rx.recv() {
        if let Ok(mut n) = queue_len.lock() {
            *n = n.saturating_sub(1);
        }
        let cancel = cancels
            .lock()
            .ok()
            .and_then(|m| m.get(&job.id).cloned())
            .unwrap_or_default();
        patch(&jobs, &job.id, |j| {
            j.status = "downloading".into();
            j.started = Some(Instant::now());
        });
        if let Err(e) = process(&job, &jobs, &settings, &install, &cancel) {
            patch(&jobs, &job.id, |j| {
                j.status = if cancel.is_cancelled() {
                    "cancelled".into()
                } else {
                    "error".into()
                };
                j.error = e.chars().take(500).collect();
            });
        }
        if let Ok(mut map) = cancels.lock() {
            map.remove(&job.id);
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
    cancel: &Cancel,
) -> Result<(), String> {
    let cfg = current_settings(settings);
    let lib = settings::ensure_library();

    let stem = file_stem(&job.title, &job.pin_id);
    let mut tmp = TempFile::new(lib.join(format!(".rustpin-download-{}", job.id)));
    download(&job.orig_url, tmp.path(), &job.id, jobs, cancel)?;

    let ext = image_ext(tmp.path());
    let orig_path = settings::unique_path(&lib, &format!("{stem}_orig"), ext);
    fs::rename(tmp.path(), &orig_path).map_err(|e| format!("save original: {e}"))?;
    tmp.keep();
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

    let mut raw = TempFile::new(lib.join(format!(".rustpin-upscale-{}.png", job.id)));
    let model = upscayl::run(&inst, &orig_path, raw.path(), scale, &cfg.model, cancel)?;

    // upscayl-bin exits 0 on a picture it only half drew, so believe the pixels
    // rather than the exit code. If the result is not the source at `scale`,
    // throw it away and hand back the original: a wallpaper at its own size is
    // a disappointment, a wallpaper with half of it black is a broken file that
    // gets deleted the moment `delete_orig` runs.
    if let Err(why) = upscayl::result_is_sane(&orig_path, raw.path(), scale) {
        let note = format!("Upscaling failed ({why}) \u{2014} the original was kept.");
        patch(jobs, &job.id, |j| {
            j.model = model;
            j.note = note;
            j.status = "done".into();
            j.progress = 1.0;
            j.output_path = j.orig_path.clone();
        });
        return Ok(());
    }

    patch(jobs, &job.id, |j| {
        j.model = model;
        j.status = "saving".into();
    });

    let (uw, uh) = image::image_dimensions(raw.path()).unwrap_or((0, 0));
    let stem_final = format!("{stem}_upscayl_{scale}x");
    let final_path = settings::unique_path(&lib, &stem_final, "png");
    if let Some((nw, nh)) = settings::fit_size(target, uw, uh) {
        resize_png(raw.path(), &final_path, nw, nh)?;
    } else {
        fs::rename(raw.path(), &final_path).map_err(|e| format!("save: {e}"))?;
    }
    raw.keep();

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
    cancel: &Cancel,
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
    // No small cap here: a 4K original is routinely well over the megabytes
    // and cutting it off used to leave a truncated file that only failed much
    // later, inside Upscayl. MAX_DOWNLOAD is the only ceiling.
    let mut reader = resp.into_reader().take(MAX_DOWNLOAD);
    let mut buf = vec![0u8; CHUNK];
    let mut got = 0u64;
    loop {
        if cancel.is_cancelled() {
            return Err("Cancelled.".into());
        }
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
    // The reader can only ever run short because the size ceiling was hit:
    // complain here, where the cause is obvious, instead of letting a partial
    // JPEG masquerade as a good original.
    if got >= MAX_DOWNLOAD {
        return Err(format!(
            "image is bigger than the {} MiB ceiling — download stopped",
            MAX_DOWNLOAD / (1024 * 1024)
        ));
    }
    if total > 0 && got < total {
        return Err(format!(
            "connection dropped after {} of {} bytes",
            got / 1024,
            total / 1024
        ));
    }
    patch(jobs, id, |j| j.progress = j.progress.max(0.6));
    Ok(())
}

fn resize_png(src: &Path, dst: &Path, w: u32, h: u32) -> Result<(), String> {
    let img = image::open(src).map_err(|e| format!("read upscale result: {e}"))?;
    let out = img.resize_exact(w, h, image::imageops::FilterType::Lanczos3);
    out.save(dst).map_err(|e| format!("save: {e}"))?;
    // A disk that fills up half way through a write reports success for some
    // encoders and leaves a short file. Read it back before believing it.
    let (rw, rh) = image::image_dimensions(dst).unwrap_or((0, 0));
    if (rw, rh) != (w, h) {
        let _ = fs::remove_file(dst);
        return Err(format!("the saved picture came out {rw}x{rh}, not {w}x{h}"));
    }
    Ok(())
}

fn image_ext(path: &Path) -> &'static str {
    let Ok(head) = fs::read(path).map(|b| b[..12.min(b.len())].to_vec()) else {
        return "jpg";
    };
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png"
    } else if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        "webp"
    } else {
        "jpg"
    }
}

/// Keep only characters that are safe in a filename, collapsing every run of
/// anything else into a single `-`.
fn slug(text: &str, max: usize) -> String {
    let mut clean = String::new();
    let mut dash = false;
    for c in text.chars() {
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
    if clean.len() > max {
        clean.truncate(max);
        while clean.ends_with('-') {
            clean.pop();
        }
    }
    clean
}

fn file_stem(title: &str, pin_id: &str) -> String {
    // Both halves are slugged: the id comes from the API, and a `/` in it
    // would otherwise write the finished file outside the library folder.
    let mut clean = slug(title, 60);
    if clean.is_empty() {
        clean = "pin".into();
    }
    let id = slug(pin_id, 40);
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

    /// A one-shot HTTP server that sends `body` but promises `declared_len`
    /// bytes, which is how a download that dies half way looks from here.
    fn serve_once(body: Vec<u8>, declared_len: usize) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/wallpaper.jpg", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            if let Ok((mut sock, _)) = listener.accept() {
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {declared_len}\r\n\r\n"
                );
                let _ = sock.write_all(head.as_bytes());
                let _ = sock.write_all(&body);
                let _ = sock.flush();
                // Close the writing half and then keep draining, otherwise the
                // kernel resets the connection while the client still has
                // unread bytes buffered and it sees ECONNRESET instead of EOF.
                let _ = sock.shutdown(std::net::Shutdown::Write);
                let mut sink = [0u8; 4096];
                while sock.read(&mut sink).unwrap_or(0) > 0 {}
            }
        });
        url
    }

    fn fetch_to(url: &str, dest: &Path) -> Result<(), String> {
        let jobs: Arc<Mutex<Vec<Job>>> = Arc::new(Mutex::new(Vec::new()));
        download(url, dest, "test", &jobs, &Cancel::new())
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rustpin-test-{}", new_id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// A 4K original is bigger than 4 MiB all the time, and cutting it off used
    /// to leave a truncated file that only fell over later, inside Upscayl.
    #[test]
    fn a_large_original_downloads_whole() {
        let body = vec![0xABu8; 5 * 1024 * 1024];
        let url = serve_once(body.clone(), body.len());
        let dest = scratch("big.jpg");
        fetch_to(&url, &dest).expect("a 5 MiB original must not be cut short");
        assert_eq!(fs::metadata(&dest).unwrap().len() as usize, body.len());
    }

    /// The point of the size check: a promise that is not kept is an error
    /// here, not a corrupt file in the library. ureq catches this one itself;
    /// the explicit `got < total` check is the backstop for the paths it does
    /// not, and either message will do.
    #[test]
    fn a_download_that_stops_early_is_an_error() {
        let url = serve_once(vec![0xABu8; 1000], 500_000);
        let dest = scratch("short.jpg");
        let err = fetch_to(&url, &dest).expect_err("a short download must fail");
        assert!(
            err.contains("connection dropped") || err.contains("closed before"),
            "{err}"
        );
    }

    #[test]
    fn an_empty_response_is_an_error() {
        let url = serve_once(Vec::new(), 0);
        let dest = scratch("empty.jpg");
        assert!(fetch_to(&url, &dest).is_err());
    }

    #[test]
    fn a_whole_download_is_not_an_error() {
        let body = vec![0x5Au8; 4096];
        let url = serve_once(body.clone(), body.len());
        let dest = scratch("ok.jpg");
        fetch_to(&url, &dest).expect("an intact download must pass");
    }

    #[test]
    fn cancelling_stops_a_download_in_its_tracks() {
        let body = vec![0xABu8; 8 * 1024 * 1024];
        let url = serve_once(body.clone(), body.len());
        let dest = scratch("cancelled.jpg");
        let jobs: Arc<Mutex<Vec<Job>>> = Arc::new(Mutex::new(Vec::new()));
        let cancel = Cancel::new();
        cancel.cancel();
        let err = download(&url, &dest, "test", &jobs, &cancel)
            .expect_err("an already-cancelled download must not finish");
        assert_eq!(err, "Cancelled.");
        assert!(!dest.exists() || fs::metadata(&dest).unwrap().len() == 0);
    }

    #[test]
    fn file_names_cannot_escape_the_library() {
        // A `/` from the API must not turn the finished file into a path.
        assert!(!file_stem("../../etc/passwd", "../../evil").contains('/'));
        assert_eq!(slug("a/b\\c", 60), "a-b-c");
        assert_eq!(slug("...", 60), "");
        assert_eq!(slug("a  b", 60), "a-b");
        assert_eq!(slug(&"x".repeat(200), 60).len(), 60);
        // Non-ASCII is dropped, not carried into the filesystem.
        assert_eq!(slug("café ☕ time", 60), "caf-time");
        assert_eq!(file_stem("", ""), "pin");
    }

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
        let (pins, bookmarks) = client.search("cozy wallpaper", &[]).expect("search");
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
            let job = engine.jobs().into_iter().find(|j| j.id == id).expect("job");
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
