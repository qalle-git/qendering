//! Blender render bridge: drives a pool of persistent headless Blender
//! processes (each running `python/blender_render.py` in `--worker` mode) over
//! a JSON-line stdin/stdout protocol. The rendering itself stays in Python
//! because it runs inside Blender's embedded interpreter via Sollumz.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const ITEM_TIMEOUT: Duration = Duration::from_secs(120);
const READY_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_WORKER_CRASHES: u32 = 3;
const MAX_AUTO_WORKERS: usize = 8;

/// A unit of render work; serializes to the JSON line the Python worker reads
/// (`type` = `item` for clothing, `object` for a standalone `.ydr`).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum RenderItem {
    #[serde(rename = "item")]
    Clothing {
        ydd_path: String,
        dds_files: Vec<String>,
        output_path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        category: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        fallback_ydd: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        camera_elevation: Option<f64>,
    },
    #[serde(rename = "object")]
    Object {
        ydr_path: String,
        /// External `.ytd` textures pre-extracted to DDS (Sollumz does not
        /// auto-apply external textures on `.ydr` import).
        #[serde(skip_serializing_if = "Vec::is_empty")]
        dds_files: Vec<String>,
        output_path: String,
        /// When set, render this many frames spinning a full 360° (for GIF).
        #[serde(skip_serializing_if = "Option::is_none")]
        frames: Option<u32>,
    },
}

impl RenderItem {
    /// Build a clothing item.
    ///
    /// * `ydd_path` — drawable to import.
    /// * `dds_files` — pre-extracted DDS textures for the drawable.
    /// * `output_path` — where the worker writes the rendered image.
    pub fn clothing(
        ydd_path: impl Into<String>,
        dds_files: Vec<String>,
        output_path: impl Into<String>,
    ) -> Self {
        RenderItem::Clothing {
            ydd_path: ydd_path.into(),
            dds_files,
            output_path: output_path.into(),
            category: None,
            fallback_ydd: None,
            camera_elevation: None,
        }
    }

    /// Build a standalone object item.
    ///
    /// * `ydr_path` — object drawable to import.
    /// * `dds_files` — external `.ytd` textures pre-extracted to DDS (empty if
    ///   the object's textures are embedded).
    /// * `output_path` — where the worker writes the rendered image.
    pub fn object(
        ydr_path: impl Into<String>,
        dds_files: Vec<String>,
        output_path: impl Into<String>,
    ) -> Self {
        RenderItem::Object {
            ydr_path: ydr_path.into(),
            dds_files,
            output_path: output_path.into(),
            frames: None,
        }
    }

    /// Request a spinning render of `n` frames (objects only); the worker
    /// returns the frame image paths for GIF assembly.
    pub fn with_frames(mut self, n: u32) -> Self {
        if let RenderItem::Object { frames, .. } = &mut self {
            *frames = Some(n);
        }
        self
    }

    /// The image path this item renders to.
    pub fn output_path(&self) -> &str {
        match self {
            RenderItem::Clothing { output_path, .. } | RenderItem::Object { output_path, .. } => {
                output_path
            }
        }
    }

    fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// Render settings sent to each worker (as a `CONFIG:` line) before any items.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RenderConfig {
    /// Blender render resolution in pixels; `None` keeps the worker preset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub render_size: Option<u32>,
    /// TAA sample count; `None` keeps the worker preset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taa_samples: Option<u32>,
    /// Object camera azimuth in degrees; `None` keeps the worker default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub azimuth: Option<f64>,
    /// Object camera elevation in degrees; `None` keeps the worker default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elevation: Option<f64>,
}

impl RenderConfig {
    /// Whether any setting is present (and thus worth sending).
    fn is_empty(&self) -> bool {
        self.render_size.is_none()
            && self.taa_samples.is_none()
            && self.azimuth.is_none()
            && self.elevation.is_none()
    }

    fn to_line(&self) -> String {
        format!("CONFIG:{}", serde_json::to_string(self).unwrap_or_default())
    }
}

/// Outcome of rendering one item, parsed from the worker's `RESULT:` line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderResult {
    pub output_path: String,
    pub success: bool,
    #[serde(default)]
    pub error: Option<String>,
    /// Frame image paths for a spin render (empty for a single still).
    #[serde(default)]
    pub frames: Vec<String>,
}

impl RenderResult {
    fn failure(output_path: &str, error: impl Into<String>) -> Self {
        RenderResult {
            output_path: output_path.to_string(),
            success: false,
            error: Some(error.into()),
            frames: Vec::new(),
        }
    }
}

enum WorkerError {
    Crash,
    Timeout,
}

/// Locate the Blender executable, searching `PATH` then the known Windows
/// install directories (newest version first).
pub fn find_blender() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "blender.exe" } else { "blender" };

    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(exe);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    if cfg!(windows) {
        for ver in ["4.5", "4.4", "4.3", "4.2", "4.1", "4.0"] {
            let p = PathBuf::from(format!(
                "C:\\Program Files\\Blender Foundation\\Blender {ver}\\blender.exe"
            ));
            if p.is_file() {
                return Some(p);
            }
        }
    }

    None
}

/// A persistent Blender subprocess with a background stdout reader.
struct BlenderWorker {
    blender_path: PathBuf,
    script_path: PathBuf,
    config: RenderConfig,
    child: Option<Child>,
    rx: Option<Receiver<String>>,
}

impl BlenderWorker {
    fn new(blender_path: &Path, script_path: &Path, config: RenderConfig) -> Self {
        BlenderWorker {
            blender_path: blender_path.to_path_buf(),
            script_path: script_path.to_path_buf(),
            config,
            child: None,
            rx: None,
        }
    }

    /// Spawn the process, wait for `READY`, and push the config line.
    fn start(&mut self) -> bool {
        let mut cmd = Command::new(&self.blender_path);
        cmd.arg("-b")
            .arg("-P")
            .arg(&self.script_path)
            .arg("--")
            .arg("--worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(_) => return false,
        };

        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => return false,
        };
        let (tx, rx) = mpsc::channel::<String>();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        if let Some(stderr) = child.stderr.take() {
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for _ in reader.lines() {}
            });
        }

        self.child = Some(child);
        self.rx = Some(rx);

        self.wait_for_ready() && self.send_config()
    }

    fn wait_for_ready(&self) -> bool {
        let Some(rx) = self.rx.as_ref() else {
            return false;
        };
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            match rx.recv_timeout(remaining) {
                Ok(line) if line.trim() == "READY" => return true,
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
    }

    fn send_config(&mut self) -> bool {
        if self.config.is_empty() {
            return true;
        }
        let line = self.config.to_line();
        self.write_line(&line)
    }

    fn write_line(&mut self, line: &str) -> bool {
        let Some(child) = self.child.as_mut() else {
            return false;
        };
        let Some(stdin) = child.stdin.as_mut() else {
            return false;
        };
        stdin.write_all(line.as_bytes()).is_ok()
            && stdin.write_all(b"\n").is_ok()
            && stdin.flush().is_ok()
    }

    /// Send one item and wait for its `RESULT:` line.
    fn render_item(&mut self, item: &RenderItem) -> Result<RenderResult, WorkerError> {
        if !self.write_line(&item.to_line()) {
            return Err(WorkerError::Crash);
        }
        let rx = self.rx.as_ref().ok_or(WorkerError::Crash)?;
        let deadline = Instant::now() + ITEM_TIMEOUT;
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(WorkerError::Timeout);
            };
            match rx.recv_timeout(remaining) {
                Ok(line) => {
                    if let Some(payload) = line.strip_prefix("RESULT:") {
                        return Ok(serde_json::from_str(payload).unwrap_or_else(|e| {
                            RenderResult::failure(
                                item.output_path(),
                                format!("worker result JSON error: {e}"),
                            )
                        }));
                    }
                }
                Err(RecvTimeoutError::Timeout) => return Err(WorkerError::Timeout),
                Err(RecvTimeoutError::Disconnected) => return Err(WorkerError::Crash),
            }
        }
    }

    fn restart(&mut self) -> bool {
        self.kill();
        self.start()
    }

    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.rx = None;
    }

    fn shutdown(&mut self) {
        if let Some(child) = self.child.as_mut() {
            drop(child.stdin.take());
        }
        self.kill();
    }
}

/// Render `items` across a pool of persistent Blender workers.
///
/// * `blender_path` — the Blender executable (see [`find_blender`]).
/// * `script_path` — path to `blender_render.py`.
/// * `items` — work to render; one [`RenderResult`] is returned per item.
/// * `parallel` — worker count, or `0` to auto-pick from CPU cores
///   (capped at [`MAX_AUTO_WORKERS`]).
/// * `config` — render settings forwarded to every worker.
/// * `on_progress` — invoked once per finished item as
///   `(result, completed_count, total)`.
pub fn render<F>(
    blender_path: &Path,
    script_path: &Path,
    items: Vec<RenderItem>,
    parallel: usize,
    config: RenderConfig,
    on_progress: F,
) -> Vec<RenderResult>
where
    F: Fn(&RenderResult, usize, usize) + Send + Sync + 'static,
{
    let total = items.len();
    if total == 0 {
        return Vec::new();
    }
    let parallel = resolve_parallel(parallel).min(total);

    let queue: Arc<Mutex<VecDeque<RenderItem>>> = Arc::new(Mutex::new(items.into_iter().collect()));
    let results: Arc<Mutex<Vec<RenderResult>>> = Arc::new(Mutex::new(Vec::with_capacity(total)));
    let on_progress = Arc::new(on_progress);

    let record = {
        let results = Arc::clone(&results);
        let on_progress = Arc::clone(&on_progress);
        move |rr: RenderResult| {
            let done = {
                let mut guard = results.lock().unwrap();
                guard.push(rr.clone());
                guard.len()
            };
            on_progress(&rr, done, total);
        }
    };

    let mut handles = Vec::new();
    for _ in 0..parallel {
        let queue = Arc::clone(&queue);
        let blender_path = blender_path.to_path_buf();
        let script_path = script_path.to_path_buf();
        let config = config.clone();
        let record = record.clone();
        handles.push(thread::spawn(move || {
            worker_loop(&blender_path, &script_path, config, &queue, &record);
        }));
    }

    for h in handles {
        let _ = h.join();
    }

    drain_remaining_as_failures(&queue, &record);

    Arc::try_unwrap(results)
        .map(|m| m.into_inner().unwrap())
        .unwrap_or_else(|arc| arc.lock().unwrap().clone())
}

/// Resolve a requested worker count, auto-picking from CPU cores when `0`.
fn resolve_parallel(requested: usize) -> usize {
    if requested > 0 {
        return requested;
    }
    let cores = thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    cores.clamp(1, MAX_AUTO_WORKERS)
}

/// Pull items off the queue and render them, restarting on crash/timeout.
fn worker_loop(
    blender_path: &Path,
    script_path: &Path,
    config: RenderConfig,
    queue: &Arc<Mutex<VecDeque<RenderItem>>>,
    record: &impl Fn(RenderResult),
) {
    let mut worker = BlenderWorker::new(blender_path, script_path, config);
    if !worker.start() {
        return;
    }
    let mut crashes = 0u32;
    while let Some(item) = queue.lock().unwrap().pop_front() {
        match worker.render_item(&item) {
            Ok(rr) => record(rr),
            Err(WorkerError::Crash) => {
                crashes += 1;
                if crashes > MAX_WORKER_CRASHES {
                    record(RenderResult::failure(
                        item.output_path(),
                        "Blender worker crashed too many times",
                    ));
                    break;
                }
                queue.lock().unwrap().push_back(item);
                if !worker.restart() {
                    break;
                }
            }
            Err(WorkerError::Timeout) => {
                record(RenderResult::failure(
                    item.output_path(),
                    format!("render timed out after {}s", ITEM_TIMEOUT.as_secs()),
                ));
                if !worker.restart() {
                    break;
                }
            }
        }
    }
    worker.shutdown();
}

/// Fail any items still queued (e.g. every worker failed to start) so the
/// caller always gets one result per input.
fn drain_remaining_as_failures(
    queue: &Arc<Mutex<VecDeque<RenderItem>>>,
    record: &impl Fn(RenderResult),
) {
    while let Some(item) = queue.lock().unwrap().pop_front() {
        record(RenderResult::failure(
            item.output_path(),
            "no Blender worker available",
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_item_json_shape() {
        let item = RenderItem::object("C:/x/table.ydr", vec![], "C:/out/table.webp");
        let v: serde_json::Value = serde_json::from_str(&item.to_line()).unwrap();
        assert_eq!(v["type"], "object");
        assert_eq!(v["ydr_path"], "C:/x/table.ydr");
        assert_eq!(v["output_path"], "C:/out/table.webp");
        assert!(v.get("ydd_path").is_none());
        assert!(v.get("dds_files").is_none()); // empty -> omitted
    }

    #[test]
    fn clothing_item_json_shape() {
        let item = RenderItem::clothing(
            "C:/x/shirt.ydd",
            vec!["C:/x/shirt/diff.dds".into()],
            "C:/out/shirt.webp",
        );
        let v: serde_json::Value = serde_json::from_str(&item.to_line()).unwrap();
        assert_eq!(v["type"], "item");
        assert_eq!(v["ydd_path"], "C:/x/shirt.ydd");
        assert_eq!(v["dds_files"][0], "C:/x/shirt/diff.dds");
        assert_eq!(v["output_path"], "C:/out/shirt.webp");
        assert!(v.get("category").is_none());
        assert!(v.get("fallback_ydd").is_none());
    }

    #[test]
    fn config_line_shape() {
        let cfg = RenderConfig {
            render_size: Some(2048),
            taa_samples: Some(8),
            azimuth: Some(30.0),
            elevation: Some(20.0),
        };
        let line = cfg.to_line();
        assert!(line.starts_with("CONFIG:"));
        let v: serde_json::Value = serde_json::from_str(line.strip_prefix("CONFIG:").unwrap()).unwrap();
        assert_eq!(v["render_size"], 2048);
        assert_eq!(v["taa_samples"], 8);
        assert_eq!(v["azimuth"], 30.0);
        assert_eq!(v["elevation"], 20.0);
        assert!(RenderConfig::default().is_empty());
        // angle-only config is still non-empty (worth sending)
        assert!(!RenderConfig { azimuth: Some(45.0), ..Default::default() }.is_empty());
    }

    #[test]
    fn spin_item_and_frames_result() {
        let item = RenderItem::object("o.ydr", vec![], "o.gif").with_frames(24);
        let v: serde_json::Value = serde_json::from_str(&item.to_line()).unwrap();
        assert_eq!(v["frames"], 24);

        let payload = r#"{"output_path":"o.gif","success":true,"error":null,"frames":["f0.png","f1.png"]}"#;
        let rr: RenderResult = serde_json::from_str(payload).unwrap();
        assert_eq!(rr.frames.len(), 2);
        // a still result (no frames key) parses to an empty frames vec
        let still: RenderResult =
            serde_json::from_str(r#"{"output_path":"o.webp","success":true}"#).unwrap();
        assert!(still.frames.is_empty());
    }

    #[test]
    fn auto_parallel_is_bounded() {
        assert_eq!(resolve_parallel(3), 3);
        let auto = resolve_parallel(0);
        assert!((1..=MAX_AUTO_WORKERS).contains(&auto));
    }

    #[test]
    fn output_path_accessor() {
        assert_eq!(
            RenderItem::object("a.ydr", vec![], "o.webp").output_path(),
            "o.webp"
        );
    }

    #[test]
    fn result_parses_worker_line() {
        let payload = r#"{"output_path":"o.webp","success":true,"error":null}"#;
        let rr: RenderResult = serde_json::from_str(payload).unwrap();
        assert!(rr.success);
        assert_eq!(rr.output_path, "o.webp");
        assert!(rr.error.is_none());
    }

    #[test]
    fn empty_items_returns_empty() {
        let out = render(
            Path::new("blender"),
            Path::new("script.py"),
            vec![],
            4,
            RenderConfig::default(),
            |_, _, _| {},
        );
        assert!(out.is_empty());
    }

    #[test]
    fn finds_installed_blender() {
        let found = find_blender().expect("Blender should be found");
        assert!(found.is_file(), "found path should exist: {found:?}");
        let name = found.file_name().unwrap().to_string_lossy().to_lowercase();
        assert!(name.starts_with("blender"), "unexpected exe: {name}");
    }

    /// Real Rust -> Python -> Blender round trip for one object. Set
    /// QENDERING_TEST_YDR and QENDERING_TEST_SCRIPT to run; skipped in CI.
    #[test]
    fn real_object_round_trip() {
        let (Ok(ydr), Ok(script)) = (
            std::env::var("QENDERING_TEST_YDR"),
            std::env::var("QENDERING_TEST_SCRIPT"),
        ) else {
            return;
        };
        let Some(blender) = find_blender() else {
            return;
        };
        let out = std::env::temp_dir().join("qendering_obj_roundtrip.webp");
        let _ = std::fs::remove_file(&out);

        let dds_files: Vec<String> = std::env::var("QENDERING_TEST_DDS_DIR")
            .ok()
            .map(|d| {
                std::fs::read_dir(&d)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().map(|x| x == "dds").unwrap_or(false))
                    .map(|p| p.to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();

        let items = vec![RenderItem::object(ydr, dds_files, out.to_string_lossy().to_string())];
        let results = render(
            &blender,
            Path::new(&script),
            items,
            1,
            RenderConfig::default(),
            |_, _, _| {},
        );
        assert_eq!(results.len(), 1);
        assert!(results[0].success, "render failed: {:?}", results[0].error);
        assert!(out.is_file(), "no output produced");
        eprintln!(
            "rendered object -> {} ({} bytes)",
            out.display(),
            std::fs::metadata(&out).unwrap().len()
        );
    }
}
