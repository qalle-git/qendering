//! Parse structured `.ytd` filenames into metadata components.
//!
//! Handles the same patterns as the original tool:
//!   1. Standard MP freemode: `mp_f_freemode_01_rhclothing^accs_diff_000_a_uni.ytd`
//!   2. Custom peds:          `strafe^accs_diff_001_a_uni.ytd`
//!   3. Base game:            `accs_diff_000_a_uni.ytd`
//!   4. Tattoo overlays:      `rushtattoo_000.ytd`
//!
//! Gender is derived from the directory path first (`[female]`/`[male]`), then
//! from the model prefix (`mp_f_`/`mp_m_`), defaulting to `unknown`.

use std::sync::LazyLock;

use regex::Regex;

static YTD_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?P<model>mp_[fm]_freemode_01)_(?P<dlcname>.+?)\^(?P<category>[a-z_]+)_diff_(?P<drawable>\d+)_(?P<variant>[a-z])(?:_(?P<suffix>[a-z]+))?\.ytd$",
    )
    .unwrap()
});

static CUSTOM_PED_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?P<model>[a-zA-Z0-9_]+)\^(?P<category>[a-z_]+)_diff_(?P<drawable>\d+)_(?P<variant>[a-z])(?:_(?P<suffix>[a-z]+))?\.ytd$",
    )
    .unwrap()
});

static BASE_GAME_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(?P<category>[a-z_]+)_diff_(?P<drawable>\d+)_(?P<variant>[a-z])(?:_(?P<suffix>[a-z]+))?\.ytd$",
    )
    .unwrap()
});

static TATTOO_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<prefix>[a-zA-Z][a-zA-Z0-9]*tattoo)_(?P<index>\d{3})\.ytd$").unwrap()
});

static BASE_GAME_DIR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^mp_(?P<gchar>[fm])_freemode_01(?:_(?P<suffix>.+))?$").unwrap());

/// GTA V "prop" component categories (hats, glasses, etc.).
pub const PROP_CATEGORIES: &[&str] = &["p_head", "p_eyes", "p_ears", "p_lwrist", "p_rwrist"];

/// Gender of an asset, derived from its path/model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gender {
    Female,
    Male,
    Unknown,
}

impl Gender {
    pub fn as_str(self) -> &'static str {
        match self {
            Gender::Female => "female",
            Gender::Male => "male",
            Gender::Unknown => "unknown",
        }
    }
}

/// Parsed metadata from a `.ytd` texture filename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YtdFileInfo {
    pub file_path: String,
    /// `"mp_f_freemode_01"`, a custom ped name, or `"base_game"`.
    pub model: String,
    /// DLC/collection name, or the model name for custom peds.
    pub dlc_name: String,
    pub gender: Gender,
    pub category: String,
    pub drawable_id: u32,
    pub variant: char,
    pub is_base: bool,
}

/// Parsed metadata from a tattoo `.ytd` filename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TattooFileInfo {
    pub file_path: String,
    pub prefix: String,
    pub index: u32,
}

pub fn is_prop_category(category: &str) -> bool {
    PROP_CATEGORIES.contains(&category)
}

/// Display name for a prop category, or the category unchanged.
pub fn prop_display_name(category: &str) -> &str {
    match category {
        "p_head" => "hat",
        "p_eyes" => "glass",
        "p_ears" => "ear",
        "p_lwrist" => "watch",
        "p_rwrist" => "bracelet",
        other => other,
    }
}

/// Human-friendly label for a (display-level) category.
pub fn category_display_name(category: &str) -> &str {
    match category {
        "accs" => "Accessories",
        "jbib" => "Tops",
        "lowr" => "Pants",
        "uppr" => "Undershirts",
        "feet" => "Shoes",
        "berd" => "Beards",
        "hair" => "Hair",
        "teef" => "Teeth",
        "decl" => "Decals",
        "task" => "Body Armor",
        "hand" => "Bags",
        "head" => "Heads",
        "hat" => "Hats",
        "glass" => "Glasses",
        "ear" => "Earrings",
        "watch" => "Watches",
        "bracelet" => "Bracelets",
        "tattoo" => "Tattoos",
        "overlay" => "Overlays",
        other => other,
    }
}

fn basename(path: &str) -> &str {
    let norm_end = path.rfind(['/', '\\']).map(|i| i + 1).unwrap_or(0);
    &path[norm_end..]
}

fn derive_gender(file_path: &str, model: &str) -> Gender {
    let normalized = file_path.replace('\\', "/").to_lowercase();
    if normalized.contains("[female]") || normalized.contains("/female/") {
        return Gender::Female;
    }
    if normalized.contains("[male]") || normalized.contains("/male/") {
        return Gender::Male;
    }
    if normalized.contains("mp_f_freemode_01") {
        return Gender::Female;
    }
    if normalized.contains("mp_m_freemode_01") {
        return Gender::Male;
    }
    if model.starts_with("mp_f_") {
        return Gender::Female;
    }
    if model.starts_with("mp_m_") {
        return Gender::Male;
    }
    Gender::Unknown
}

/// Canonical GTA V base-game collection casing (lowercase suffix -> exact name).
fn base_game_name_override(lower: &str) -> Option<&'static str> {
    Some(match lower {
        "female_apt01" => "Female_Apt01",
        "female_heist" => "Female_Heist",
        "female_freemode_halloween" => "Female_freemode_Halloween",
        "female_freemode_pilot" => "Female_freemode_Pilot",
        "female_freemode_beach" => "Female_freemode_beach",
        "female_freemode_business" => "Female_freemode_business",
        "female_freemode_business2" => "Female_freemode_business2",
        "female_freemode_hipster" => "Female_freemode_hipster",
        "female_freemode_independence" => "Female_freemode_independence",
        "female_freemode_mplts" => "Female_freemode_mpLTS",
        "female_freemode_valentines" => "Female_freemode_valentines",
        "female_xmas" => "Female_xmas",
        "female_xmas2" => "Female_xmas2",
        "male_apt01" => "Male_Apt01",
        "male_heist" => "Male_Heist",
        "male_freemode_halloween" => "Male_freemode_Halloween",
        "male_freemode_pilot" => "Male_freemode_Pilot",
        "male_freemode_beach" => "Male_freemode_beach",
        "male_freemode_business" => "Male_freemode_business",
        "male_freemode_business2" => "Male_freemode_business2",
        "male_freemode_hipster" => "Male_freemode_hipster",
        "male_freemode_independence" => "Male_freemode_independence",
        "male_freemode_mplts" => "Male_freemode_mpLTS",
        "male_freemode_valentines" => "Male_freemode_valentines",
        "male_xmas" => "Male_xmas",
        "male_xmas2" => "Male_xmas2",
        _ => return None,
    })
}

/// Derive `(dlc_name, gender)` for base-game files from the directory path.
fn derive_base_game_info(file_path: &str) -> (String, Gender) {
    let normalized = file_path.replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').collect();

    // Walk parent directories (skip the filename) looking for a ped directory.
    if parts.len() >= 2 {
        for part in parts[..parts.len() - 1].iter().rev() {
            if let Some(m) = BASE_GAME_DIR_RE.captures(part) {
                let gender = if &m["gchar"] == "f" || &m["gchar"] == "F" {
                    Gender::Female
                } else {
                    Gender::Male
                };
                match m.name("suffix").map(|s| s.as_str()) {
                    None => return ("base".to_string(), gender),
                    Some(mut suffix) => {
                        // Strip prop dir prefix: "p" -> base, "p_xxx" -> "xxx".
                        if suffix == "p" {
                            return ("base".to_string(), gender);
                        }
                        if let Some(rest) = suffix.strip_prefix("p_") {
                            suffix = rest;
                        }
                        let lower = suffix.to_lowercase();
                        let dlc_name = if let Some(canon) = base_game_name_override(&lower) {
                            canon.to_string()
                        } else if lower.starts_with("mp_") {
                            suffix.to_string()
                        } else {
                            let mut c = suffix.chars();
                            match c.next() {
                                Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
                                None => suffix.to_string(),
                            }
                        };
                        return (dlc_name, gender);
                    }
                }
            }
        }
    }

    ("base".to_string(), derive_gender(file_path, "base_game"))
}

fn first_char(s: &str) -> char {
    s.chars().next().unwrap_or('a')
}

/// Extract metadata from a `.ytd` filename, or `None` if it matches no pattern.
pub fn parse_ytd_filename(file_path: &str) -> Option<YtdFileInfo> {
    let filename = basename(file_path);

    if let Some(m) = YTD_PATTERN.captures(filename) {
        let model = m["model"].to_string();
        let category = m["category"].to_string();
        let mut dlc_name = m["dlcname"].to_string();
        // Prop files prefix the DLC name with "p_"; strip it.
        if is_prop_category(&category) {
            if let Some(rest) = dlc_name.strip_prefix("p_") {
                dlc_name = rest.to_string();
            }
        }
        let variant = first_char(&m["variant"]);
        let gender = derive_gender(file_path, &model);
        return Some(YtdFileInfo {
            file_path: file_path.to_string(),
            model,
            dlc_name,
            gender,
            category,
            drawable_id: m["drawable"].parse().ok()?,
            variant,
            is_base: variant == 'a',
        });
    }

    if let Some(m) = CUSTOM_PED_PATTERN.captures(filename) {
        let model = m["model"].to_string();
        let variant = first_char(&m["variant"]);
        let gender = derive_gender(file_path, &model);
        return Some(YtdFileInfo {
            file_path: file_path.to_string(),
            dlc_name: model.clone(),
            model,
            gender,
            category: m["category"].to_string(),
            drawable_id: m["drawable"].parse().ok()?,
            variant,
            is_base: variant == 'a',
        });
    }

    if let Some(m) = BASE_GAME_PATTERN.captures(filename) {
        let variant = first_char(&m["variant"]);
        let (dlc_name, gender) = derive_base_game_info(file_path);
        return Some(YtdFileInfo {
            file_path: file_path.to_string(),
            model: "base_game".to_string(),
            dlc_name,
            gender,
            category: m["category"].to_string(),
            drawable_id: m["drawable"].parse().ok()?,
            variant,
            is_base: variant == 'a',
        });
    }

    None
}

/// Extract metadata from a tattoo `.ytd` filename, or `None`.
pub fn parse_tattoo_filename(file_path: &str) -> Option<TattooFileInfo> {
    let filename = basename(file_path);
    let m = TATTOO_PATTERN.captures(filename)?;
    Some(TattooFileInfo {
        file_path: file_path.to_string(),
        prefix: m["prefix"].to_string(),
        index: m["index"].parse().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_freemode() {
        let info =
            parse_ytd_filename("mp_f_freemode_01_rhclothing^accs_diff_000_a_uni.ytd").unwrap();
        assert_eq!(info.model, "mp_f_freemode_01");
        assert_eq!(info.dlc_name, "rhclothing");
        assert_eq!(info.category, "accs");
        assert_eq!(info.drawable_id, 0);
        assert_eq!(info.variant, 'a');
        assert!(info.is_base);
        assert_eq!(info.gender, Gender::Female);
    }

    #[test]
    fn dlc_name_with_underscores_and_subdlc() {
        let info = parse_ytd_filename(
            "mp_f_freemode_01_mp_f_gunrunning_01^jbib_diff_012_b_uni.ytd",
        )
        .unwrap();
        assert_eq!(info.dlc_name, "mp_f_gunrunning_01");
        assert_eq!(info.category, "jbib");
        assert_eq!(info.drawable_id, 12);
        assert_eq!(info.variant, 'b');
        assert!(!info.is_base);
    }

    #[test]
    fn real_civ3_male_file() {
        let info =
            parse_ytd_filename("mp_m_freemode_01_mp_m_civ3^jbib_diff_007_a_uni.ytd").unwrap();
        assert_eq!(info.model, "mp_m_freemode_01");
        assert_eq!(info.dlc_name, "mp_m_civ3");
        assert_eq!(info.gender, Gender::Male);
        assert_eq!(info.category, "jbib");
    }

    #[test]
    fn prop_strips_p_prefix() {
        let info =
            parse_ytd_filename("mp_f_freemode_01_p_rhclothing^p_head_diff_000_a.ytd").unwrap();
        assert_eq!(info.category, "p_head");
        assert_eq!(info.dlc_name, "rhclothing");
        assert!(is_prop_category(&info.category));
        assert_eq!(prop_display_name(&info.category), "hat");
    }

    #[test]
    fn custom_ped() {
        let info = parse_ytd_filename("strafe^accs_diff_001_a_uni.ytd").unwrap();
        assert_eq!(info.model, "strafe");
        assert_eq!(info.dlc_name, "strafe");
        assert_eq!(info.gender, Gender::Unknown);
        assert_eq!(info.drawable_id, 1);
    }

    #[test]
    fn base_game_with_subpack_dir() {
        let info = parse_ytd_filename(
            "base/mp_f_freemode_01_female_freemode_beach/accs_diff_003_a_uni.ytd",
        )
        .unwrap();
        assert_eq!(info.model, "base_game");
        assert_eq!(info.dlc_name, "Female_freemode_beach");
        assert_eq!(info.gender, Gender::Female);
    }

    #[test]
    fn base_game_plain() {
        let info =
            parse_ytd_filename("base/mp_m_freemode_01/jbib_diff_005_a_uni.ytd").unwrap();
        assert_eq!(info.dlc_name, "base");
        assert_eq!(info.gender, Gender::Male);
    }

    #[test]
    fn gender_from_bracket_path() {
        let info = parse_ytd_filename("pack/[female]/accs/strafe^accs_diff_000_a_uni.ytd")
            .unwrap();
        assert_eq!(info.gender, Gender::Female);
    }

    #[test]
    fn windows_path_separators() {
        let info = parse_ytd_filename(
            r"C:\res\[clothing]\40_civ3\stream\mp_f_freemode_01_mp_f_civ3^lowr_diff_010_a_uni.ytd",
        )
        .unwrap();
        assert_eq!(info.category, "lowr");
        assert_eq!(info.drawable_id, 10);
    }

    #[test]
    fn tattoo() {
        let info = parse_tattoo_filename("rushtattoo_004.ytd").unwrap();
        assert_eq!(info.prefix, "rushtattoo");
        assert_eq!(info.index, 4);
    }

    #[test]
    fn non_matching_returns_none() {
        assert!(parse_ytd_filename("random_file.ytd").is_none());
        assert!(parse_ytd_filename("mp_f_freemode_01_mp_f_civ3^jbib_007_u.ydd").is_none());
        assert!(parse_tattoo_filename("not_a_tattoo.ytd").is_none());
    }

    #[test]
    fn category_labels() {
        assert_eq!(category_display_name("jbib"), "Tops");
        assert_eq!(category_display_name("unknown_cat"), "unknown_cat");
    }
}
