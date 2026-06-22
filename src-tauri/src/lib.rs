use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

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

fn render_one(ytd: &Path, out: &Path, fmt: OutputFormat) -> qendering_core::Result<()> {
    let res = rsc7::parse_file(ytd)?;
    let texs = parse_texture_dictionary(&res.virtual_data, &res.physical_data)?;
    let diff = select_diffuse_texture(&texs)
        .ok_or_else(|| qendering_core::Error::Parse("no diffuse texture found".into()))?;
    save_preview(diff, out, DEFAULT_CANVAS, fmt)
}

fn run_flat(app: &AppHandle, input_dir: &str, output_dir: &str, format: &str) {
    let root = Path::new(input_dir);
    let fmt = OutputFormat::parse(format).unwrap_or(OutputFormat::Webp);
    let ytds = discover_ytd_base_files(root);
    let total = ytds.len() as u32;

    let _ = app.emit("start", StartMsg { total });
    if total == 0 {
        let _ = app.emit("log", "No clothing .ytd files found under the input folder.");
    }

    let tex_dir = Path::new(output_dir).join("textures");
    let mut seen: HashMap<String, u32> = HashMap::new();
    let mut processed = 0u32;
    let mut failed = 0u32;

    for (i, ytd) in ytds.iter().enumerate() {
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

/// Extract every texture from the pack's `.ytd` files to DDS in a temp dir,
/// returning the file paths. Sollumz does not auto-apply external `.ytd`
/// textures on `.ydr` import, so the worker force-loads these by name.
fn extract_pack_dds(app: &AppHandle, input_root: &Path) -> Vec<String> {
    let mut ytds = Vec::new();
    collect_ytd_files(input_root, &mut ytds);
    if ytds.is_empty() {
        return Vec::new();
    }
    let dds_dir = std::env::temp_dir().join(format!("qendering_obj_dds_{}", std::process::id()));
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

fn run_objects(app: &AppHandle, input_dir: &str, output_dir: &str, format: &str) {
    let root = Path::new(input_dir);
    let ydrs = discover_ydr_objects(root);
    let total = ydrs.len() as u32;

    let _ = app.emit("start", StartMsg { total });
    if total == 0 {
        let _ = app.emit("log", "No .ydr objects found under the input folder.");
    }
    // Objects render through Blender, which writes WebP; other formats aren't
    // supported on this path yet.
    if !format.eq_ignore_ascii_case("webp") {
        let _ = app.emit(
            "log",
            "Object previews are written as WebP regardless of the selected format.",
        );
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

    let tex_dir = Path::new(output_dir).join("textures");
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
        let out = tex_dir.join(format!("{name}.webp"));
        items.push(RenderItem::object(
            ydr.to_string_lossy().to_string(),
            pack_dds.clone(),
            out.to_string_lossy().to_string(),
        ));
    }

    let app_cb = app.clone();
    let results = qendering_render::render(
        &blender,
        &script,
        items,
        0,
        RenderConfig::default(),
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
    );

    let processed = results.iter().filter(|r| r.success).count() as u32;
    let failed = results.len() as u32 - processed;
    let _ = app.emit("done", DoneMsg { processed, failed });
}

#[tauri::command]
fn start_render(app: AppHandle, input_dir: String, output_dir: String, format: String, mode: String) {
    std::thread::spawn(move || {
        if mode == "objects" {
            run_objects(&app, &input_dir, &output_dir, &format);
        } else {
            run_flat(&app, &input_dir, &output_dir, &format);
        }
    });
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
        _ => "image/webp",
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:{mime};base64,{b64}"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            scan,
            start_render,
            read_image_data_url
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
