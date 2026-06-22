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

// ---------------------------------------------------------------------------
// manifest (machine-readable list of what a render produced)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct PropEntry {
    /// Output basename without extension — also the CDN image name.
    name: String,
    /// Rendered file name (with extension), relative to the textures folder.
    file: String,
    /// Top-level source folder (pack/DLC) the model came from, if resolvable.
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
}

#[derive(Serialize)]
struct Manifest {
    /// `"clothing"` or `"objects"`.
    mode: String,
    /// Output format the files were written in (`webp` / `png` / `jpg` / `gif`).
    format: String,
    /// Number of entries in `props`.
    count: usize,
    props: Vec<PropEntry>,
}

/// `<output_dir>/[subfolder/]manifest.json` — sits next to the textures folder.
fn manifest_path(output_dir: &str, subfolder: &str) -> PathBuf {
    let base = Path::new(output_dir);
    let base = if subfolder.is_empty() {
        base.to_path_buf()
    } else {
        base.join(subfolder)
    };
    base.join("manifest.json")
}

/// Top-level folder (pack/DLC) a model sits under, relative to the input root.
fn source_label(model: &Path, root: &Path) -> Option<String> {
    let rel = model.strip_prefix(root).ok()?;
    let first = rel.components().next()?;
    Some(first.as_os_str().to_string_lossy().to_string())
}

/// Write `manifest.json` listing every successfully rendered prop, logging the
/// outcome to the UI.
fn write_manifest(app: &AppHandle, path: &Path, manifest: &Manifest) {
    let json = match serde_json::to_string_pretty(manifest) {
        Ok(s) => s,
        Err(e) => {
            let _ = app.emit("log", format!("Failed to serialize manifest: {e}"));
            return;
        }
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(path, json) {
        Ok(()) => {
            let _ = app.emit(
                "log",
                format!("Wrote {} ({} props).", path.display(), manifest.count),
            );
        }
        Err(e) => {
            let _ = app.emit("log", format!("Failed to write manifest.json: {e}"));
        }
    }
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
    let mut props: Vec<PropEntry> = Vec::new();

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
                props.push(PropEntry {
                    name: name.clone(),
                    file: file.clone(),
                    source: source_label(ytd, root),
                });
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

    write_manifest(
        app,
        &manifest_path(output_dir, subfolder),
        &Manifest {
            mode: "clothing".into(),
            format: fmt.ext().to_string(),
            count: props.len(),
            props,
        },
    );

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
            || name.starts_with("qendering_cloth_dds_")
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

/// Temp root for the per-item clothing DDS extracted for 3D clothing renders.
fn temp_cloth_dds_dir() -> PathBuf {
    std::env::temp_dir().join(format!("qendering_cloth_dds_{}", std::process::id()))
}

/// Extract one `.ytd`'s textures to DDS files in `dest`, returning their paths.
/// Used by 3D clothing rendering, where each piece carries its own texture
/// dictionary (unlike objects, which share a pre-extracted pack).
fn extract_ytd_to_dir(ytd: &Path, dest: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(res) = rsc7::parse_file(ytd) else { return out };
    let Ok(texs) = parse_texture_dictionary(&res.virtual_data, &res.physical_data) else {
        return out;
    };
    let _ = std::fs::create_dir_all(dest);
    let mut seen: HashSet<String> = HashSet::new();
    for t in &texs {
        if t.raw_data.is_empty() {
            continue;
        }
        let safe = sanitize_tex_name(&t.name);
        if safe.is_empty() || !seen.insert(safe.to_lowercase()) {
            continue;
        }
        let Ok(bytes) = qendering_core::dds::build_dds(t) else { continue };
        let p = dest.join(format!("{safe}.dds"));
        if std::fs::write(&p, bytes).is_ok() {
            out.push(p.to_string_lossy().to_string());
        }
    }
    out
}

/// 3D clothing render: import each paired `.ydd` drawable in Blender (applying
/// its `.ytd` textures) and render a front-facing still, instead of the flat
/// texture-extraction pipeline in [`run_flat`].
fn run_clothing_3d(
    app: &AppHandle,
    input_dir: &str,
    output_dir: &str,
    format: &str,
    subfolder: &str,
    cancel: Arc<AtomicBool>,
) {
    let root = Path::new(input_dir);
    let fmt = OutputFormat::parse(format).unwrap_or(OutputFormat::Webp);
    let ytds = discover_ytd_base_files(root);

    let Some(blender) = qendering_render::find_blender() else {
        let _ = app.emit("start", StartMsg { total: 0 });
        let _ = app.emit(
            "log",
            "Blender not found. Install Blender 4.x with the Sollumz add-on for 3D clothing.",
        );
        let _ = app.emit("done", DoneMsg { processed: 0, failed: 0 });
        return;
    };
    let Some(script) = resolve_render_script(app) else {
        let _ = app.emit("start", StartMsg { total: 0 });
        let _ = app.emit("log", "Could not locate blender_render.py.");
        let _ = app.emit("done", DoneMsg { processed: 0, failed: 0 });
        return;
    };

    let config = RenderConfig {
        still_format: Some(fmt.blender_format().to_string()),
        ..Default::default()
    };
    let ext = fmt.ext();
    let tex_dir = output_tex_dir(output_dir, subfolder);
    let dds_root = temp_cloth_dds_dir();
    let _ = std::fs::remove_dir_all(&dds_root);
    let _ = std::fs::create_dir_all(&dds_root);

    let mut seen: HashMap<String, u32> = HashMap::new();
    let mut items: Vec<RenderItem> = Vec::new();
    let mut source_by_file: HashMap<String, Option<String>> = HashMap::new();
    let mut skipped = 0u32;
    for (i, ytd) in ytds.iter().enumerate() {
        let Some(ydd) = find_ydd_for_ytd(ytd) else {
            skipped += 1;
            continue;
        };
        let base = output_base_name(ytd);
        let n = seen.entry(base.clone()).or_insert(0);
        let name = if *n == 0 {
            base.clone()
        } else {
            format!("{base}_{}", *n + 1)
        };
        *n += 1;
        let file = format!("{name}.{ext}");
        let dds = extract_ytd_to_dir(ytd, &dds_root.join(format!("i{i}")));
        let out = tex_dir.join(&file);
        source_by_file.insert(file.clone(), source_label(ytd, root));
        items.push(RenderItem::clothing(
            ydd.to_string_lossy().to_string(),
            dds,
            out.to_string_lossy().to_string(),
        ));
    }

    let total = items.len() as u32;
    let _ = app.emit("start", StartMsg { total });
    if skipped > 0 {
        let _ = app.emit(
            "log",
            format!("Skipped {skipped} clothing piece(s) with no paired .ydd drawable."),
        );
    }
    if total == 0 {
        let _ = std::fs::remove_dir_all(&dds_root);
        let _ = app.emit("done", DoneMsg { processed: 0, failed: 0 });
        return;
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

    let mut processed = 0u32;
    let mut failed = 0u32;
    let mut props: Vec<PropEntry> = Vec::new();
    for rr in &results {
        if rr.success {
            processed += 1;
            let out = Path::new(&rr.output_path);
            let file = out
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let name = out
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            props.push(PropEntry {
                source: source_by_file.get(&file).cloned().flatten(),
                name,
                file,
            });
        } else {
            failed += 1;
        }
    }

    write_manifest(
        app,
        &manifest_path(output_dir, subfolder),
        &Manifest {
            mode: "clothing".into(),
            format: ext.to_string(),
            count: props.len(),
            props,
        },
    );

    let _ = std::fs::remove_dir_all(&dds_root);
    let _ = app.emit("done", DoneMsg { processed, failed });
}

const OBJ_GIF_FRAMES: u32 = 24;
const OBJ_GIF_TOTAL_MS: u32 = 2000;

/// Render one group of object `.ydr` files into `tex_dir` with its own fresh
/// worker pool, returning `(processed, failed, manifest_entries)`. Progress is
/// reported against the whole run: `base_done` is the number of items finished
/// in earlier groups and `grand_total` the run-wide total.
#[allow(clippy::too_many_arguments)]
fn render_object_group(
    app: &AppHandle,
    blender: &Path,
    script: &Path,
    ydrs: &[PathBuf],
    root: &Path,
    tex_dir: &Path,
    ext: &str,
    animate: bool,
    config: &RenderConfig,
    pack_dds: &[String],
    cancel: &Arc<AtomicBool>,
    base_done: u32,
    grand_total: u32,
) -> (u32, u32, Vec<PropEntry>) {
    let mut seen: HashMap<String, u32> = HashMap::new();
    let mut items: Vec<RenderItem> = Vec::with_capacity(ydrs.len());
    // Output file name -> source pack, for the manifest (results only carry the
    // output path, so we map back from the file name).
    let mut source_by_file: HashMap<String, Option<String>> = HashMap::new();
    for ydr in ydrs {
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
        let file = format!("{name}.{ext}");
        source_by_file.insert(file.clone(), source_label(ydr, root));
        let out = tex_dir.join(&file);
        let mut item = RenderItem::object(
            ydr.to_string_lossy().to_string(),
            pack_dds.to_vec(),
            out.to_string_lossy().to_string(),
        );
        if animate {
            item = item.with_frames(OBJ_GIF_FRAMES);
        }
        items.push(item);
    }

    let app_cb = app.clone();
    let results = qendering_render::render(
        blender,
        script,
        items,
        0,
        config.clone(),
        move |rr, done, _total| {
            let file = Path::new(&rr.output_path)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let _ = app_cb.emit(
                "progress",
                Progress {
                    current: base_done + done as u32,
                    total: grand_total,
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
        Arc::clone(cancel),
    );

    let mut processed = 0u32;
    let mut failed = 0u32;
    let mut props: Vec<PropEntry> = Vec::new();
    for rr in &results {
        let out = Path::new(&rr.output_path);
        let file = out
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let name = out
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut ok = false;
        if animate {
            if rr.success && !rr.frames.is_empty() {
                let delay = OBJ_GIF_TOTAL_MS / OBJ_GIF_FRAMES.max(1);
                match qendering_core::texture::frames_to_gif(&rr.frames, out, 256, delay) {
                    Ok(()) => {
                        processed += 1;
                        ok = true;
                    }
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
            ok = true;
        } else {
            failed += 1;
        }
        if ok {
            props.push(PropEntry {
                source: source_by_file.get(&file).cloned().flatten(),
                name,
                file,
            });
        }
    }
    (processed, failed, props)
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
    batch: bool,
    cancel: Arc<AtomicBool>,
) {
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

    // Build the work groups. Batch mode renders each top-level pack folder in
    // isolation (its own fresh worker pool + subfolder + manifest), so one bad
    // pack can't crash-storm the others; otherwise it is one whole-input group.
    let groups: Vec<(String, Vec<PathBuf>)> = if batch {
        let mut grouped: Vec<(String, Vec<PathBuf>)> = Vec::new();
        for ydr in &ydrs {
            let pack = source_label(ydr, root).unwrap_or_else(|| "_loose".to_string());
            let rel = if subfolder.is_empty() {
                pack
            } else {
                format!("{subfolder}/{pack}")
            };
            match grouped.iter_mut().find(|(r, _)| *r == rel) {
                Some((_, v)) => v.push(ydr.clone()),
                None => grouped.push((rel, vec![ydr.clone()])),
            }
        }
        let _ = app.emit("log", format!("Per-pack batch: {} pack(s).", grouped.len()));
        grouped
    } else {
        vec![(subfolder.to_string(), ydrs.clone())]
    };

    let mut processed = 0u32;
    let mut failed = 0u32;
    let mut base_done = 0u32;
    for (rel, group) in &groups {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let tex_dir = output_tex_dir(output_dir, rel);
        let (p, f, props) = render_object_group(
            app, &blender, &script, group, root, &tex_dir, ext, animate, &config, &pack_dds,
            &cancel, base_done, total,
        );
        write_manifest(
            app,
            &manifest_path(output_dir, rel),
            &Manifest {
                mode: "objects".into(),
                format: ext.to_string(),
                count: props.len(),
                props,
            },
        );
        if batch {
            let _ = app.emit("log", format!("Pack {rel}: {p} ok, {f} failed."));
        }
        processed += p;
        failed += f;
        base_done += group.len() as u32;
    }

    if cancel.load(Ordering::Relaxed) {
        let _ = app.emit("log", "Render stopped.");
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
    batch: bool,
    clothing3d: bool,
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
                batch,
                cancel,
            );
        } else if clothing3d {
            run_clothing_3d(&app, &input_dir, &output_dir, &format, &subfolder, cancel);
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
