use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::Serialize;
use tauri::{AppHandle, Emitter};

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

#[tauri::command]
fn start_render(app: AppHandle, input_dir: String, output_dir: String, format: String, mode: String) {
    std::thread::spawn(move || {
        if mode == "objects" {
            let _ = app.emit(
                "log",
                "Object (.ydr) 3D rendering is not wired up yet — it arrives with the Blender render bridge.",
            );
            let _ = app.emit("done", DoneMsg { processed: 0, failed: 0 });
            return;
        }
        run_flat(&app, &input_dir, &output_dir, &format);
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
