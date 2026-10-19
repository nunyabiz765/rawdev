//! Reading and writing JPEGs, and copying the RAW's EXIF (+GPS) with exiftool exactly as develop.py does.
//!
//! JPEG work goes through libjpeg-turbo (the system library, via the `turbojpeg` crate). That is the same
//! library Pillow uses, with the same settings (accurate integer DCT, standard Huffman tables, "fancy"
//! chroma upsampling when reading), so the camera JPEG decodes to the same pixels as in Python and our
//! outputs decode to the same pixels as Python's would.

use crate::Result;
use crate::image::Rgb8;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use turbojpeg::{Compressor, Image, PixelFormat, Subsamp};

/// The "sRGB built-in" profile Pillow embeds (LittleCMS's sRGB, taken from one of the Python outputs).
pub const SRGB_ICC: &[u8] = include_bytes!("../assets/srgb-lcms.icc");

/// Chroma subsampling: 4:4:4 keeps full colour resolution (full-size export, PIL `subsampling=0`);
/// 4:2:0 halves it both ways (PIL's default, used for the share copy).
#[derive(Clone, Copy, Debug)]
pub enum Chroma {
    Full444,
    Half420,
}

/// Decode a JPEG file to 8-bit RGB.
pub fn read_jpeg(path: &Path) -> Result<Rgb8> {
    decode_jpeg(&std::fs::read(path)?)
}

/// Decode JPEG bytes to 8-bit RGB.
pub fn decode_jpeg(bytes: &[u8]) -> Result<Rgb8> {
    let img = turbojpeg::decompress(bytes, PixelFormat::RGB)?;
    // `pitch` is the length of one row in bytes; libjpeg-turbo packs rows tightly for RGB
    if img.pitch != img.width * 3 {
        return Err("unexpected row padding in decoded JPEG".into());
    }
    Ok(Rgb8::from_vec(img.width, img.height, img.pixels))
}

/// Encode `img` as a baseline JPEG at `quality` with the sRGB profile attached, and write it to `path`.
pub fn write_jpeg(path: &Path, img: &Rgb8, quality: i32, chroma: Chroma) -> Result<()> {
    let mut c = Compressor::new()?;
    c.set_quality(quality)?;
    c.set_subsamp(match chroma {
        Chroma::Full444 => Subsamp::None,
        Chroma::Half420 => Subsamp::Sub2x2,
    })?;
    let image = Image {
        pixels: &img.data[..],
        width: img.width,
        pitch: img.width * 3,
        height: img.height,
        format: PixelFormat::RGB,
    };
    let jpeg = c.compress_to_vec(image)?;
    std::fs::write(path, with_icc_profile(&jpeg, SRGB_ICC))?;
    Ok(())
}

/// Insert an ICC profile into a JPEG byte stream. The profile goes in an APP2 "ICC_PROFILE" segment
/// right after the APP0 (JFIF) segment, which is where libjpeg/Pillow put it:
///
///   FF E2 | length (2 bytes, counts itself) | "ICC_PROFILE\0" | chunk 1 | of 1 | profile bytes
fn with_icc_profile(jpeg: &[u8], icc: &[u8]) -> Vec<u8> {
    // SOI is 2 bytes; if APP0 follows, skip it too (its length field is at bytes 4..6)
    let mut at = 2;
    if jpeg[2..4] == [0xFF, 0xE0] {
        at = 4 + u16::from_be_bytes([jpeg[4], jpeg[5]]) as usize;
    }
    let mut seg = vec![0xFF, 0xE2];
    seg.extend_from_slice(&((2 + 12 + 2 + icc.len()) as u16).to_be_bytes());
    seg.extend_from_slice(b"ICC_PROFILE\0");
    seg.extend_from_slice(&[1, 1]);
    seg.extend_from_slice(icc);

    let mut out = Vec::with_capacity(jpeg.len() + seg.len());
    out.extend_from_slice(&jpeg[..at]);
    out.extend_from_slice(&seg);
    out.extend_from_slice(&jpeg[at..]);
    out
}

/// Copy all EXIF and GPS tags from the RAW into each output, and reset Orientation to 1 (LibRaw already
/// rotated the pixels). Same arguments as develop.py, but one exiftool run for both files.
pub fn copy_exif(raw: &Path, outputs: &[&Path]) -> Result<()> {
    let status = Command::new(crate::tools::exiftool())
        .args(["-q", "-overwrite_original", "-TagsFromFile"])
        .arg(raw)
        .args(["-EXIF:all", "-GPS:all", "-Orientation=1", "-n"])
        .args(outputs)
        .status()?;
    if !status.success() {
        return Err(format!("exiftool failed copying tags from {}", raw.display()).into());
    }
    Ok(())
}

/// Read the ISO of many RAWs with a single exiftool run (`-T` prints one tab-separated line per file, in
/// argument order). A missing value falls back to 800, like develop.py.
pub fn read_isos(raws: &[PathBuf]) -> Result<HashMap<PathBuf, u32>> {
    let out = Command::new(crate::tools::exiftool())
        .args(["-T", "-ISO"])
        .args(raws)
        .output()?;
    if !out.status.success() {
        return Err("exiftool could not read ISO values".into());
    }
    let text = String::from_utf8(out.stdout)?;
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() != raws.len() {
        return Err(format!(
            "exiftool returned {} ISO lines for {} files",
            lines.len(),
            raws.len()
        )
        .into());
    }
    Ok(raws
        .iter()
        .zip(lines)
        .map(|(p, l)| (p.clone(), l.trim().parse().unwrap_or(800)))
        .collect())
}

/// What one exiftool run tells us about each RAW before developing it.
#[derive(Clone, Debug, Default)]
pub struct RawMeta {
    /// ISO (800 when the file has none, as the original recipe did)
    pub iso: u32,
    /// embedded JPEG previews, biggest first: (exiftool tag, byte length)
    pub previews: Vec<(String, u64)>,
    pub make: String,
    pub model: String,
}

/// The exiftool tags that can hold an embedded JPEG preview, across makers.
pub const PREVIEW_TAGS: [&str; 4] = ["JpgFromRaw", "PreviewImage", "OtherImage", "ThumbnailImage"];

/// Read ISO, make/model and the embedded preview sizes of many RAWs in one exiftool run (`-json`).
pub fn read_meta(raws: &[PathBuf]) -> Result<Vec<RawMeta>> {
    if raws.is_empty() {
        return Ok(Vec::new());
    }
    let mut cmd = Command::new(crate::tools::exiftool());
    cmd.args(["-json", "-ISO", "-Make", "-Model"]);
    for t in PREVIEW_TAGS {
        cmd.arg(format!("-{t}"));
    }
    let out = cmd.args(raws).output()?;
    // exiftool exits 1 if one file had a problem but still prints the rest; judge by the JSON
    let text = String::from_utf8(out.stdout)?;
    let rows: Vec<serde_json::Value> = serde_json::from_str(&text)
        .map_err(|e| format!("exiftool -json output not understood: {e}"))?;
    if rows.len() != raws.len() {
        return Err(format!(
            "exiftool returned metadata for {} of {} files",
            rows.len(),
            raws.len()
        )
        .into());
    }
    Ok(rows
        .iter()
        .map(|r| {
            let s = |k: &str| r.get(k).map(json_text).unwrap_or_default();
            let mut previews: Vec<(String, u64)> = PREVIEW_TAGS
                .iter()
                .filter_map(|t| binary_len(r.get(*t)?).map(|n| (t.to_string(), n)))
                .collect();
            previews.sort_by_key(|p| std::cmp::Reverse(p.1));
            RawMeta {
                iso: s("ISO").trim().parse().unwrap_or(800),
                previews,
                make: s("Make"),
                model: s("Model"),
            }
        })
        .collect())
}

fn json_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// exiftool shows binary tags as "(Binary data 123456 bytes, use -b option to extract)".
fn binary_len(v: &serde_json::Value) -> Option<u64> {
    let s = v.as_str()?;
    let rest = s.strip_prefix("(Binary data ")?;
    rest.split_whitespace().next()?.parse().ok()
}

/// Extract one embedded preview's bytes (`exiftool -b -TAG raw`, to a pipe; nothing is written).
pub fn extract_preview(raw: &Path, tag: &str) -> Result<Vec<u8>> {
    let out = Command::new(crate::tools::exiftool())
        .arg("-b")
        .arg(format!("-{tag}"))
        .arg(raw)
        .output()?;
    if !out.status.success() || out.stdout.len() < 4 || out.stdout[..2] != [0xFF, 0xD8] {
        return Err(format!("no JPEG in {tag}").into());
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    #[test]
    fn binary_tag_lengths() {
        let v = serde_json::json!("(Binary data 1534567 bytes, use -b option to extract)");
        assert_eq!(super::binary_len(&v), Some(1534567));
        assert_eq!(super::binary_len(&serde_json::json!("12")), None);
    }
}
