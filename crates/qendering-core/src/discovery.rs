//! Asset discovery and `.ytd` ↔ `.ydd` pairing.
//!
//! - [`discover_ytd_base_files`] finds the base-variant clothing textures
//!   (`*_diff_NNN_a*.ytd`) under an input tree.
//! - [`discover_ydr_objects`] finds standalone world objects (`*.ydr`).
//! - [`find_ydd_for_ytd`] locates the drawable (`.ydd`) that goes with a
//!   texture, mirroring the original pairing heuristics.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use walkdir::WalkDir;

/// Base-variant texture files: the `_a` variant (optionally with a race
/// suffix like `_uni`/`_whi`). One per drawable.
static BASE_YTD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)_diff_\d+_a(?:_[a-z]+)?\.ytd$").unwrap());

/// Prefix + drawable id from a DLC/custom `.ytd` (has a `^` separator).
static YTD_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?P<prefix>.+?\^[a-z_]+)_diff_(?P<drawable>\d+)_[a-z](?:_[a-z]+)?\.ytd$")
        .unwrap()
});

/// Prefix + drawable id from a base-game `.ytd` (no `^`).
static BASE_GAME_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?P<prefix>[a-z_]+)_diff_(?P<drawable>\d+)_[a-z](?:_[a-z]+)?\.ytd$").unwrap()
});

/// Minimum `.ydd` size; smaller files are stub/placeholder drawables with no
/// real mesh and are skipped during pairing.
const MIN_YDD_SIZE: u64 = 1024;

/// Preferred `.ydd` suffixes, best first.
const YDD_SUFFIX_PREFERENCE: [&str; 2] = ["_u", "_r"];

fn file_name_lower(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// Should this directory be skipped during discovery? (`[replacements]`)
fn is_skipped_dir(name: &str) -> bool {
    name.eq_ignore_ascii_case("[replacements]")
}

/// Recursively find base-variant clothing `.ytd` files under `input_dir`.
pub fn discover_ytd_base_files(input_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let walker = WalkDir::new(input_dir).into_iter().filter_entry(|e| {
        !(e.file_type().is_dir()
            && e.depth() > 0
            && is_skipped_dir(&e.file_name().to_string_lossy()))
    });
    for entry in walker.filter_map(Result::ok) {
        if entry.file_type().is_file() {
            let name = entry.file_name().to_string_lossy();
            if BASE_YTD_RE.is_match(&name) {
                out.push(entry.into_path());
            }
        }
    }
    out.sort();
    out
}

/// Recursively find standalone object drawables (`.ydr`) under `input_dir`.
pub fn discover_ydr_objects(input_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in WalkDir::new(input_dir).into_iter().filter_map(Result::ok) {
        if entry.file_type().is_file() && file_name_lower(entry.path()).ends_with(".ydr") {
            out.push(entry.into_path());
        }
    }
    out.sort();
    out
}

/// First-level subdirectory of `input_dir` that `file_path` lives under
/// (the "resource pack" name), or `None` if not under `input_dir`.
pub fn resource_pack_of(file_path: &Path, input_dir: &Path) -> Option<String> {
    let rel = file_path.strip_prefix(input_dir).ok()?;
    let first = rel.components().next()?;
    // Only count it as a pack if there's at least one more component (the file).
    if rel.components().count() < 2 {
        return None;
    }
    Some(first.as_os_str().to_string_lossy().to_string())
}

fn suffix_rank(path: &Path) -> usize {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    for (rank, suffix) in YDD_SUFFIX_PREFERENCE.iter().enumerate() {
        if stem.ends_with(suffix) {
            return rank;
        }
    }
    YDD_SUFFIX_PREFERENCE.len()
}

/// Pick the best `.ydd` from candidates, preferring `_u` then `_r`.
fn rank_and_pick(mut candidates: Vec<PathBuf>) -> Option<PathBuf> {
    candidates.sort_by_key(|p| (suffix_rank(p), p.clone()));
    candidates.into_iter().next()
}

/// Scan a single directory for `.ydd` files matching `ydd_prefix`
/// (`{prefix}_{drawable}_`), skipping stubs below `min_size`.
fn scan_dir_for_ydd(directory: &Path, ydd_prefix: &str, min_size: u64) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    // Also match an unsuffixed prop file: prefix "p_head_000_" -> "p_head_000.ydd".
    let exact_nosuffix = format!("{}.ydd", ydd_prefix.trim_end_matches('_'));
    let Ok(entries) = std::fs::read_dir(directory) else {
        return candidates;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name_lower = file_name_lower(&path);
        if !name_lower.ends_with(".ydd") {
            continue;
        }
        if name_lower.starts_with(ydd_prefix) || name_lower == exact_nosuffix {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if size < min_size {
                continue;
            }
            candidates.push(path);
        }
    }
    candidates
}

/// Find the `.ydd` drawable that corresponds to a `.ytd` texture.
///
/// Searches the same directory, then the parent, then sibling directories
/// (to handle centralized `textures/` layouts). Prefers `_u`, then `_r`.
pub fn find_ydd_for_ytd(ytd_path: &Path) -> Option<PathBuf> {
    let filename = ytd_path.file_name()?.to_string_lossy().to_string();

    let caps = YTD_PREFIX_RE
        .captures(&filename)
        .or_else(|| BASE_GAME_PREFIX_RE.captures(&filename))?;
    let prefix = &caps["prefix"];
    let drawable = &caps["drawable"];
    let ydd_prefix = format!("{}_{}_", prefix, drawable).to_lowercase();

    let directory = ytd_path.parent().unwrap_or_else(|| Path::new("."));

    // 1. Same directory.
    let candidates = scan_dir_for_ydd(directory, &ydd_prefix, MIN_YDD_SIZE);
    if !candidates.is_empty() {
        return rank_and_pick(candidates);
    }

    // 2. Parent directory.
    if let Some(parent) = directory.parent() {
        if parent != directory {
            let candidates = scan_dir_for_ydd(parent, &ydd_prefix, MIN_YDD_SIZE);
            if !candidates.is_empty() {
                return rank_and_pick(candidates);
            }

            // 3. Sibling directories.
            let mut sib_candidates = Vec::new();
            if let Ok(entries) = std::fs::read_dir(parent) {
                for entry in entries.filter_map(Result::ok) {
                    let p = entry.path();
                    if p.is_dir() && p != directory {
                        sib_candidates.extend(scan_dir_for_ydd(&p, &ydd_prefix, MIN_YDD_SIZE));
                    }
                }
            }
            if !sib_candidates.is_empty() {
                return rank_and_pick(sib_candidates);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn write_sized(path: &Path, bytes: usize) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(path).unwrap();
        f.write_all(&vec![b'x'; bytes]).unwrap();
    }

    #[test]
    fn discovers_only_base_variant_ytd() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_sized(&root.join("pack/m^accs_diff_000_a_uni.ytd"), 10);
        write_sized(&root.join("pack/m^accs_diff_000_b_uni.ytd"), 10); // not base
        write_sized(&root.join("pack/m^accs_diff_001_a_whi.ytd"), 10); // base (race suffix)
        write_sized(&root.join("pack/notes.txt"), 10);
        // [replacements] should be skipped entirely
        write_sized(&root.join("[replacements]/m^jbib_diff_000_a_uni.ytd"), 10);

        let found = discover_ytd_base_files(root);
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(found.len(), 2, "got {names:?}");
        assert!(names.iter().any(|n| n == "m^accs_diff_000_a_uni.ytd"));
        assert!(names.iter().any(|n| n == "m^accs_diff_001_a_whi.ytd"));
    }

    #[test]
    fn discovers_ydr_objects() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_sized(&root.join("sub/pr_table_01.ydr"), 10);
        write_sized(&root.join("sub/pr_chair_01.YDR"), 10); // case-insensitive
        write_sized(&root.join("sub/skip.ytd"), 10);
        let found = discover_ydr_objects(root);
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn resource_pack_extraction() {
        let input = Path::new("/in");
        let file = Path::new("/in/40_civ3/stream/x^accs_diff_000_a_uni.ytd");
        assert_eq!(resource_pack_of(file, input).as_deref(), Some("40_civ3"));
        // File directly in input has no pack.
        assert_eq!(resource_pack_of(Path::new("/in/foo.ytd"), input), None);
    }

    #[test]
    fn pairs_ydd_prefers_u_and_skips_stub() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("stream");
        let ytd = d.join("mp_f_freemode_01_civ^accs_diff_000_a_uni.ytd");
        write_sized(&ytd, 10);
        // Real meshes (>1KB) for _u and _r, plus a tiny stub variant.
        write_sized(&d.join("mp_f_freemode_01_civ^accs_000_u.ydd"), 2000);
        write_sized(&d.join("mp_f_freemode_01_civ^accs_000_r.ydd"), 2000);
        write_sized(&d.join("mp_f_freemode_01_civ^accs_000_s.ydd"), 100); // stub

        let picked = find_ydd_for_ytd(&ytd).unwrap();
        assert_eq!(
            picked.file_name().unwrap().to_string_lossy(),
            "mp_f_freemode_01_civ^accs_000_u.ydd"
        );
    }

    #[test]
    fn pairs_ydd_in_sibling_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ytd = root.join("textures/mp_f_freemode_01_civ^jbib_diff_002_a_uni.ytd");
        write_sized(&ytd, 10);
        // .ydd lives in a sibling dir, not next to the .ytd.
        write_sized(
            &root.join("F_JBIB/mp_f_freemode_01_civ^jbib_002_u.ydd"),
            2000,
        );
        let picked = find_ydd_for_ytd(&ytd).unwrap();
        assert_eq!(
            picked.file_name().unwrap().to_string_lossy(),
            "mp_f_freemode_01_civ^jbib_002_u.ydd"
        );
    }

    #[test]
    fn no_ydd_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let ytd = dir.path().join("mp_f_freemode_01_civ^accs_diff_000_a_uni.ytd");
        write_sized(&ytd, 10);
        // Only a stub present -> nothing usable.
        write_sized(
            &dir.path().join("mp_f_freemode_01_civ^accs_000_u.ydd"),
            100,
        );
        assert!(find_ydd_for_ytd(&ytd).is_none());
    }
}
