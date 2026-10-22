//! The "layered" recipe: a float pipeline built from ordered adjustment steps, the way an image editor
//! stacks layers. Where the night and day recipes fit a tone curve to the camera JPEG and work in 8 bits,
//! this one starts from LibRaw's own display rendering (BT.709 gamma, auto white point, brightness
//! matched to the camera JPEG) and then runs the look file's `[[step]]` list in order on f32 planes.
//!
//! The step maths follows ImageMagick 7 (Q16 HDRI) operator for operator - `-level`,
//! `-sigmoidal-contrast`, `-modulate` with a saturation mask, Lab blur, `-contrast-stretch`, `-unsharp`,
//! `-statistic median`, `-morphology` - including its Gaussian kernel construction and its Lab/HSL
//! formulas, so a look that was first prototyped as a `magick` command line can be carried over and
//! give the same picture in a few seconds instead of minutes. Values may leave 0..1 between steps (as in
//! a float editor); steps that ImageMagick clamps are clamped here too.

use crate::decode::Linear16;
use crate::image::Rgb8;
use crate::lookfile::{Display, Step};
use rayon::prelude::*;

/// Rec.709 luma weights on the encoded values (ImageMagick's `-colorspace gray`).
const LUMA: [f32; 3] = [0.212656, 0.715158, 0.072186];

/// A colour image as three float planes (R, G, B), row-major. Values are display-encoded, 0..1.
#[derive(Clone, Debug)]
pub struct Planes {
    pub width: usize,
    pub height: usize,
    pub c: [Vec<f32>; 3],
}

impl Planes {
    pub fn len(&self) -> usize {
        self.width * self.height
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Luma plane (Rec.709 weights on the encoded values).
    pub fn luma(&self) -> Vec<f32> {
        let [r, g, b] = &self.c;
        (0..self.len())
            .into_par_iter()
            .map(|i| LUMA[0] * r[i] + LUMA[1] * g[i] + LUMA[2] * b[i])
            .collect()
    }

    /// Round to 8 bits (clamped) and interleave.
    pub fn to_rgb8(&self) -> Rgb8 {
        let mut out = Rgb8::new(self.width, self.height);
        out.data
            .par_chunks_exact_mut(3)
            .enumerate()
            .for_each(|(i, px)| {
                for c in 0..3 {
                    px[c] = (self.c[c][i].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                }
            });
        out
    }

    /// 16-bit binary PPM of the planes (for `--dump-stages`; any image viewer or `magick` reads it).
    pub fn to_ppm16(&self) -> Vec<u8> {
        let mut out = format!("P6\n{} {}\n65535\n", self.width, self.height).into_bytes();
        out.reserve(self.len() * 6);
        for i in 0..self.len() {
            for c in 0..3 {
                let v = (self.c[c][i].clamp(0.0, 1.0) * 65535.0 + 0.5) as u16;
                out.extend_from_slice(&v.to_be_bytes());
            }
        }
        out
    }
}

/// What the display rendering decided, for the log line and the manifest.
#[derive(Clone, Copy, Debug)]
pub struct DisplayInfo {
    /// the auto white point: linear 16-bit value that maps to white at brightness 1
    pub white: u32,
    /// the brightness used (1.0 without a reference)
    pub brightness: f64,
    /// mean luma of the reference and of the frame at brightness 1 (`None` without a reference)
    pub ref_mean: Option<f64>,
    pub frame_mean: f64,
    /// share of pixels whose green is at white in the main rendering
    pub clip_g: f64,
}

// ---- display rendering -------------------------------------------------------------------------------

/// LibRaw's auto white point (`write_ppm_tiff`): per channel, walk the 8192-bin histogram (value >> 3)
/// down from the top until more than `clip_share` of the pixels are above; take the highest of the three.
pub fn auto_white(raw: &Linear16, clip_share: f32) -> u32 {
    let n = raw.width * raw.height;
    let hist = raw
        .data
        .par_chunks(3 * 65536)
        .fold(
            || vec![0u32; 3 * 0x2000],
            |mut h, chunk| {
                for px in chunk.chunks_exact(3) {
                    for c in 0..3 {
                        h[c * 0x2000 + (px[c] >> 3) as usize] += 1;
                    }
                }
                h
            },
        )
        .reduce(
            || vec![0u32; 3 * 0x2000],
            |mut a, b| {
                for (x, y) in a.iter_mut().zip(b) {
                    *x += y;
                }
                a
            },
        );
    // LibRaw: `int perc = width * height * auto_bright_thr` (float maths, truncated)
    let perc = (n as f32 * clip_share) as i64;
    let mut white = 0u32;
    for c in 0..3 {
        let mut total = 0i64;
        let mut val = 0x2000i64;
        loop {
            val -= 1;
            if val <= 32 {
                break;
            }
            total += hist[c * 0x2000 + val as usize] as i64;
            if total > perc {
                break;
            }
        }
        white = white.max(val as u32);
    }
    white
}

/// LibRaw/dcraw `gamma_curve(0.45, 4.5, 2, imax)`: the BT.709 display curve as a 65536-entry table,
/// with `imax = (white << 3) / bright` mapped to white.
pub fn display_curve(white: u32, bright: f32) -> Vec<u16> {
    let (pwr, ts) = (0.45f64, 4.5f64);
    let mut g = [pwr, ts, 0.0, 0.0, 0.0];
    let mut bnd = [0.0f64, 0.0];
    bnd[(g[1] >= 1.0) as usize] = 1.0;
    if g[1] != 0.0 && (g[1] - 1.0) * (g[0] - 1.0) <= 0.0 {
        for _ in 0..48 {
            g[2] = (bnd[0] + bnd[1]) / 2.0;
            let hi = ((g[2] / g[1]).powf(-g[0]) - 1.0) / g[0] - 1.0 / g[2] > -1.0;
            bnd[hi as usize] = g[2];
        }
        g[3] = g[2] / g[1];
        g[4] = g[2] * (1.0 / g[0] - 1.0);
    }
    let imax = (((white << 3) as f32) / bright) as i32;
    (0..0x10000u32)
        .map(|i| {
            let r = i as f64 / imax as f64;
            if r >= 1.0 {
                return 0xffff;
            }
            let v = if r < g[3] {
                r * g[1]
            } else {
                r.powf(g[0]) * (1.0 + g[4]) - g[4]
            };
            (65536.0 * v) as u16
        })
        .collect()
}

fn mean_luma_through(raw: &Linear16, curve: &[u16]) -> f64 {
    let sum: f64 = raw
        .data
        .par_chunks(3 * 4096)
        .map(|chunk| {
            chunk
                .chunks_exact(3)
                .map(|px| {
                    (0..3)
                        .map(|c| LUMA[c] as f64 * curve[px[c] as usize] as f64)
                        .sum::<f64>()
                })
                .sum::<f64>()
        })
        .sum();
    sum / 65535.0 / (raw.width * raw.height) as f64
}

/// Mean luma of an 8-bit reference, 0..1.
pub fn mean_luma8(img: &Rgb8) -> f64 {
    let sum: f64 = img
        .data
        .par_chunks(3 * 4096)
        .map(|chunk| {
            chunk
                .chunks_exact(3)
                .map(|px| (0..3).map(|c| LUMA[c] as f64 * px[c] as f64).sum::<f64>())
                .sum::<f64>()
        })
        .sum();
    sum / 255.0 / (img.width * img.height) as f64
}

/// Render the linear decode for display: auto white point, BT.709 curve, brightness matched to the
/// reference (`(ref_mean / frame_mean) ^ match_power`, clamped, rounded to 0.001), and optionally a
/// darker rendering blended in where the frame is bright (highlight recovery without reconstruction).
pub fn render(raw: &Linear16, d: &Display, reference: Option<&Rgb8>) -> (Planes, DisplayInfo) {
    let white = auto_white(raw, d.clip_share);
    let frame_mean = mean_luma_through(raw, &display_curve(white, 1.0));
    let ref_mean = reference.map(mean_luma8);
    let brightness = match ref_mean {
        Some(pm) if frame_mean > 0.0 => {
            let b = (pm / frame_mean)
                .powf(d.match_power)
                .clamp(d.brightness_min, d.brightness_max);
            (b * 1000.0).round() / 1000.0
        }
        _ => 1.0,
    };
    let base = display_curve(white, brightness as f32);
    let n = raw.width * raw.height;
    let plane = |c: usize, lut: &[u16]| -> Vec<f32> {
        (0..n)
            .into_par_iter()
            .map(|i| lut[raw.data[i * 3 + c] as usize] as f32 / 65535.0)
            .collect()
    };
    let mut p = Planes {
        width: raw.width,
        height: raw.height,
        c: [plane(0, &base), plane(1, &base), plane(2, &base)],
    };
    let clip_g = p.c[1].par_iter().filter(|v| **v >= 1.0).count() as f64 / n.max(1) as f64;
    if let Some(hb) = &d.highlight_blend {
        let dark = display_curve(white, (brightness * hb.darker) as f32);
        let mut mask = p.luma();
        level_plane(&mut mask, hb.mask_black, hb.mask_white);
        let mask = blur(&mask, p.width, p.height, hb.mask_sigma);
        for c in 0..3 {
            p.c[c].par_iter_mut().enumerate().for_each(|(i, v)| {
                let dv = dark[raw.data[i * 3 + c] as usize] as f32 / 65535.0;
                *v += (dv - *v) * mask[i];
            });
        }
    }
    (
        p,
        DisplayInfo {
            white,
            brightness,
            ref_mean,
            frame_mean,
            clip_g,
        },
    )
}

// ---- building blocks ---------------------------------------------------------------------------------

/// `(x - black) / (white - black)`, clamped to 0..1 (ImageMagick `-level black,white`).
pub fn level(x: f32, black: f32, white: f32) -> f32 {
    ((x - black) / (white - black)).clamp(0.0, 1.0)
}

fn level_plane(p: &mut [f32], black: f32, white: f32) {
    p.par_iter_mut().for_each(|v| *v = level(*v, black, white));
}

/// A linear ramp from 0 at `start` to 1 at `end` (either direction), clamped.
fn ramp(x: f32, start: f32, end: f32) -> f32 {
    ((x - start) / (end - start)).clamp(0.0, 1.0)
}

/// ImageMagick's Gaussian kernel width for `-blur 0xSIGMA` (`GetOptimalKernelWidth1D`, Q16): the
/// smallest odd width whose edge weight still exceeds 1/65535 of the total.
pub fn kernel_width(sigma: f64) -> usize {
    let g = sigma.abs();
    if g <= 1e-12 {
        return 3;
    }
    let alpha = 1.0 / (2.0 * g * g);
    let beta = 1.0 / ((2.0 * std::f64::consts::PI).sqrt() * g);
    let mut width = 5usize;
    loop {
        let j = (width as i64 - 1) / 2;
        let norm: f64 = (-j..=j)
            .map(|i| (-((i * i) as f64) * alpha).exp() * beta)
            .sum();
        let value = (-((j * j) as f64) * alpha).exp() * beta / norm;
        if value < 1.0 / 65535.0 || value < 1e-12 {
            break;
        }
        width += 2;
    }
    width - 2
}

/// ImageMagick's 1-D blur kernel: the Gaussian sampled at thirds of a pixel and binned ("KernelRank"
/// 3), then normalised.
pub fn kernel(sigma: f64) -> Vec<f32> {
    let width = kernel_width(sigma);
    let rank = 3i64;
    let v = (width as i64 * rank - 1) / 2;
    let s = sigma * rank as f64;
    let alpha = 1.0 / (2.0 * s * s);
    let beta = 1.0 / ((2.0 * std::f64::consts::PI).sqrt() * s);
    let mut k = vec![0f64; width];
    for u in -v..=v {
        k[((u + v) / rank) as usize] += (-((u * u) as f64) * alpha).exp() * beta;
    }
    let sum: f64 = k.iter().sum();
    k.iter().map(|x| (x / sum) as f32).collect()
}

/// Separable Gaussian blur of one plane, edge pixels repeated (ImageMagick `-blur 0xSIGMA`).
pub fn blur(src: &[f32], w: usize, h: usize, sigma: f64) -> Vec<f32> {
    let k = kernel(sigma);
    let r = k.len() / 2;
    // horizontal: each row padded with its edge values, then a multiply-add per tap across the row
    let mut tmp = vec![0f32; w * h];
    tmp.par_chunks_mut(w).enumerate().for_each(|(y, out)| {
        let row = &src[y * w..(y + 1) * w];
        let mut pad = Vec::with_capacity(w + 2 * r);
        pad.extend(std::iter::repeat_n(row[0], r));
        pad.extend_from_slice(row);
        pad.extend(std::iter::repeat_n(row[w - 1], r));
        for (t, &kt) in k.iter().enumerate() {
            let p = &pad[t..t + w];
            for x in 0..w {
                out[x] += kt * p[x];
            }
        }
    });
    // vertical: in column strips narrow enough that the rows a tap needs stay in cache
    const STRIP: usize = 512;
    let strips: Vec<(usize, Vec<f32>)> = (0..w.div_ceil(STRIP))
        .into_par_iter()
        .map(|s| {
            let x0 = s * STRIP;
            let sw = STRIP.min(w - x0);
            let mut out = vec![0f32; sw * h];
            for y in 0..h {
                let o = &mut out[y * sw..(y + 1) * sw];
                for (t, &kt) in k.iter().enumerate() {
                    let yy = (y + t).saturating_sub(r).min(h - 1);
                    let row = &tmp[yy * w + x0..yy * w + x0 + sw];
                    for x in 0..sw {
                        o[x] += kt * row[x];
                    }
                }
            }
            (x0, out)
        })
        .collect();
    let mut out = vec![0f32; w * h];
    for (x0, s) in strips {
        let sw = s.len() / h;
        for y in 0..h {
            out[y * w + x0..y * w + x0 + sw].copy_from_slice(&s[y * sw..(y + 1) * sw]);
        }
    }
    out
}

/// Min (`erode`) or max of the neighbourhood given as `(dx, dy)` offsets, edge pixels repeated.
fn morph(src: &[f32], w: usize, h: usize, offsets: &[(isize, isize)], max: bool) -> Vec<f32> {
    let mut out = vec![0f32; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            let mut acc = if max { f32::MIN } else { f32::MAX };
            for &(dx, dy) in offsets {
                let xx = (x as isize + dx).clamp(0, w as isize - 1) as usize;
                let yy = (y as isize + dy).clamp(0, h as isize - 1) as usize;
                let v = src[yy * w + xx];
                acc = if max { acc.max(v) } else { acc.min(v) };
            }
            *o = acc;
        }
    });
    out
}

/// The offsets inside a disk of `radius` pixels (ImageMagick's `Disk:radius` kernel: x² + y² <= r²).
pub fn disk(radius: f32) -> Vec<(isize, isize)> {
    let r = radius.floor() as isize;
    let mut v = Vec::new();
    for dy in -r..=r {
        for dx in -r..=r {
            if (dx * dx + dy * dy) as f32 <= radius * radius {
                v.push((dx, dy));
            }
        }
    }
    v
}

/// Per-channel median over a `size` x `size` window (values rounded to 16 bits, as ImageMagick's
/// `-statistic median` does: the element at index n/2 of the sorted window), computed only where `need`
/// is true (elsewhere the input is copied).
///
/// A sliding histogram (Huang's method, two levels: 256 coarse bins of 256 values each): moving one
/// pixel right takes the left column out and puts the new right column in, and the median is found by
/// walking from the previous median's coarse bin.
fn median(src: &[f32], w: usize, h: usize, size: usize, need: &[bool]) -> Vec<f32> {
    let r = (size / 2) as isize;
    let q: Vec<u16> = src
        .par_iter()
        .map(|v| (v.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16)
        .collect();
    let mut out = src.to_vec();
    let k = (size * size / 2) as u32; // rank of the median, 0-based
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let nrow = &need[y * w..(y + 1) * w];
        let (Some(first), Some(last)) =
            (nrow.iter().position(|b| *b), nrow.iter().rposition(|b| *b))
        else {
            return;
        };
        let lines: Vec<&[u16]> = (-r..=r)
            .map(|dy| {
                let yy = (y as isize + dy).clamp(0, h as isize - 1) as usize;
                &q[yy * w..(yy + 1) * w]
            })
            .collect();
        let cx = |x: isize| x.clamp(0, w as isize - 1) as usize;
        let mut hist = SlidingHist::new();
        for dx in -r..=r {
            hist.column(&lines, cx(first as isize + dx), 1);
        }
        for x in first..=last {
            if x > first {
                hist.column(&lines, cx(x as isize - r - 1), -1);
                hist.column(&lines, cx(x as isize + r), 1);
            }
            if nrow[x] {
                row[x] = hist.nth(k) as f32 / 65535.0;
            }
        }
    });
    out
}

/// A histogram of 16-bit values with a coarse level (256 bins of 256 values) and a remembered
/// position, so that finding the k-th value next to the last one only walks a few bins.
struct SlidingHist {
    fine: Vec<u16>,
    coarse: [u32; 256],
    /// coarse bin of the last answer, and how many values lie in the bins below it
    cm: usize,
    below: u32,
}

impl SlidingHist {
    fn new() -> SlidingHist {
        SlidingHist {
            fine: vec![0; 65536],
            coarse: [0; 256],
            cm: 0,
            below: 0,
        }
    }

    /// Add (`sign` = 1) or remove (-1) one column of the window.
    fn column(&mut self, lines: &[&[u16]], col: usize, sign: i32) {
        for l in lines {
            let v = l[col] as usize;
            let c = v >> 8;
            if sign > 0 {
                self.fine[v] += 1;
                self.coarse[c] += 1;
                if c < self.cm {
                    self.below += 1;
                }
            } else {
                self.fine[v] -= 1;
                self.coarse[c] -= 1;
                if c < self.cm {
                    self.below -= 1;
                }
            }
        }
    }

    /// The value at sorted index `k`.
    fn nth(&mut self, k: u32) -> usize {
        while self.below > k {
            self.cm -= 1;
            self.below -= self.coarse[self.cm];
        }
        while self.below + self.coarse[self.cm] <= k {
            self.below += self.coarse[self.cm];
            self.cm += 1;
        }
        let mut acc = self.below;
        let mut v = self.cm << 8;
        loop {
            acc += self.fine[v] as u32;
            if acc > k {
                return v;
            }
            v += 1;
        }
    }
}

/// Grow a boolean mask by `r` pixels in every direction (a square).
fn grow(mask: &[bool], w: usize, h: usize, r: usize) -> Vec<bool> {
    // rows: a pixel is set if a set pixel lies within r to its left or right
    let mut tmp = vec![false; w * h];
    tmp.par_chunks_mut(w).enumerate().for_each(|(y, out)| {
        let row = &mask[y * w..(y + 1) * w];
        let mut d = usize::MAX;
        for x in 0..w {
            d = if row[x] { 0 } else { d.saturating_add(1) };
            out[x] = d <= r;
        }
        d = usize::MAX;
        for x in (0..w).rev() {
            d = if row[x] { 0 } else { d.saturating_add(1) };
            out[x] |= d <= r;
        }
    });
    // then columns, the same way
    let mut out = vec![false; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, o)| {
        for yy in y.saturating_sub(r)..=(y + r).min(h - 1) {
            let row = &tmp[yy * w..(yy + 1) * w];
            for x in 0..w {
                o[x] |= row[x];
            }
        }
    });
    out
}

// ---- colour: HSL and Lab as ImageMagick computes them ----------------------------------------------

/// HSL lightness, chroma (max - min) and saturation of an encoded RGB pixel.
#[inline]
fn hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let c = max - min;
    let l = (max + min) / 2.0;
    if c <= 0.0 {
        return (l, 0.0, 0.0, min);
    }
    let d = if l <= 0.5 { 2.0 * l } else { 2.0 - 2.0 * l };
    let s = if d.abs() < 1e-12 { 0.0 } else { c / d };
    (l, c, s, min)
}

/// ImageMagick `-modulate 100*lightness,100*saturation` on one pixel (HSL, hue unchanged, unclamped).
#[inline]
pub fn modulate(px: [f32; 3], lightness: f32, saturation: f32) -> [f32; 3] {
    let (l, c, s, min) = hsl(px[0], px[1], px[2]);
    let l2 = l * lightness;
    let s2 = s * saturation;
    let c2 = if l2 <= 0.5 {
        2.0 * l2 * s2
    } else {
        (2.0 - 2.0 * l2) * s2
    };
    let min2 = l2 - 0.5 * c2;
    if c <= 0.0 {
        return [l2, l2, l2];
    }
    let k = c2 / c;
    [
        min2 + (px[0] - min) * k,
        min2 + (px[1] - min) * k,
        min2 + (px[2] - min) * k,
    ]
}

const D65: [f64; 3] = [0.95047, 1.0, 1.08883];
const CIE_EPS: f64 = 216.0 / 24389.0;
const CIE_K: f64 = 24389.0 / 27.0;

#[inline]
fn srgb_decode(x: f64) -> f64 {
    if x <= 0.0404482362771076 {
        x / 12.92
    } else {
        ((x + 0.055) / 1.055).powf(2.4)
    }
}

#[inline]
fn srgb_encode(x: f64) -> f64 {
    if x <= 0.0031306684425005883 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// Encoded sRGB (0..1) to ImageMagick's Lab encoding: L/100, a/255 + 0.5, b/255 + 0.5.
#[inline]
pub fn rgb_to_lab(px: [f32; 3]) -> [f32; 3] {
    let r = srgb_decode(px[0] as f64);
    let g = srgb_decode(px[1] as f64);
    let b = srgb_decode(px[2] as f64);
    let x = 0.4124564 * r + 0.3575761 * g + 0.1804375 * b;
    let y = 0.2126729 * r + 0.7151522 * g + 0.0721750 * b;
    let z = 0.0193339 * r + 0.1191920 * g + 0.9503041 * b;
    let f = |t: f64| {
        if t > CIE_EPS {
            t.cbrt()
        } else {
            (CIE_K * t + 16.0) / 116.0
        }
    };
    let (fx, fy, fz) = (f(x / D65[0]), f(y / D65[1]), f(z / D65[2]));
    [
        ((116.0 * fy - 16.0) / 100.0) as f32,
        ((500.0 * (fx - fy)) / 255.0 + 0.5) as f32,
        ((200.0 * (fy - fz)) / 255.0 + 0.5) as f32,
    ]
}

/// The inverse of [`rgb_to_lab`] (unclamped, like ImageMagick in HDRI).
#[inline]
pub fn lab_to_rgb(lab: [f32; 3]) -> [f32; 3] {
    let l = 100.0 * lab[0] as f64;
    let a = 255.0 * (lab[1] as f64 - 0.5);
    let bb = 255.0 * (lab[2] as f64 - 0.5);
    let fy = (l + 16.0) / 116.0;
    let fx = fy + a / 500.0;
    let fz = fy - bb / 200.0;
    let x = if fx * fx * fx > CIE_EPS {
        fx * fx * fx
    } else {
        (116.0 * fx - 16.0) / CIE_K
    };
    let y = if l > CIE_K * CIE_EPS {
        fy * fy * fy
    } else {
        l / CIE_K
    };
    let z = if fz * fz * fz > CIE_EPS {
        fz * fz * fz
    } else {
        (116.0 * fz - 16.0) / CIE_K
    };
    let (x, y, z) = (D65[0] * x, D65[1] * y, D65[2] * z);
    let r = 3.2404542 * x - 1.5371385 * y - 0.4985314 * z;
    let g = -0.9692660 * x + 1.8760108 * y + 0.0415560 * z;
    let b = 0.0556434 * x - 0.2040259 * y + 1.0572252 * z;
    // out of gamut below zero: ImageMagick lifts all three channels by the most negative one (keeps the
    // hue direction instead of clipping one channel); above 1 it leaves the values alone
    let m = r.min(g).min(b);
    let (r, g, b) = if m < 0.0 {
        (r - m, g - m, b - m)
    } else {
        (r, g, b)
    };
    [
        srgb_encode(r) as f32,
        srgb_encode(g) as f32,
        srgb_encode(b) as f32,
    ]
}

// ---- the steps -------------------------------------------------------------------------------------

fn px(p: &Planes, i: usize) -> [f32; 3] {
    [p.c[0][i], p.c[1][i], p.c[2][i]]
}

/// Mix `over` into `p` by `mask` (0 = keep, 1 = replace), per pixel.
fn mix_planes(p: &mut Planes, over: &Planes, mask: &[f32]) {
    for c in 0..3 {
        p.c[c]
            .par_iter_mut()
            .zip(over.c[c].par_iter())
            .zip(mask.par_iter())
            .for_each(|((v, o), m)| *v += (o - *v) * m);
    }
}

/// Apply `f` to every pixel, producing a new image.
fn map_pixels(p: &Planes, f: impl Fn([f32; 3]) -> [f32; 3] + Sync) -> Planes {
    let n = p.len();
    let px3: Vec<[f32; 3]> = (0..n).into_par_iter().map(|i| f(px(p, i))).collect();
    let mut out = Planes {
        width: p.width,
        height: p.height,
        c: [vec![0.0; n], vec![0.0; n], vec![0.0; n]],
    };
    for c in 0..3 {
        out.c[c]
            .par_iter_mut()
            .zip(px3.par_iter())
            .for_each(|(o, v)| *o = v[c]);
    }
    out
}

fn sigmoid(x: f32, contrast: f32, mid: f32) -> f32 {
    let s = |u: f32| 1.0 / (1.0 + (-u).exp());
    let lo = s(-contrast * mid);
    let hi = s(contrast * (1.0 - mid));
    // unclamped, like ImageMagick HDRI
    (s(contrast * (x - mid)) - lo) / (hi - lo)
}

/// Contrast stretch per channel: the darkest `black` and brightest `white` shares of the pixels are
/// clipped, the rest stretched to fill 0..1 (ImageMagick `-channel RGB -contrast-stretch`).
fn stretch(plane: &mut [f32], black: f64, white: f64) {
    let n = plane.len() as f64;
    let q = |v: f32| -> usize {
        if v <= 0.0 {
            0
        } else if v >= 1.0 {
            65535
        } else {
            (v * 65535.0 + 0.5) as usize
        }
    };
    let hist = plane
        .par_chunks(65536)
        .fold(
            || vec![0u32; 65536],
            |mut h, ch| {
                for &v in ch {
                    h[q(v)] += 1;
                }
                h
            },
        )
        .reduce(
            || vec![0u32; 65536],
            |mut a, b| {
                for (x, y) in a.iter_mut().zip(b) {
                    *x += y;
                }
                a
            },
        );
    let (mut lo, mut acc) = (0usize, 0f64);
    while lo <= 65535 {
        acc += hist[lo] as f64;
        if acc > black * n {
            break;
        }
        lo += 1;
    }
    let (mut hi, mut acc) = (65535usize, 0f64);
    while hi != 0 {
        acc += hist[hi] as f64;
        if acc > white * n {
            break;
        }
        hi -= 1;
    }
    let lo = lo.min(65535) as f32;
    let hi = hi as f32;
    plane.par_iter_mut().for_each(|v| {
        let j = q(*v) as f32;
        *v = if j < lo {
            0.0
        } else if j > hi {
            1.0
        } else if hi > lo {
            (j - lo) / (hi - lo)
        } else {
            0.0
        };
    });
}

fn unsharp(p: &mut Planes, sigma: f64, amount: f32, threshold: f32) {
    let (w, h) = (p.width, p.height);
    for c in 0..3 {
        let b = blur(&p.c[c], w, h, sigma);
        p.c[c].par_iter_mut().zip(b.par_iter()).for_each(|(v, bl)| {
            let d = *v - bl;
            // unclamped, like ImageMagick HDRI (overshoot survives until a `clamp` step)
            if (2.0 * d).abs() >= threshold {
                *v += amount * d;
            }
        });
    }
}

/// Run one step.
pub fn apply(p: &mut Planes, step: &Step) {
    let (w, h) = (p.width, p.height);
    match *step {
        Step::Levels { black, white } => {
            for c in 0..3 {
                level_plane(&mut p.c[c], black, white);
            }
        }
        Step::Clamp => {
            for c in 0..3 {
                p.c[c].par_iter_mut().for_each(|v| *v = v.clamp(0.0, 1.0));
            }
        }
        Step::Sigmoid { contrast, midpoint } => {
            for c in 0..3 {
                p.c[c]
                    .par_iter_mut()
                    .for_each(|v| *v = sigmoid(*v, contrast, midpoint));
            }
        }
        Step::Saturation {
            amount,
            lightness,
            mask_floor,
        } => {
            let n = p.len();
            let out: Vec<[f32; 3]> = (0..n)
                .into_par_iter()
                .map(|i| {
                    let x = px(p, i);
                    let (_, _, s, _) = hsl(x[0], x[1], x[2]);
                    let m = level(1.0 - s, mask_floor, 1.0);
                    let o = modulate(x, lightness, amount);
                    [
                        x[0] + (o[0] - x[0]) * m,
                        x[1] + (o[1] - x[1]) * m,
                        x[2] + (o[2] - x[2]) * m,
                    ]
                })
                .collect();
            for c in 0..3 {
                p.c[c]
                    .par_iter_mut()
                    .zip(out.par_iter())
                    .for_each(|(v, o)| *v = o[c]);
            }
        }
        Step::ChromaBlur { sigma } => {
            let mut lab = map_pixels(p, rgb_to_lab);
            lab.c[1] = blur(&lab.c[1], w, h, sigma);
            lab.c[2] = blur(&lab.c[2], w, h, sigma);
            *p = map_pixels(&lab, lab_to_rgb);
        }
        Step::Stretch { black, white } => {
            for c in 0..3 {
                stretch(&mut p.c[c], black, white);
            }
        }
        Step::Unsharp {
            sigma,
            amount,
            threshold,
        } => unsharp(p, sigma, amount, threshold),
        Step::Fringe {
            a_start,
            a_end,
            b_start,
            b_end,
            mask_sigma,
            top_full,
            top_zero,
            edge_start,
            edge_end,
            edge_sigma,
            saturation,
        } => {
            // colour detector in Lab (a, b in ImageMagick's /255 + 0.5 encoding -> real units here)
            let n = p.len();
            let det: Vec<f32> = (0..n)
                .into_par_iter()
                .map(|i| {
                    let lab = rgb_to_lab(px(p, i));
                    let a = (lab[1] - 0.5) * 255.0;
                    let b = (lab[2] - 0.5) * 255.0;
                    ramp(a, a_start, a_end) * ramp(b, b_start, b_end)
                })
                .collect();
            let mut mask = blur(&det, w, h, mask_sigma);
            // only in the top of the frame: full above `top_full`, none below `top_zero`
            mask.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
                let t = if h > 1 {
                    y as f32 / (h - 1) as f32
                } else {
                    0.0
                };
                let g = ramp(t, top_zero, top_full);
                row.iter_mut().for_each(|v| *v *= g);
            });
            // only on thin high-contrast edges
            let luma = p.luma();
            let d3 = disk(1.5);
            let hi = morph(&luma, w, h, &d3, true);
            let lo = morph(&luma, w, h, &d3, false);
            let edge: Vec<f32> = hi
                .par_iter()
                .zip(lo.par_iter())
                .map(|(a, b)| ramp(a - b, edge_start, edge_end))
                .collect();
            let edge = blur(&edge, w, h, edge_sigma);
            mask.par_iter_mut()
                .zip(edge.par_iter())
                .for_each(|(m, e)| *m = (*m * e).clamp(0.0, 1.0));
            let grey = map_pixels(p, |x| modulate(x, 1.0, saturation));
            mix_planes(p, &grey, &mask);
        }
        Step::BrightSmooth {
            mask_start,
            mask_end,
            mask_erode,
            mask_sigma,
            saturation,
            median: size,
            blur: post_sigma,
        } => {
            let mut mask = p.luma();
            level_plane(&mut mask, mask_start, mask_end);
            if mask_erode > 0.0 {
                mask = morph(&mask, w, h, &disk(mask_erode), false);
            }
            let mask = blur(&mask, w, h, mask_sigma);
            let need: Vec<bool> = mask.par_iter().map(|m| *m > 0.0).collect();
            if !need.iter().any(|b| *b) {
                return;
            }
            let src = if saturation != 1.0 {
                map_pixels(p, |x| modulate(x, 1.0, saturation))
            } else {
                p.clone()
            };
            let r = if post_sigma > 0.0 {
                kernel_width(post_sigma) / 2
            } else {
                0
            };
            let need_med = if r > 0 { grow(&need, w, h, r) } else { need };
            let mut over = Planes {
                width: w,
                height: h,
                c: [Vec::new(), Vec::new(), Vec::new()],
            };
            for c in 0..3 {
                let m = if size > 1 {
                    median(&src.c[c], w, h, size, &need_med)
                } else {
                    src.c[c].clone()
                };
                over.c[c] = if post_sigma > 0.0 {
                    blur(&m, w, h, post_sigma)
                } else {
                    m
                };
            }
            mix_planes(p, &over, &mask);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Numbers printed by ImageMagick 7.1.2 Q16-HDRI for the same operations
    /// (`magick xc:'rgb(200,100,50)' -colorspace Lab ...`, `-modulate`, `-sigmoidal-contrast`).
    #[test]
    fn colour_maths_matches_imagemagick() {
        let lab = rgb_to_lab([200.0 / 255.0, 100.0 / 255.0, 50.0 / 255.0]);
        for (got, want) in lab.iter().zip([0.536245f32, 0.642411, 0.677953]) {
            // ImageMagick's sRGB decode uses a fast pow approximation: agreement to ~1e-4
            assert!((got - want).abs() < 1e-4, "{got} vs {want}");
        }
        let back = lab_to_rgb(lab);
        for (got, want) in back.iter().zip([200.0f32, 100.0, 50.0]) {
            assert!((got * 255.0 - want).abs() < 1e-3);
        }
        // out of gamut: `magick xc:'rgb(10%,60%,30%)' -set colorspace Lab -colorspace sRGB`
        for (lab, want) in [
            ([0.10f32, 0.60, 0.30], [0.0f32, 0.150762, 0.407611]),
            ([0.60, 0.90, 0.50], [1.16332, 0.0, 0.621146]),
        ] {
            for (got, want) in lab_to_rgb(lab).iter().zip(want) {
                assert!((got - want).abs() < 2e-4, "{got} vs {want}");
            }
        }
        let m = modulate([200.0 / 255.0, 100.0 / 255.0, 50.0 / 255.0], 1.0, 1.25);
        for (got, want) in m.iter().zip([218.75f32, 93.75, 31.25]) {
            assert!((got * 255.0 - want).abs() < 1e-3, "{got} vs {want}");
        }
        // out of range on purpose: ImageMagick HDRI does not clamp -modulate
        let m = modulate([250.0 / 255.0, 10.0 / 255.0, 5.0 / 255.0], 1.0, 1.35);
        for (got, want) in m.iter().zip([292.875f32, -31.125, -37.875]) {
            assert!((got * 255.0 - want).abs() < 1e-3, "{got} vs {want}");
        }
        assert!((sigmoid(0.3, 3.0, 0.45) - 0.289865).abs() < 1e-6);
        // ImageMagick HDRI: `-sigmoidal-contrast 3,45%` of 1.3 is 1.14009 (no clamp)
        assert!((sigmoid(1.3, 3.0, 0.45) - 1.14009).abs() < 1e-5);
        assert_eq!(hsl(200.0 / 255.0, 100.0 / 255.0, 50.0 / 255.0).2, 0.6);
    }

    #[test]
    fn kernels_are_normalised_and_sized_like_imagemagick() {
        for s in [1.1, 2.0, 2.5, 6.0, 20.0] {
            let k = kernel(s);
            assert_eq!(k.len() % 2, 1);
            assert!((k.iter().sum::<f32>() - 1.0).abs() < 1e-5);
            // symmetric
            assert!((k[0] - k[k.len() - 1]).abs() < 1e-9);
        }
        assert_eq!(kernel_width(0.0), 3);
        assert!(kernel_width(20.0) > kernel_width(6.0));
        // a flat image stays flat under blur, edges included
        let flat = vec![0.25f32; 40 * 30];
        assert!(
            blur(&flat, 40, 30, 3.0)
                .iter()
                .all(|v| (v - 0.25).abs() < 1e-6)
        );
    }

    #[test]
    fn disks_match_imagemagick_kernels() {
        assert_eq!(disk(1.5).len(), 9); // 3x3
        assert_eq!(disk(2.0).len(), 13);
    }

    #[test]
    fn display_curve_is_monotonic_and_white_at_imax() {
        let c = display_curve(4096, 1.0); // imax = 32768
        assert!(c.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(c[0], 0);
        assert_eq!(c[32768], 0xffff);
        assert!(c[32767] > 65000);
        // a brighter rendering lifts every value
        let b = display_curve(4096, 1.5);
        assert!(b[8000] > c[8000]);
    }

    /// The sliding-histogram median against sorting every window, edges included.
    #[test]
    fn median_matches_brute_force() {
        let (w, h) = (37, 23);
        let mut seed = 12345u32;
        let v: Vec<f32> = (0..w * h)
            .map(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                (seed >> 8) as f32 / (1u32 << 24) as f32
            })
            .collect();
        let need: Vec<bool> = (0..w * h).map(|i| i % 7 != 3).collect();
        for size in [3usize, 7, 15] {
            let m = median(&v, w, h, size, &need);
            let r = (size / 2) as isize;
            for y in 0..h {
                for x in 0..w {
                    let i = y * w + x;
                    if !need[i] {
                        assert_eq!(m[i], v[i]);
                        continue;
                    }
                    let mut win = Vec::new();
                    for dy in -r..=r {
                        for dx in -r..=r {
                            let yy = (y as isize + dy).clamp(0, h as isize - 1) as usize;
                            let xx = (x as isize + dx).clamp(0, w as isize - 1) as usize;
                            win.push((v[yy * w + xx] * 65535.0 + 0.5) as u16);
                        }
                    }
                    win.sort();
                    assert_eq!(
                        m[i],
                        win[win.len() / 2] as f32 / 65535.0,
                        "size {size} at {x},{y}"
                    );
                }
            }
        }
    }

    #[test]
    fn median_and_grow() {
        let w = 5;
        let mut v = vec![0.5f32; 25];
        v[12] = 1.0; // one hot pixel
        let m = median(&v, w, 5, 3, &[true; 25]);
        assert!((m[12] - 0.5).abs() < 1e-4);
        let mut need = vec![false; 25];
        need[12] = true;
        let g = grow(&need, 5, 5, 1);
        assert_eq!(g.iter().filter(|b| **b).count(), 9);
        assert!(g[6] && g[18] && !g[0]);
    }

    #[test]
    fn stretch_fills_the_range() {
        let mut v: Vec<f32> = (0..1000).map(|i| 0.2 + 0.5 * i as f32 / 999.0).collect();
        stretch(&mut v, 0.0, 0.0);
        let min = v.iter().cloned().fold(1.0, f32::min);
        let max = v.iter().cloned().fold(0.0, f32::max);
        assert_eq!((min, max), (0.0, 1.0));
    }
}
