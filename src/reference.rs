//! The tone reference: the camera's own rendering of the frame, which the tone curve is fitted to.
//!
//! In order of preference:
//!
//! 1. **a sidecar JPEG** the camera wrote next to the RAW (RAW+JPEG shooting): `IMG_1.JPG` beside
//!    `IMG_1.CR3`, or in the shoot layout `<shoot>/jpg/IMG_1.JPG` for `<shoot>/raw/IMG_1.CR3`;
//! 2. **the RAW's embedded preview** (almost every camera stores a full- or half-size JPEG inside the RAW;
//!    exiftool extracts it to a pipe, nothing is written). The curve fit only looks at an 800 px thumbnail
//!    of the reference, so a preview is enough;
//! 3. **a fixed neutral curve** (`[tone.neutral]` in the look) when there is neither.
//!
//! An embedded preview is only trusted if it is at least `MIN_PREVIEW_EDGE` pixels on its long edge and
//! has the frame's aspect ratio (either way round: previews are often stored unrotated). Otherwise the
//! next candidate is tried, and finally the neutral curve. The sky matrix (`--match`) needs the reference
//! to line up pixel for pixel, so it additionally requires the same orientation (see `develop.rs`).

use crate::export;
use crate::image::Rgb8;
use std::path::{Path, PathBuf};

/// Smallest usable embedded preview, long edge in pixels.
pub const MIN_PREVIEW_EDGE: usize = 320;

/// How much a reference's aspect ratio may differ from the frame's and still count as "the same".
pub const ASPECT_TOLERANCE: f64 = 0.03;

/// What the user asked for (`--tone-ref`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RefMode {
    /// sidecar, then preview, then neutral
    Auto,
    /// skip any sidecar: embedded preview, then neutral
    Preview,
    /// always the neutral curve
    Neutral,
}

impl RefMode {
    pub fn parse(s: &str) -> Option<RefMode> {
        match s {
            "auto" => Some(RefMode::Auto),
            "preview" => Some(RefMode::Preview),
            "neutral" => Some(RefMode::Neutral),
            _ => None,
        }
    }
}

/// The plan for one frame, decided before developing.
#[derive(Clone, Debug)]
pub enum ToneRef {
    Sidecar(PathBuf),
    /// embedded preview tags to try, biggest first
    Preview(Vec<String>),
    Neutral,
}

impl ToneRef {
    /// For the plan / log: "sidecar", "preview", "neutral".
    pub fn kind(&self) -> &'static str {
        match self {
            ToneRef::Sidecar(_) => "sidecar",
            ToneRef::Preview(_) => "preview",
            ToneRef::Neutral => "neutral",
        }
    }
}

/// The camera JPEG written next to a RAW, if there is one.
pub fn find_sidecar(raw: &Path) -> Option<PathBuf> {
    let stem = raw.file_stem()?.to_str()?;
    let dir = raw.parent()?;
    let mut dirs = vec![dir.to_path_buf()];
    // shoot layout: <shoot>/raw/X.CR3 + <shoot>/jpg/X.JPG
    if dir.file_name().is_some_and(|n| n == "raw")
        && let Some(shoot) = dir.parent()
    {
        dirs.insert(0, shoot.join("jpg"));
    }
    for d in dirs {
        for ext in ["JPG", "jpg", "JPEG", "jpeg"] {
            let p = d.join(format!("{stem}.{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Decide the reference for one frame.
pub fn plan(raw: &Path, mode: RefMode, previews: &[(String, u64)]) -> ToneRef {
    if mode == RefMode::Auto
        && let Some(s) = find_sidecar(raw)
    {
        return ToneRef::Sidecar(s);
    }
    if mode != RefMode::Neutral && !previews.is_empty() {
        return ToneRef::Preview(previews.iter().map(|p| p.0.clone()).collect());
    }
    ToneRef::Neutral
}

/// A loaded reference image, and where it came from ("sidecar", "preview:JpgFromRaw").
pub struct Loaded {
    pub image: Rgb8,
    pub source: String,
}

fn aspect_close(a: (usize, usize), b: (usize, usize)) -> bool {
    let ra = a.0 as f64 / a.1.max(1) as f64;
    let rb = b.0 as f64 / b.1.max(1) as f64;
    (ra / rb - 1.0).abs() <= ASPECT_TOLERANCE
}

/// The reference has the frame's aspect ratio in the same orientation (good enough for the sky matrix).
pub fn lines_up(reference: &Rgb8, frame: (usize, usize)) -> bool {
    aspect_close((reference.width, reference.height), frame)
}

/// Load the planned reference for a frame of size `frame` (after LibRaw's rotation). `Ok(None)` means
/// "use the neutral curve" (planned, or no usable preview). A sidecar that can't be read is an error,
/// as it always was: a broken camera JPEG is worth a failed frame and a look, not a silent fallback.
pub fn load(raw: &Path, plan: &ToneRef, frame: (usize, usize)) -> crate::Result<Option<Loaded>> {
    match plan {
        ToneRef::Neutral => Ok(None),
        ToneRef::Sidecar(p) => Ok(Some(Loaded {
            image: export::read_jpeg(p)?,
            source: "sidecar".to_string(),
        })),
        ToneRef::Preview(tags) => {
            for tag in tags {
                let Ok(bytes) = export::extract_preview(raw, tag) else {
                    continue;
                };
                let Ok(img) = export::decode_jpeg(&bytes) else {
                    continue;
                };
                let long = img.width.max(img.height);
                let fits = aspect_close((img.width, img.height), frame)
                    || aspect_close((img.height, img.width), frame);
                if long >= MIN_PREVIEW_EDGE && fits {
                    return Ok(Some(Loaded {
                        image: img,
                        source: format!("preview:{tag}"),
                    }));
                }
            }
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aspect_either_way_round() {
        assert!(aspect_close((6000, 4000), (6024, 4020)));
        assert!(!aspect_close((4000, 6000), (6024, 4020)));
        assert!(!aspect_close((1920, 1080), (6000, 4000)));
    }

    #[test]
    fn sidecar_in_shoot_layout_and_next_to_raw() {
        let d = std::env::temp_dir().join(format!("rawdev-sidecar-{}", std::process::id()));
        std::fs::create_dir_all(d.join("raw")).unwrap();
        std::fs::create_dir_all(d.join("jpg")).unwrap();
        std::fs::write(d.join("raw/A.CR3"), b"x").unwrap();
        assert_eq!(find_sidecar(&d.join("raw/A.CR3")), None);
        std::fs::write(d.join("raw/A.jpg"), b"x").unwrap();
        assert_eq!(
            find_sidecar(&d.join("raw/A.CR3")),
            Some(d.join("raw/A.jpg"))
        );
        std::fs::write(d.join("jpg/A.JPG"), b"x").unwrap();
        assert_eq!(
            find_sidecar(&d.join("raw/A.CR3")),
            Some(d.join("jpg/A.JPG"))
        );
        std::fs::remove_dir_all(&d).unwrap();
    }
}
