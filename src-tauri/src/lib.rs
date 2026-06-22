use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

/// Managed cancel flag, toggled by `cancel_render`, checked by the render loops.
struct CancelFlag(Arc<AtomicBool>);

use qendering_render::{RenderConfig, RenderItem};

use qendering_core::discovery::{discover_ydr_objects, discover_ytd_base_files, find_ydd_for_ytd};
use qendering_core::filename::parse_ytd_filename;
use qendering_core::rsc7;
use qendering_core::texture::{save_preview, OutputFormat, DEFAULT_CANVAS};
use qendering_core::ytd::{parse_texture_dictionary, select_diffuse_texture};

// ---------------------------------------------------------------------------
// scan
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone)]
struct DlcCount {
    name: String,
    items: u32,
}

#[derive(Serialize, Clone)]
struct ScanResult {
    clothing_total: u32,
    dlcs: Vec<DlcCount>,
    objects: u32,
}

#[tauri::command]
fn scan(input_dir: String) -> ScanResult {
    let root = Path::new(&input_dir);

    let ytds = discover_ytd_base_files(root);
    let mut by_dlc: BTreeMap<String, u32> = BTreeMap::new();
    let mut clothing_total = 0u32;
    for y in &ytds {
        if let Some(info) = parse_ytd_filename(&y.to_string_lossy()) {
            *by_dlc.entry(info.dlc_name).or_insert(0) += 1;
            clothing_total += 1;
        }
    }
    let dlcs = by_dlc
        .into_iter()
        .map(|(name, items)| DlcCount { name, items })
        .collect();

    let objects = discover_ydr_objects(root).len() as u32;

    ScanResult {
        clothing_total,
        dlcs,
        objects,
    }
}

// ---------------------------------------------------------------------------
// render (flat clothing-texture pipeline, pure qendering-core)
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone)]
struct StartMsg {
    total: u32,
}

#[derive(Serialize, Clone)]
struct Progress {
    current: u32,
    total: u32,
    file: String,
    ok: bool,
}

#[derive(Serialize, Clone)]
struct DoneMsg {
    processed: u32,
    failed: u32,
}

/// Output basename for a texture: prefer the paired `.ydd` drawable name,
/// else the `.ytd` stem; `^` -> `_`.
fn output_base_name(ytd: &Path) -> String {
    if let Some(ydd) = find_ydd_for_ytd(ytd) {
        if let Some(stem) = ydd.file_stem() {
            return stem.to_string_lossy().replace('^', "_");
        }
    }
    ytd.file_stem()
        .map(|s| s.to_string_lossy().replace('^', "_"))
        .unwrap_or_else(|| "texture".to_string())
}

/// `<output_dir>/[subfolder/]textures` — where rendered files are written.
fn output_tex_dir(output_dir: &str, subfolder: &str) -> PathBuf {
    let base = Path::new(output_dir);
    let base = if subfolder.is_empty() {
        base.to_path_buf()
    } else {
        base.join(subfolder)
    };
    base.join("textures")
}

fn render_one(ytd: &Path, out: &Path, fmt: OutputFormat) -> qendering_core::Result<()> {
    let res = rsc7::parse_file(ytd)?;
    let texs = parse_texture_dictionary(&res.virtual_data, &res.physical_data)?;
    let diff = select_diffuse_texture(&texs)
        .ok_or_else(|| qendering_core::Error::Parse("no diffuse texture found".into()))?;
    save_preview(diff, out, DEFAULT_CANVAS, fmt)
}

fn run_flat(
    app: &AppHandle,
    input_dir: &str,
    output_dir: &str,
    format: &str,
    subfolder: &str,
    cancel: &AtomicBool,
) {
    let root = Path::new(input_dir);
    let fmt = OutputFormat::parse(format).unwrap_or(OutputFormat::Webp);
    let ytds = discover_ytd_base_files(root);
    let total = ytds.len() as u32;

    let _ = app.emit("start", StartMsg { total });
    if total == 0 {
        let _ = app.emit("log", "No clothing .ytd files found under the input folder.");
    }

    let tex_dir = output_tex_dir(output_dir, subfolder);
    let mut seen: HashMap<String, u32> = HashMap::new();
    let mut processed = 0u32;
    let mut failed = 0u32;

    for (i, ytd) in ytds.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            let _ = app.emit("log", "Render stopped.");
            break;
        }
        let base = output_base_name(ytd);
        let n = seen.entry(base.clone()).or_insert(0);
        let name = if *n == 0 {
            base.clone()
        } else {
            format!("{base}_{}", *n + 1)
        };
        *n += 1;

        let file = format!("{name}.{}", fmt.ext());
        let out_path = tex_dir.join(&file);

        let ok = match render_one(ytd, &out_path, fmt) {
            Ok(()) => {
                processed += 1;
                true
            }
            Err(e) => {
                failed += 1;
                let _ = app.emit("log", format!("FAIL {}: {e}", ytd.display()));
                false
            }
        };

        let _ = app.emit(
            "progress",
            Progress {
                current: (i as u32) + 1,
                total,
                file,
                ok,
            },
        );
    }

    let _ = app.emit("done", DoneMsg { processed, failed });
}

// ---------------------------------------------------------------------------
// objects (3D render via the Blender bridge)
// ---------------------------------------------------------------------------

/// Locate `blender_render.py`: the bundled resource first, then walk up from
/// the executable and working directory (dev builds).
fn resolve_render_script(app: &AppHandle) -> Option<PathBuf> {
    if let Ok(p) = app
        .path()
        .resolve("python/blender_render.py", tauri::path::BaseDirectory::Resource)
    {
        if p.is_file() {
            return Some(p);
        }
    }

    let mut starts: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            starts.push(d.to_path_buf());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        starts.push(cwd);
    }
    for start in starts {
        let mut cur = Some(start);
        for _ in 0..8 {
            let Some(dir) = cur else { break };
            let cand = dir.join("python").join("blender_render.py");
            if cand.is_file() {
                return Some(cand);
            }
            cur = dir.parent().map(Path::to_path_buf);
        }
    }
    None
}

fn collect_ytd_files(root: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect_ytd_files(&p, out);
            } else if p
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.eq_ignore_ascii_case("ytd"))
                .unwrap_or(false)
            {
                out.push(p);
            }
        }
    }
}

fn sanitize_tex_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '^') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Temp dir holding the pack's pre-extracted DDS textures for object renders.
fn temp_obj_dds_dir() -> PathBuf {
    std::env::temp_dir().join(format!("qendering_obj_dds_{}", std::process::id()))
}

/// Remove this process's stale qendering temp dirs (pre-extracted DDS, preview
/// frames). Worker scratch dirs are cleaned by the worker on shutdown.
fn cleanup_temp_dirs() {
    let tmp = std::env::temp_dir();
    let Ok(rd) = std::fs::read_dir(&tmp) else { return };
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("qendering_obj_dds_")
            || name.starts_with("qendering_preview_")
            || name.starts_with("qendering_worker_")
        {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Extract every texture from the pack's `.ytd` files to DDS in a temp dir,
/// returning the file paths. Sollumz does not auto-apply external `.ytd`
/// textures on `.ydr` import, so the worker force-loads these by name.
fn extract_pack_dds(app: &AppHandle, input_root: &Path) -> Vec<String> {
    let mut ytds = Vec::new();
    collect_ytd_files(input_root, &mut ytds);
    if ytds.is_empty() {
        return Vec::new();
    }
    let dds_dir = temp_obj_dds_dir();
    let _ = std::fs::create_dir_all(&dds_dir);

    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for ytd in &ytds {
        let Ok(res) = rsc7::parse_file(ytd) else { continue };
        let Ok(texs) = parse_texture_dictionary(&res.virtual_data, &res.physical_data) else {
            continue;
        };
        for t in &texs {
            if t.raw_data.is_empty() {
                continue;
            }
            let safe = sanitize_tex_name(&t.name);
            if safe.is_empty() || !seen.insert(safe.to_lowercase()) {
                continue;
            }
            let Ok(bytes) = qendering_core::dds::build_dds(t) else { continue };
            let p = dds_dir.join(format!("{safe}.dds"));
            if std::fs::write(&p, bytes).is_ok() {
                out.push(p.to_string_lossy().to_string());
            }
        }
    }
    if !out.is_empty() {
        let _ = app.emit(
            "log",
            format!("Pre-extracted {} pack texture(s) for objects.", out.len()),
        );
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn run_objects(
    app: &AppHandle,
    input_dir: &str,
    output_dir: &str,
    format: &str,
    azimuth_deg: f64,
    elevation_deg: f64,
    animate: bool,
    subfolder: &str,
    cancel: Arc<AtomicBool>,
) {
    const GIF_FRAMES: u32 = 24;
    const GIF_TOTAL_MS: u32 = 2000;

    let root = Path::new(input_dir);
    let ydrs = discover_ydr_objects(root);
    let total = ydrs.len() as u32;

    let fmt = OutputFormat::parse(format).unwrap_or(OutputFormat::Webp);

    let _ = app.emit("start", StartMsg { total });
    if total == 0 {
        let _ = app.emit("log", "No .ydr objects found under the input folder.");
    }

    let Some(blender) = qendering_render::find_blender() else {
        let _ = app.emit(
            "log",
            "Blender not found. Install Blender 4.x with the Sollumz add-on to render objects.",
        );
        let _ = app.emit("done", DoneMsg { processed: 0, failed: 0 });
        return;
    };
    let Some(script) = resolve_render_script(app) else {
        let _ = app.emit("log", "Could not locate blender_render.py.");
        let _ = app.emit("done", DoneMsg { processed: 0, failed: 0 });
        return;
    };
    if total == 0 {
        let _ = app.emit("done", DoneMsg { processed: 0, failed: 0 });
        return;
    }

    // External-texture support: pre-extract the pack's .ytd textures to DDS so
    // the worker can force-load them (Sollumz doesn't auto-apply external
    // textures on .ydr import). Embedded-texture objects are unaffected.
    let pack_dds = extract_pack_dds(app, root);

    let config = RenderConfig {
        azimuth: Some(azimuth_deg),
        elevation: Some(elevation_deg),
        // Spins force PNG frames internally; only stills honor the format here.
        still_format: if animate {
            None
        } else {
            Some(fmt.blender_format().to_string())
        },
        ..Default::default()
    };
    let ext = if animate { "gif" } else { fmt.ext() };

    let tex_dir = output_tex_dir(output_dir, subfolder);
    let mut seen: HashMap<String, u32> = HashMap::new();
    let mut items: Vec<RenderItem> = Vec::with_capacity(ydrs.len());
    for ydr in &ydrs {
        let base = ydr
            .file_stem()
            .map(|s| s.to_string_lossy().replace('^', "_"))
            .unwrap_or_else(|| "object".to_string());
        let n = seen.entry(base.clone()).or_insert(0);
        let name = if *n == 0 {
            base.clone()
        } else {
            format!("{base}_{}", *n + 1)
        };
        *n += 1;
        let out = tex_dir.join(format!("{name}.{ext}"));
        let mut item = RenderItem::object(
            ydr.to_string_lossy().to_string(),
            pack_dds.clone(),
            out.to_string_lossy().to_string(),
        );
        if animate {
            item = item.with_frames(GIF_FRAMES);
        }
        items.push(item);
    }

    let app_cb = app.clone();
    let results = qendering_render::render(
        &blender,
        &script,
        items,
        0,
        config,
        move |rr, done, total| {
            let file = Path::new(&rr.output_path)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let _ = app_cb.emit(
                "progress",
                Progress {
                    current: done as u32,
                    total: total as u32,
                    file,
                    ok: rr.success,
                },
            );
            if !rr.success {
                if let Some(e) = &rr.error {
                    let _ = app_cb.emit("log", format!("FAIL {}: {e}", rr.output_path));
                }
            }
        },
        Arc::clone(&cancel),
    );

    if cancel.load(Ordering::Relaxed) {
        let _ = app.emit("log", "Render stopped.");
    }

    // Assemble spin frames into GIFs (animate) or count stills.
    let mut processed = 0u32;
    let mut failed = 0u32;
    for rr in &results {
        if animate {
            if rr.success && !rr.frames.is_empty() {
                let delay = GIF_TOTAL_MS / GIF_FRAMES.max(1);
                match qendering_core::texture::frames_to_gif(
                    &rr.frames,
                    Path::new(&rr.output_path),
                    256,
                    delay,
                ) {
                    Ok(()) => processed += 1,
                    Err(e) => {
                        failed += 1;
                        let _ = app.emit("log", format!("GIF assembly failed {}: {e}", rr.output_path));
                    }
                }
            } else {
                failed += 1;
            }
        } else if rr.success {
            processed += 1;
        } else {
            failed += 1;
        }
    }

    // Drop the pre-extracted pack DDS now that every worker is done with it.
    let _ = std::fs::remove_dir_all(temp_obj_dds_dir());

    let _ = app.emit("done", DoneMsg { processed, failed });
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn start_render(
    app: AppHandle,
    state: tauri::State<CancelFlag>,
    input_dir: String,
    output_dir: String,
    format: String,
    mode: String,
    azimuth_deg: f64,
    elevation_deg: f64,
    animate: bool,
    subfolder: String,
) {
    let cancel = state.0.clone();
    cancel.store(false, Ordering::Relaxed);
    std::thread::spawn(move || {
        if mode == "objects" {
            run_objects(
                &app,
                &input_dir,
                &output_dir,
                &format,
                azimuth_deg,
                elevation_deg,
                animate,
                &subfolder,
                cancel,
            );
        } else {
            run_flat(&app, &input_dir, &output_dir, &format, &subfolder, &cancel);
        }
    });
}

/// Request the running render to stop after the current item.
#[tauri::command]
fn cancel_render(state: tauri::State<CancelFlag>) {
    state.0.store(true, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// live preview
// ---------------------------------------------------------------------------

#[tauri::command]
fn read_image_data_url(path: String) -> Result<String, String> {
    use base64::Engine;
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let mime = match Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        _ => "image/webp",
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:{mime};base64,{b64}"))
}

/// List rendered output files (relative to `<output_dir>/textures/`, forward
/// slashes, sorted) for the gallery.
#[tauri::command]
fn list_outputs(output_dir: String) -> Vec<String> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, base, out);
            } else {
                let is_img = p
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_lowercase())
                    .map(|s| matches!(s.as_str(), "webp" | "png" | "jpg" | "jpeg" | "gif"))
                    .unwrap_or(false);
                if is_img {
                    if let Ok(rel) = p.strip_prefix(base) {
                        out.push(rel.to_string_lossy().replace('\\', "/"));
                    }
                }
            }
        }
    }
    let tex = Path::new(&output_dir).join("textures");
    let mut out = Vec::new();
    walk(&tex, &tex, &mut out);
    out.sort();
    out
}

/// Render the first object in the folder as a quick turntable (N azimuth
/// frames at a low resolution) for the live-rotate preview. Returns the frame
/// image paths in azimuth order; the UI scrubs through them with the slider.
#[tauri::command]
fn preview_turntable(
    app: AppHandle,
    input_dir: String,
    mode: String,
    elevation_deg: f64,
    frames: u32,
) -> Result<Vec<String>, String> {
    if mode != "objects" {
        return Err("Turntable preview is only available for objects.".into());
    }
    let root = Path::new(&input_dir);
    let ydrs = discover_ydr_objects(root);
    let first = ydrs
        .first()
        .ok_or_else(|| "No .ydr objects found in the folder.".to_string())?;

    let blender = qendering_render::find_blender()
        .ok_or_else(|| "Blender not found. Install Blender 4.x with Sollumz.".to_string())?;
    let script = resolve_render_script(&app)
        .ok_or_else(|| "Could not locate blender_render.py.".to_string())?;
    let pack_dds = extract_pack_dds(&app, root);

    let n = frames.clamp(8, 64);
    let preview_dir =
        std::env::temp_dir().join(format!("qendering_preview_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&preview_dir);
    let _ = std::fs::create_dir_all(&preview_dir);
    let out = preview_dir.join("preview.gif"); // unused stem; spin returns frames

    // Fast preview: low resolution + single sample.
    let config = RenderConfig {
        render_size: Some(512),
        taa_samples: Some(1),
        elevation: Some(elevation_deg),
        ..Default::default()
    };
    let item = RenderItem::object(
        first.to_string_lossy().to_string(),
        pack_dds,
        out.to_string_lossy().to_string(),
    )
    .with_frames(n);

    let results = qendering_render::render(
        &blender,
        &script,
        vec![item],
        1,
        config,
        |_, _, _| {},
        Arc::new(AtomicBool::new(false)),
    );
    let rr = results
        .into_iter()
        .next()
        .ok_or_else(|| "Render produced no result.".to_string())?;
    if !rr.success {
        return Err(rr.error.unwrap_or_else(|| "Render failed.".to_string()));
    }
    if rr.frames.is_empty() {
        return Err("No frames were produced.".to_string());
    }
    Ok(rr.frames)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Sweep any temp dirs left behind by a previous crash before we start.
    cleanup_temp_dirs();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(CancelFlag(Arc::new(AtomicBool::new(false))))
        .invoke_handler(tauri::generate_handler![
            scan,
            start_render,
            cancel_render,
            read_image_data_url,
            list_outputs,
            preview_turntable
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
