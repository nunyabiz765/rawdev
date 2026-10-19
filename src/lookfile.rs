//! Look files: every number that makes up a look, read from TOML instead of being written into the code.
//!
//! A look picks one of the two pipelines (`recipe = "night"` or `"day"`, the code in `develop.rs`) and sets
//! its parameters. Four looks ship built in (`looks/*.toml`, compiled into the binary with `include_str!`):
//! `neutral-day` (the default for `rawdev develop`), `neutral-night`, and the two original looks the
//! engine was written for, `rich-night` and `rich-day`. The rich looks reproduce the original
//! hard-coded values exactly; `rich_presets_match_the_old_constants` below checks that number for number.
//!
//! `serde` does the reading: `#[derive(Deserialize)]` writes the code that turns a TOML table into the
//! struct, and `deny_unknown_fields` makes a typo in a key an error instead of a silently ignored line.

use crate::Result;
use crate::look::NrParams;
use serde::Deserialize;
use std::path::Path;

/// The looks compiled into the binary: (name, TOML source).
pub const BUILTIN: &[(&str, &str)] = &[
    ("neutral-day", include_str!("../looks/neutral-day.toml")),
    ("neutral-night", include_str!("../looks/neutral-night.toml")),
    ("rich-day", include_str!("../looks/rich-day.toml")),
    ("rich-night", include_str!("../looks/rich-night.toml")),
];

/// Which pipeline a look runs.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecipeKind {
    Night,
    Day,
}

impl RecipeKind {
    pub fn name(self) -> &'static str {
        match self {
            RecipeKind::Night => "night",
            RecipeKind::Day => "day",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Look {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub recipe: RecipeKind,
    pub tone: Tone,
    pub nr: Nr,
    pub chroma: Chroma,
    pub defringe: Defringe,
    /// day recipe only
    pub ca_fringe: Option<CaFringe>,
    /// day recipe only; needed for `--shadow-lift`
    pub shadow_lift: Option<ShadowLift>,
    /// day recipe only; needed for `--match`
    pub sky_match: Option<SkyMatch>,
    pub sharpen: Sharpen,
    pub export: Export,
    /// the TOML text this look was read from (hashed into the manifest)
    #[serde(skip)]
    pub source_text: String,
    /// "builtin" or the file's path
    #[serde(skip)]
    pub origin: String,
}

/// The tone stage (curve fitted to the camera's JPEG, then sRGB + vibrance).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tone {
    /// the whole fitted curve is multiplied by this (night: 0.97 so night stays night; day: 1.0)
    pub curve_scale: f64,
    /// saturation boost at zero saturation, tapering to none for fully saturated colours
    pub vibrance: f32,
    /// the fixed curve used when there is no camera JPEG and no usable embedded preview
    #[serde(default)]
    pub neutral: NeutralCurve,
}

/// The fallback tone curve: a straight gain from black up to `knee` (output, linear light), then a
/// smooth shoulder to white at the RAW's clip point. `gain = 2.0` lifts the RAW by one stop, which puts a
/// typical metered mid-grey near sRGB 120.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NeutralCurve {
    pub gain: f64,
    pub knee: f64,
}

impl Default for NeutralCurve {
    fn default() -> NeutralCurve {
        NeutralCurve {
            gain: 2.0,
            knee: 0.6,
        }
    }
}

/// Noise reduction.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Nr {
    /// no NR at all at or below this ISO
    pub off_max_iso: Option<u32>,
    /// median window for Cr/Cb after NLM
    pub chroma_median: usize,
    /// share of the un-denoised luma blended back for texture
    pub luma_blend: f64,
    /// where that luma comes from when a pre-median runs: "nlm-input" (after the median) or "original"
    pub blend_from: BlendFrom,
    /// NLM strengths by ISO, lowest first; the last step has no `max_iso`
    pub strength: Vec<NrStep>,
    /// applied automatically at or above `min_iso`
    pub hot: Option<NrHot>,
    /// applied to the frames named by `--strong` (night recipe)
    pub strong: Option<NrStrong>,
}

#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BlendFrom {
    NlmInput,
    Original,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NrStep {
    pub max_iso: Option<u32>,
    /// NLM `h` for luma and for colour
    pub luma: f32,
    pub color: f32,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NrHot {
    pub min_iso: u32,
    /// 3x3 RGB median before NLM
    pub pre_median: bool,
    pub luma_blend: f64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NrStrong {
    /// added to the ISO table's strengths
    pub luma_add: f32,
    pub color_add: f32,
    pub pre_median: bool,
    pub chroma_median: usize,
    pub luma_blend: f64,
}

/// Lab chroma lift, stronger on muted colours, sparing near-whites.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chroma {
    /// lift at zero chroma (0.30 = +30%)
    pub amount: f32,
    /// the lift fades to nothing at this C*
    pub fade_chroma: f32,
    /// near-white protection: full lift below L* = `white_l - white_width`, none above `white_l`
    pub white_l: f32,
    pub white_width: f32,
}

/// Purple-fringe cleanup in a ring just outside blown lights.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defringe {
    /// night: frames named by `--nofringe` skip it; day: always on when enabled
    pub enabled: bool,
    /// bright-core threshold (Rec.709 luma on 8-bit values)
    pub core_luma: f32,
    /// ring width: the cores are grown by an ellipse this many pixels across
    pub grow_px: usize,
    /// purple detector: ramps up over a* from `a_start` to `a_start + a_width`, and over b* from
    /// `b_end` down to `b_end - b_width`
    pub a_start: f32,
    pub a_width: f32,
    pub b_end: f32,
    pub b_width: f32,
    pub blur_sigma: f64,
    /// how far a/b are pulled to neutral at full mask
    pub strength: f32,
}

/// Day: purple at high-contrast edges.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaFringe {
    pub enabled: bool,
    pub a_start: f32,
    pub a_width: f32,
    pub b_end: f32,
    pub b_width: f32,
    /// Lab L gradient that counts as a full-strength edge
    pub edge_scale: f32,
    pub blur_sigma: f64,
    pub strength: f32,
    /// b is pulled by this share of the a pull
    pub b_factor: f32,
}

/// Day: gentle lift in linear light.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowLift {
    /// the lift fades to nothing at this linear value
    pub threshold: f32,
    pub max_boost: f32,
    pub gamma: f32,
}

/// Day: colour matrix fitted to the camera JPEG on blue sky, blended through a soft blue mask.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkyMatch {
    /// minimum blueness for a pixel to be a fitting sample
    pub sample_blueness: f32,
    /// the blend mask ramps from `mask_start` over `mask_width` of blueness, then is blurred
    pub mask_start: f32,
    pub mask_width: f32,
    pub mask_sigma: f64,
}

/// Unsharp mask (Pillow's UnsharpMask).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sharpen {
    pub radius: f32,
    pub percent: i32,
    pub threshold: i32,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Export {
    pub full_quality: i32,
    /// "4:4:4" or "4:2:0"
    pub full_chroma: String,
    pub share_long_edge: usize,
    pub share_quality: i32,
    pub share_chroma: String,
}

impl Look {
    /// Parse and check a look from TOML text.
    pub fn parse(text: &str) -> Result<Look> {
        let mut look: Look = toml::from_str(text)?;
        look.validate()?;
        look.source_text = text.to_string();
        look.origin = "builtin".to_string();
        Ok(look)
    }

    /// A built-in name (`rich-night`) or a path to a `.toml` file.
    pub fn load(name_or_path: &str) -> Result<Look> {
        if let Some((_, text)) = BUILTIN.iter().find(|(n, _)| *n == name_or_path) {
            return Look::parse(text);
        }
        let path = Path::new(name_or_path);
        if !path.exists() {
            let names: Vec<&str> = BUILTIN.iter().map(|b| b.0).collect();
            return Err(format!(
                "no look called {name_or_path} (built in: {}; or give a path to a .toml file)",
                names.join(", ")
            )
            .into());
        }
        let text = std::fs::read_to_string(path)?;
        let mut look = Look::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        look.origin = path.display().to_string();
        Ok(look)
    }

    fn validate(&self) -> Result<()> {
        let nc = &self.tone.neutral;
        if !(nc.gain >= 1.0 && nc.gain <= 16.0 && nc.knee > 0.05 && nc.knee < 0.95) {
            return Err("tone.neutral: gain must be 1..16 and knee between 0.05 and 0.95".into());
        }
        if self.export.share_long_edge < 16 {
            return Err("export.share_long_edge must be at least 16".into());
        }
        for q in [self.export.full_quality, self.export.share_quality] {
            if !(1..=100).contains(&q) {
                return Err(format!("export quality must be 1..100, not {q}").into());
            }
        }
        let s = &self.nr.strength;
        if s.is_empty() || s.last().unwrap().max_iso.is_some() {
            return Err("nr.strength: the last step must have no max_iso".into());
        }
        if s[..s.len() - 1].iter().any(|x| x.max_iso.is_none())
            || s.windows(2).any(|w| match (w[0].max_iso, w[1].max_iso) {
                (Some(a), Some(b)) => a >= b,
                _ => false,
            })
        {
            return Err(
                "nr.strength: max_iso must rise step by step, only the last step open".into(),
            );
        }
        for m in [self.nr.chroma_median]
            .into_iter()
            .chain(self.nr.strong.as_ref().map(|s| s.chroma_median))
        {
            if m != 3 && m != 5 {
                return Err(format!("nr chroma_median must be 3 or 5, not {m}").into());
            }
        }
        for c in [&self.export.full_chroma, &self.export.share_chroma] {
            if c != "4:4:4" && c != "4:2:0" {
                return Err(
                    format!("export chroma must be \"4:4:4\" or \"4:2:0\", not {c:?}").into(),
                );
            }
        }
        if self.recipe == RecipeKind::Night {
            for (set, key) in [
                (self.ca_fringe.is_some(), "ca_fringe"),
                (self.shadow_lift.is_some(), "shadow_lift"),
                (self.sky_match.is_some(), "sky_match"),
            ] {
                if set {
                    return Err(format!("[{key}] is only used by the day recipe").into());
                }
            }
        }
        Ok(())
    }

    /// NLM strengths for an ISO from the `nr.strength` table.
    pub fn nr_strength(&self, iso: u32) -> (f32, f32) {
        let step = self
            .nr
            .strength
            .iter()
            .find(|s| s.max_iso.is_none_or(|m| iso <= m))
            .expect("validated: the last step is open");
        (step.luma, step.color)
    }

    /// The NR settings for one frame, or `None` if NR is off at this ISO.
    pub fn nr_params(&self, iso: u32, strong: bool) -> Option<NrParams> {
        let nr = &self.nr;
        if nr.off_max_iso.is_some_and(|m| iso <= m) {
            return None;
        }
        let (h_luma, h_color) = self.nr_strength(iso);
        let mut p = NrParams {
            h_luma,
            h_color,
            pre_median: false,
            chroma_median: nr.chroma_median,
            luma_blend: nr.luma_blend,
            blend_from_input: nr.blend_from == BlendFrom::Original,
        };
        if let Some(hot) = &nr.hot
            && iso >= hot.min_iso
        {
            p.pre_median = hot.pre_median;
            p.luma_blend = hot.luma_blend;
        }
        if strong && let Some(s) = &nr.strong {
            p.h_luma = h_luma + s.luma_add;
            p.h_color = h_color + s.color_add;
            p.pre_median = s.pre_median;
            p.chroma_median = s.chroma_median;
            p.luma_blend = s.luma_blend;
        }
        Some(p)
    }
}

/// Look up a built-in look's TOML source.
pub fn builtin_source(name: &str) -> Option<&'static str> {
    BUILTIN.iter().find(|(n, _)| *n == name).map(|b| b.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The values the code had hard-wired before look files (git main, ad99662). TOML numbers are read
    /// as f64 and then narrowed to f32, so check each one lands on exactly the old f32 literal.
    #[test]
    fn rich_presets_match_the_old_constants() {
        let n = Look::load("rich-night").unwrap();
        let d = Look::load("rich-day").unwrap();
        assert_eq!(n.recipe, RecipeKind::Night);
        assert_eq!(d.recipe, RecipeKind::Day);
        assert_eq!(n.tone.curve_scale, 0.97);
        assert_eq!(d.tone.curve_scale, 1.0);
        for l in [&n, &d] {
            assert_eq!(l.tone.vibrance, 0.06f32);
            assert_eq!(l.chroma.amount, 0.30f32);
            assert_eq!(l.chroma.fade_chroma, 90.0f32);
            assert_eq!(l.chroma.white_l, 97.0f32);
            assert_eq!(l.chroma.white_width, 12.0f32);
            let f = &l.defringe;
            assert!(f.enabled);
            assert_eq!(
                (
                    f.core_luma,
                    f.grow_px,
                    f.a_start,
                    f.a_width,
                    f.b_end,
                    f.b_width
                ),
                (235.0f32, 15, 4.0f32, 10.0f32, 6.0f32, 10.0f32)
            );
            assert_eq!((f.blur_sigma, f.strength), (1.5f64, 0.8f32));
            assert_eq!(
                (l.sharpen.radius, l.sharpen.percent, l.sharpen.threshold),
                (1.0f32, 45, 4)
            );
            let e = &l.export;
            assert_eq!((e.full_quality, e.full_chroma.as_str()), (92, "4:4:4"));
            assert_eq!(
                (e.share_long_edge, e.share_quality, e.share_chroma.as_str()),
                (2000, 90, "4:2:0")
            );
        }
        let c = d.ca_fringe.as_ref().unwrap();
        assert!(c.enabled);
        assert_eq!(
            (c.a_start, c.a_width, c.b_end, c.b_width, c.edge_scale),
            (5.0f32, 12.0f32, 6.0f32, 14.0f32, 22.0f32)
        );
        assert_eq!(
            (c.blur_sigma, c.strength, c.b_factor),
            (1.5f64, 0.70f32, 0.4f32)
        );
        let s = d.shadow_lift.as_ref().unwrap();
        assert_eq!(
            (s.threshold, s.max_boost, s.gamma),
            (0.18f32, 1.50f32, 0.80f32)
        );
        let m = d.sky_match.as_ref().unwrap();
        assert_eq!(
            (m.sample_blueness, m.mask_start, m.mask_width, m.mask_sigma),
            (0.2f32, 0.12f32, 0.15f32, 3.0f64)
        );
        assert!(n.ca_fringe.is_none() && n.shadow_lift.is_none() && n.sky_match.is_none());
    }

    /// The old NR functions (`look::NrParams::for_frame` / `for_day`), written out, against the presets
    /// at every ISO from 0 to 102400.
    #[test]
    fn rich_presets_nr_matches_the_old_tables() {
        fn old_night(iso: u32) -> (f32, f32) {
            match iso {
                0..=500 => (3.0, 5.0),
                501..=1000 => (4.0, 6.0),
                1001..=2000 => (6.0, 9.0),
                2001..=3200 => (7.0, 11.0),
                _ => (9.0, 14.0),
            }
        }
        fn old_day(iso: u32) -> (f32, f32) {
            match iso {
                0..=3200 => old_night(iso),
                3201..=5000 => (9.0, 14.0),
                _ => (10.0, 18.0),
            }
        }
        let n = Look::load("rich-night").unwrap();
        let d = Look::load("rich-day").unwrap();
        for iso in 0..=102_400u32 {
            let (hl, hc) = old_night(iso);
            let p = n.nr_params(iso, false).unwrap();
            assert_eq!((p.h_luma, p.h_color), (hl, hc));
            assert_eq!(
                (
                    p.pre_median,
                    p.chroma_median,
                    p.luma_blend,
                    p.blend_from_input
                ),
                (false, 3, 0.2, false)
            );
            let p = n.nr_params(iso, true).unwrap();
            assert_eq!((p.h_luma, p.h_color), (hl + 1.0, hc + 7.0));
            assert_eq!(
                (
                    p.pre_median,
                    p.chroma_median,
                    p.luma_blend,
                    p.blend_from_input
                ),
                (true, 5, 0.1, false)
            );

            let p = d.nr_params(iso, false);
            if iso <= 200 {
                assert!(p.is_none());
                continue;
            }
            let p = p.unwrap();
            let hot = iso >= 5000;
            assert_eq!((p.h_luma, p.h_color), old_day(iso));
            assert_eq!(
                (
                    p.pre_median,
                    p.chroma_median,
                    p.luma_blend,
                    p.blend_from_input
                ),
                (hot, 3, if hot { 0.1 } else { 0.2 }, true)
            );
        }
    }

    #[test]
    fn every_builtin_parses_and_is_named_after_its_file() {
        for (name, text) in BUILTIN {
            let l = Look::parse(text).unwrap();
            assert_eq!(&l.name, name);
        }
        assert_eq!(Look::load("neutral-day").unwrap().recipe, RecipeKind::Day);
        assert_eq!(
            Look::load("neutral-night").unwrap().recipe,
            RecipeKind::Night
        );
    }

    #[test]
    fn bad_looks_are_rejected() {
        let good = builtin_source("rich-night").unwrap();
        assert!(Look::parse(&good.replace("vibrance", "vibrence")).is_err());
        assert!(Look::parse(&good.replace("recipe = \"night\"", "recipe = \"noon\"")).is_err());
        let day = builtin_source("rich-day").unwrap();
        assert!(Look::parse(&day.replace("recipe = \"day\"", "recipe = \"night\"")).is_err());
    }
}
