//! Per-frame choices: the command-line lists (`--match 7,12`) and the optional `recipe.toml`.
//!
//! A **selector** names a frame either by its file stem (`IMG_0007`, `DSC01234`) or by the number at the
//! end of the stem (`7`, `1234`). A selector that matches no input is an error, so a typo can't be
//! silently ignored.
//!
//! A **recipe file** (`--recipe FILE`) holds the same choices in one place, so an agent can write one
//! file instead of building long flag strings:
//!
//! ```toml
//! look = "rich-day"            # optional; --look on the command line wins
//!
//! [frames.IMG_0007]            # or [frames.7]
//! match = true                 # day looks: sky colour matrix
//! shadow_lift = true           # day looks: gentle linear-light shadow lift
//! recover_hl = true            # day looks: decode with highlight blend (-H 2)
//!
//! [frames.IMG_0012]
//! strong = true                # night looks: stronger NR for saturated coloured LEDs
//! nofringe = true              # night looks: skip the purple-ring cleanup
//! ```
//!
//! Flags and the file add up: a frame gets an option if either names it.

use crate::Result;
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameFlags {
    #[serde(default, rename = "match")]
    pub match_sky: bool,
    #[serde(default)]
    pub shadow_lift: bool,
    #[serde(default)]
    pub recover_hl: bool,
    #[serde(default)]
    pub strong: bool,
    #[serde(default)]
    pub nofringe: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeFile {
    pub look: Option<String>,
    #[serde(default)]
    pub frames: BTreeMap<String, FrameFlags>,
}

impl RecipeFile {
    pub fn load(path: &Path) -> Result<RecipeFile> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("recipe {}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| format!("recipe {}: {e}", path.display()).into())
    }
}

/// The number at the end of a stem: `IMG_0007` -> 7, `DSC01234` -> 1234, `P1010001` -> 1010001.
fn trailing_number(stem: &str) -> Option<u64> {
    let digits: String = stem
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    digits.parse().ok()
}

/// Does `sel` name the frame with this stem?
pub fn matches(sel: &str, stem: &str) -> bool {
    if sel == stem {
        return true;
    }
    if !sel.is_empty() && sel.chars().all(|c| c.is_ascii_digit()) {
        return sel.parse::<u64>().ok() == trailing_number(stem);
    }
    false
}

/// Split a comma-separated list.
pub fn parse_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

/// For each selector list, the set of stems it names. Errors on a selector that names nothing.
pub fn resolve(what: &str, sels: &[String], stems: &[String]) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    for s in sels {
        let hit: Vec<&String> = stems.iter().filter(|st| matches(s, st)).collect();
        if hit.is_empty() {
            return Err(format!("{what} {s}: no input frame has that name or number").into());
        }
        out.extend(hit.into_iter().cloned());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors() {
        assert!(matches("7", "IMG_0007"));
        assert!(matches("IMG_0007", "IMG_0007"));
        assert!(matches("1234", "DSC01234"));
        assert!(!matches("70", "IMG_0007"));
        assert!(!matches("IMG_00", "IMG_0007"));
        let stems = vec!["IMG_1".to_string(), "IMG_2".to_string()];
        assert!(resolve("--match", &["3".into()], &stems).is_err());
        assert_eq!(resolve("--match", &["2".into()], &stems).unwrap().len(), 1);
    }

    #[test]
    fn recipe_file_parses_and_rejects_typos() {
        let r: RecipeFile =
            toml::from_str("look = \"rich-day\"\n[frames.7]\nmatch = true\n").unwrap();
        assert!(r.frames["7"].match_sky);
        assert!(toml::from_str::<RecipeFile>("[frames.1]\nmacth = true\n").is_err());
    }
}
