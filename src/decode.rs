//! RAW decoding: we let LibRaw's own `dcraw_emu` tool do it, with exactly the flags the Python recipe uses,
//! and read its 16-bit PPM output straight from a pipe (no temp file).
//!
//! Why shell out instead of linking libraw through FFI? See the README ("Decoding"). Short version: the
//! subprocess gives byte-identical output to the Python pipeline for free, costs ~0.8 s, and keeps
//! `unsafe` C bindings out of a learning codebase.

use crate::Result;
use rayon::prelude::*;
use std::path::Path;
use std::process::{Command, Stdio};

/// The dcraw_emu flags from develop.py, in the same order:
///   -w        camera white balance
///   -W        no automatic brightening
///   -H 0      highlights: clip
///   -o 1      output colour space sRGB (primaries; the gamma is set by -g)
///   -q 3      AHD demosaic
///   -6        16-bit output
///   -g 1 1    linear "gamma" (no curve)
pub const DCRAW_FLAGS: [&str; 12] = [
    "-w", "-W", "-H", "0", "-o", "1", "-q", "3", "-6", "-g", "1", "1",
];

/// Linear 16-bit RGB straight from LibRaw (already rotated to the camera's orientation).
pub struct Linear16 {
    pub width: usize,
    pub height: usize,
    /// The PPM "maxval" (65535 for 16-bit output).
    pub maxval: u32,
    /// Interleaved R G B samples.
    pub data: Vec<u16>,
}

/// Highlight mode, dcraw_emu's `-H` flag.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Highlights {
    /// `-H 0`: clip (the default; blown areas go neutral white once green is clipped too).
    Clip,
    /// `-H 2`: blend - rebuild blown channels from the ones that still have detail (the day recipe's
    /// RECOVER_HL frames: bright skies and white walls keep some texture).
    Blend,
}

/// Decode a RAW file with `dcraw_emu ... -Z -` (the `-Z -` part means "write the image to stdout").
pub fn decode_raw(raw: &Path) -> Result<Linear16> {
    decode_raw_with(raw, Highlights::Clip)
}

/// Same as [`decode_raw`], choosing the highlight mode.
pub fn decode_raw_with(raw: &Path, hl: Highlights) -> Result<Linear16> {
    let mut flags = DCRAW_FLAGS;
    flags[3] = match hl {
        Highlights::Clip => "0",
        Highlights::Blend => "2",
    };
    let mut cmd = Command::new(crate::tools::dcraw_emu());
    if needs_single_thread(raw) {
        cmd.env("OMP_NUM_THREADS", "1");
    }
    let out = cmd
        .args(flags)
        .args(["-Z", "-"])
        .arg(raw)
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("could not run dcraw_emu (is libraw installed?): {e}"))?;
    if !out.status.success() {
        return Err(format!("dcraw_emu failed on {} ({})", raw.display(), out.status).into());
    }
    parse_ppm16(&out.stdout)
}

/// LibRaw's X-Trans demosaic (Fujifilm RAF) is not deterministic when OpenMP runs it on several
/// threads: three runs of the same file gave three different images (up to 34 levels apart after the
/// tone curve; found by `tools/gpu-parity.sh`). With one thread it is stable. Bayer formats (CR2, CR3,
/// NEF, ARW, ORF, RW2, PEF, DNG) were checked the same way and are deterministic, so only RAF pays the
/// single-thread cost. (A Bayer Fujifilm file is decoded single-threaded too; that is merely slower.)
pub fn needs_single_thread(raw: &Path) -> bool {
    raw.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("raf"))
}

/// Parse a binary 16-bit PPM ("P6", maxval > 255, big-endian samples).
/// The header is four whitespace-separated fields - `P6 width height maxval` - then exactly one
/// whitespace byte, then the pixel data. dcraw_emu never writes `#` comments, so we don't handle them.
fn parse_ppm16(bytes: &[u8]) -> Result<Linear16> {
    let mut fields: Vec<&str> = Vec::new();
    let mut i = 0;
    while fields.len() < 4 {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if start == i {
            return Err("truncated PPM header".into());
        }
        fields.push(std::str::from_utf8(&bytes[start..i])?);
    }
    i += 1; // the single whitespace byte after maxval

    if fields[0] != "P6" {
        return Err(format!("expected a P6 PPM, got {:?}", fields[0]).into());
    }
    let width: usize = fields[1].parse()?;
    let height: usize = fields[2].parse()?;
    let maxval: u32 = fields[3].parse()?;
    if maxval <= 255 {
        return Err("expected 16-bit PPM output from dcraw_emu (-6)".into());
    }
    let pixels = &bytes[i..];
    if pixels.len() != width * height * 3 * 2 {
        return Err(format!(
            "PPM data is {} bytes, expected {}",
            pixels.len(),
            width * height * 6
        )
        .into());
    }
    // Each sample is two bytes, most significant first ("big-endian").
    let data: Vec<u16> = pixels
        .par_chunks_exact(2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
        .collect();
    Ok(Linear16 {
        width,
        height,
        maxval,
        data,
    })
}
