//! Ports of the OpenCV (cv2 5.0) routines the Python recipe calls.
//!
//! The goal here is *the same numbers as cv2*, not "a Lab conversion". OpenCV's 8-bit colour conversions
//! are integer code with lookup tables, and the 8-bit rounding is part of the look (every chroma lift and
//! defringe pass round-trips through 8-bit Lab). So these functions copy OpenCV's tables and fixed-point
//! arithmetic from `modules/imgproc/src/color_lab.cpp` / `color_yuv.simd.hpp`. The tests at the bottom
//! compare them with real cv2 output for all 16.7 million 8-bit inputs: they match bit for bit.

use crate::image::{Gray8, GrayF32, Rgb8, RgbF32};
use rayon::prelude::*;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------------------------------------
// Lab
// ---------------------------------------------------------------------------------------------------------

/// Which transfer curve sits in front of the Lab maths.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Gamma {
    /// `COLOR_RGB2LAB` / `COLOR_LAB2RGB`: 8-bit values are sRGB-encoded.
    Srgb,
    /// `COLOR_LBGR2Lab` / `COLOR_Lab2LBGR`: 8-bit values are treated as *linear* light. This is what
    /// `fastNlMeansDenoisingColored` uses internally (odd, but it's what the reference look was made with).
    Linear,
}

/// sRGB -> XYZ (D65) and the D65 white point, exactly as OpenCV defines them.
const RGB2XYZ: [[f64; 3]; 3] = [
    [0.412453, 0.357580, 0.180423],
    [0.212671, 0.715160, 0.072169],
    [0.019334, 0.119193, 0.950227],
];
const XYZ2RGB: [[f64; 3]; 3] = [
    [3.240479, -1.53715, -0.498535],
    [-0.969256, 1.875991, 0.041556],
    [0.055648, -0.204043, 1.057311],
];
const WHITE: [f64; 3] = [0.950456, 1.0, 1.088754];

/// Fixed-point bases OpenCV uses.
const LAB_SHIFT: i32 = 12; // matrix coefficients are scaled by 2^12
const LAB_SHIFT2: i32 = 15; // cube-root table values are scaled by 2^15
const BASE: i32 = 1 << 14; // Lab -> RGB works in units of 2^14
const MIN_AB: i32 = -8145; // smallest index into the a/b -> X/Z table

/// All the lookup tables, built once on first use (`OnceLock` = a thread-safe "compute once" cell).
struct LabTables {
    // forward (RGB -> Lab)
    gamma_srgb: [i32; 256], // sRGB 8-bit -> linear, scaled to 0..2040
    cbrt: Vec<i32>, // f(t) of the Lab formula (cube root, or the linear toe), scaled by 2^15
    to_xyz: [[i32; 3]; 3], // RGB -> XYZ / white, scaled by 2^12
    // inverse (Lab -> RGB)
    l_to_y: [i32; 256],       // Y from L, scaled by 2^14
    l_to_fy: [i32; 256],      // f(Y) from L, scaled by 2^14
    ab_to_xz: Vec<i32>,       // f^-1 for X and Z, indexed by (value - MIN_AB)
    to_rgb: [[i32; 3]; 3],    // XYZ * white -> RGB, scaled by 2^12
    inv_gamma_srgb: Vec<i32>, // linear 12-bit -> sRGB 8-bit
}

fn lab_tables() -> &'static LabTables {
    static TABLES: OnceLock<LabTables> = OnceLock::new();
    TABLES.get_or_init(build_lab_tables)
}

fn build_lab_tables() -> LabTables {
    // OpenCV writes these constants as exact fractions.
    let lthresh = 216f32 / 24389f32; // (6/29)^3
    let lscale = 841f32 / 108f32; // 7.787
    let lbias = 16f32 / 116f32;

    // sRGB decode for each 8-bit value, scaled by 255 * 2^3 and rounded.
    let mut gamma_srgb = [0i32; 256];
    for (i, g) in gamma_srgb.iter_mut().enumerate() {
        // (written the way that reproduces cv2's table exactly; verified by the test below)
        let x = i as f32 / 255f32;
        let lin = if x <= 0.04045 {
            x / 12.92
        } else {
            ((x as f64 + 0.055) / 1.055).powf(2.4) as f32
        };
        *g = (2040f32 * lin).round_ties_even() as i32;
    }

    // f(t): cube root above the threshold, a straight line below it. 3072 entries cover 0..1.5.
    let scale = 1f32 / 2040f32;
    let cbrt = (0..3072)
        .map(|i| {
            let x = scale * i as f32;
            let f = if x < lthresh {
                x * lscale + lbias
            } else {
                softfloat_cbrt(x)
            };
            (32768f32 * f).round_ties_even() as i32
        })
        .collect();

    let mut to_xyz = [[0i32; 3]; 3];
    let mut to_rgb = [[0i32; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            to_xyz[i][j] = (4096.0 * RGB2XYZ[i][j] / WHITE[i]).round_ties_even() as i32;
            to_rgb[i][j] = (4096.0 * XYZ2RGB[i][j] * WHITE[j]).round_ties_even() as i32;
        }
    }

    // L (0..255 encodes 0..100) -> Y and f(Y), in units of 2^14.
    let mut l_to_y = [0i32; 256];
    let mut l_to_fy = [0i32; 256];
    for i in 0..256usize {
        if i <= 20 {
            // the dark linear toe: L <= 8
            l_to_y[i] = ((i as i32 * BASE * 20 * 9) as f32 / (17 * 29 * 29 * 29) as f32)
                .round_ties_even() as i32;
            l_to_fy[i] = (BASE as f32
                * (16f32 / 116f32 + (i as i32 * 5) as f32 / (3 * 17 * 29) as f32))
                .round_ties_even() as i32;
        } else {
            let fy =
                (i as i32 * 100 * BASE) as f32 / (255 * 116) as f32 + (16 * BASE) as f32 / 116f32;
            l_to_fy[i] = fy.round_ties_even() as i32;
            l_to_y[i] = (fy * fy * fy / (BASE as f32 * BASE as f32)).round_ties_even() as i32;
        }
    }

    // f^-1 for the X and Z channels, plain integer maths (Rust's `/` truncates toward zero, like C's).
    let ab_to_xz = (MIN_AB..BASE * 9 / 4 + MIN_AB)
        .map(|i| {
            if i <= 3390 {
                i * 108 / 841 - BASE * 16 / 116 * 108 / 841
            } else {
                i * i / BASE * i / BASE
            }
        })
        .collect();

    // sRGB encode for each of 4096 linear levels.
    let inv_gamma_srgb = (0..4096)
        .map(|i| {
            let x = ((1f32 / 4096f32) * i as f32) as f64;
            let v = if x <= 7827.0 / 2500000.0 {
                x * (323.0 / 25.0)
            } else {
                x.powf(1.0 / (12.0 / 5.0)) * (1.0 + 11.0 / 200.0) - 11.0 / 200.0
            };
            (255f32 * v as f32).round_ties_even() as i32
        })
        .collect();

    LabTables {
        gamma_srgb,
        cbrt,
        to_xyz,
        l_to_y,
        l_to_fy,
        ab_to_xz,
        to_rgb,
        inv_gamma_srgb,
    }
}

/// The cube root OpenCV built its Lab table with: softfloat's `f32_cbrt`, Ken Turkowski's rational
/// approximation. It differs from a correctly rounded cube root in the last bit for a few inputs, and one
/// of those lands in the table, so we copy it rather than use `f32::cbrt`. (Positive normal inputs only.)
fn softfloat_cbrt(x: f32) -> f32 {
    let bits = x.to_bits();
    let ex = ((bits >> 23) & 0xff) as i32 - 127;
    let mut shx = ex % 3;
    if shx >= 0 {
        shx -= 3;
    }
    let ex = (ex - shx) / 3 - 1; // exponent of the result
    // the mantissa as an f64 in [0.125, 1)
    let fr = f64::from_bits((((shx + 1023) as u64) << 52) | (((bits & 0x7f_ffff) as u64) << 29));
    let a = [
        f64::from_bits(0x4046a09e6653ba70),
        f64::from_bits(0x406808f46c6116e0),
        f64::from_bits(0x405dca97439cae14),
        f64::from_bits(0x402add70d2827500),
        f64::from_bits(0x3fc4f15f83f55d2d),
        f64::from_bits(0x402d9e20660edb21),
        f64::from_bits(0x4062ff15c0285815),
        f64::from_bits(0x406510d06a8112ce),
        f64::from_bits(0x4040fecbc9e2c375),
    ];
    let fr = ((((a[0] * fr + a[1]) * fr + a[2]) * fr + a[3]) * fr + a[4])
        / ((((a[5] * fr + a[6]) * fr + a[7]) * fr + a[8]) * fr + 1.0);
    // keep the top 23 fraction bits (truncating) with the new exponent
    let mantissa = ((fr.to_bits() & ((1u64 << 52) - 1)) >> 29) as u32;
    f32::from_bits((((ex + 127) as u32) << 23) | mantissa)
}

/// OpenCV's CV_DESCALE: divide by 2^n, rounding to nearest. (`>>` on a signed int floors, like C.)
#[inline]
fn descale(x: i32, n: i32) -> i32 {
    (x + (1 << (n - 1))) >> n
}

#[inline]
fn sat_u8(x: i32) -> u8 {
    x.clamp(0, 255) as u8
}

/// One pixel RGB -> Lab (8-bit OpenCV encoding: L*255/100, a+128, b+128).
#[inline]
fn rgb_to_lab_px(t: &LabTables, gamma: Gamma, rgb: [u8; 3]) -> [u8; 3] {
    let lin = |v: u8| match gamma {
        Gamma::Srgb => t.gamma_srgb[v as usize],
        Gamma::Linear => v as i32 * 8,
    };
    let (r, g, b) = (lin(rgb[0]), lin(rgb[1]), lin(rgb[2]));
    let f =
        |row: &[i32; 3]| t.cbrt[descale(r * row[0] + g * row[1] + b * row[2], LAB_SHIFT) as usize];
    let (fx, fy, fz) = (f(&t.to_xyz[0]), f(&t.to_xyz[1]), f(&t.to_xyz[2]));

    let l_scale = (116 * 255 + 50) / 100;
    let l_shift = -((16 * 255 * (1 << LAB_SHIFT2) + 50) / 100);
    let l = descale(l_scale * fy + l_shift, LAB_SHIFT2);
    let a = descale(500 * (fx - fy) + 128 * (1 << LAB_SHIFT2), LAB_SHIFT2);
    let bb = descale(200 * (fy - fz) + 128 * (1 << LAB_SHIFT2), LAB_SHIFT2);
    [sat_u8(l), sat_u8(a), sat_u8(bb)]
}

/// One pixel Lab -> RGB (OpenCV's bit-exact integer path, `Lab2RGBinteger`).
#[inline]
fn lab_to_rgb_px(t: &LabTables, gamma: Gamma, lab: [u8; 3]) -> [u8; 3] {
    let (l, a, b) = (lab[0] as usize, lab[1] as i32, lab[2] as i32);
    let y = t.l_to_y[l];
    let ify = t.l_to_fy[l];
    // a/500 and b/200 in units of 2^14, via OpenCV's multiply-and-shift approximations
    let adiv = ((5 * a * 53687 + (1 << 7)) >> 13) - 128 * BASE / 500;
    let bdiv = ((b * 41943 + (1 << 4)) >> 9) - 128 * BASE / 200 + 1;
    let x = t.ab_to_xz[(ify + adiv - MIN_AB) as usize];
    let z = t.ab_to_xz[(ify - bdiv - MIN_AB) as usize];

    let mut out = [0u8; 3];
    for (c, row) in t.to_rgb.iter().enumerate() {
        // linear light in 12 bits (0..4095)
        let v = descale(row[0] * x + row[1] * y + row[2] * z, 14).clamp(0, 4095);
        out[c] = match gamma {
            Gamma::Srgb => t.inv_gamma_srgb[v as usize] as u8,
            Gamma::Linear => (((v << 8) - v) >> 12) as u8, // v * 255 / 4096
        };
    }
    out
}

/// Convert an RGB image to 8-bit Lab, like `cv2.cvtColor(img, COLOR_RGB2LAB)` (or LBGR2Lab for `Linear`).
pub fn rgb_to_lab(img: &Rgb8, gamma: Gamma) -> Rgb8 {
    let t = lab_tables();
    let mut out = Rgb8::new(img.width, img.height);
    out.data
        .par_chunks_exact_mut(3)
        .zip(img.data.par_chunks_exact(3))
        .for_each(|(o, p)| {
            o.copy_from_slice(&rgb_to_lab_px(t, gamma, [p[0], p[1], p[2]]));
        });
    out
}

/// Convert 8-bit Lab back to RGB, like `cv2.cvtColor(lab, COLOR_LAB2RGB)` (or Lab2LBGR for `Linear`).
pub fn lab_to_rgb(lab: &Rgb8, gamma: Gamma) -> Rgb8 {
    let t = lab_tables();
    let mut out = Rgb8::new(lab.width, lab.height);
    out.data
        .par_chunks_exact_mut(3)
        .zip(lab.data.par_chunks_exact(3))
        .for_each(|(o, p)| {
            o.copy_from_slice(&lab_to_rgb_px(t, gamma, [p[0], p[1], p[2]]));
        });
    out
}

// ---------------------------------------------------------------------------------------------------------
// YCrCb (fixed point with 14 fractional bits)
// ---------------------------------------------------------------------------------------------------------

/// RGB -> YCrCb, like `cv2.cvtColor(bgr, COLOR_BGR2YCrCb)`. Output channels are Y, Cr, Cb.
pub fn rgb_to_ycrcb(img: &Rgb8) -> Rgb8 {
    let mut out = Rgb8::new(img.width, img.height);
    out.data
        .par_chunks_exact_mut(3)
        .zip(img.data.par_chunks_exact(3))
        .for_each(|(o, p)| {
            let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
            let y = descale(r * 4899 + g * 9617 + b * 1868, 14);
            let cr = descale((r - y) * 11682 + (128 << 14), 14);
            let cb = descale((b - y) * 9241 + (128 << 14), 14);
            o[0] = sat_u8(y);
            o[1] = sat_u8(cr);
            o[2] = sat_u8(cb);
        });
    out
}

/// YCrCb -> RGB, like `cv2.cvtColor(ycc, COLOR_YCrCb2RGB)`.
pub fn ycrcb_to_rgb(img: &Rgb8) -> Rgb8 {
    let mut out = Rgb8::new(img.width, img.height);
    out.data
        .par_chunks_exact_mut(3)
        .zip(img.data.par_chunks_exact(3))
        .for_each(|(o, p)| {
            let (y, cr, cb) = (p[0] as i32, p[1] as i32 - 128, p[2] as i32 - 128);
            o[0] = sat_u8(y + descale(cr * 22987, 14));
            o[1] = sat_u8(y + descale(cb * -5636 + cr * -11698, 14));
            o[2] = sat_u8(y + descale(cb * 29049, 14));
        });
    out
}

// ---------------------------------------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------------------------------------

/// Median filter with a `ksize` x `ksize` window (3 or 5 here), edges repeated outward
/// (`cv2.medianBlur`, which uses BORDER_REPLICATE).
pub fn median_blur(src: &Gray8, ksize: usize) -> Gray8 {
    let (w, h) = (src.width, src.height);
    let r = (ksize / 2) as isize;
    let mut out = Gray8::new(w, h);
    out.data.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        // clamp a coordinate into the image ("replicate" border)
        let cy = |dy: isize| (y as isize + dy).clamp(0, h as isize - 1) as usize;
        let mut window = vec![0u8; ksize * ksize];
        for (x, o) in row.iter_mut().enumerate() {
            let mut n = 0;
            for dy in -r..=r {
                let line = &src.data[cy(dy) * w..][..w];
                for dx in -r..=r {
                    window[n] = line[(x as isize + dx).clamp(0, w as isize - 1) as usize];
                    n += 1;
                }
            }
            let mid = window.len() / 2;
            *o = *window.select_nth_unstable(mid).1;
        }
    });
    out
}

/// Median filter on each channel of a 3-channel image.
pub fn median_blur_rgb(img: &Rgb8, ksize: usize) -> Rgb8 {
    let [r, g, b] = [0, 1, 2].map(|c| median_blur(&img.channel(c), ksize));
    Rgb8::from_planes(&r, &g, &b)
}

/// The rows of OpenCV's elliptical structuring element (`getStructuringElement(MORPH_ELLIPSE, (n, n))`):
/// for each row offset `dy` in `-n/2..=n/2`, the element covers columns `-half..=half`.
fn ellipse_rows(n: usize) -> Vec<(isize, isize)> {
    let r = (n / 2) as isize;
    let inv_r2 = if r > 0 { 1.0 / (r * r) as f64 } else { 0.0 };
    (0..n as isize)
        .map(|i| {
            let dy = i - r;
            // OpenCV: dx = saturate_cast<int>(c * sqrt((r*r - dy*dy) * inv_r2)), j1 = c - dx, j2 = c + dx + 1
            let dx = (r as f64 * (((r * r - dy * dy) as f64) * inv_r2).sqrt()).round_ties_even()
                as isize;
            (dy, dx.min(r))
        })
        .collect()
}

/// Binary dilation of a 0/1 mask by an n x n ellipse (`cv2.dilate`; pixels outside the image count as 0).
///
/// Instead of visiting all ~180 kernel cells per pixel, we keep a running count of set pixels along each
/// row ("prefix sums"); then "is any pixel set in columns x-half..=x+half of row y+dy?" is one subtraction.
pub fn dilate_ellipse(mask: &Gray8, n: usize) -> Gray8 {
    let (w, h) = (mask.width, mask.height);
    // prefix[y][x] = number of set pixels in row y, columns 0..x
    let prefix: Vec<Vec<u32>> = mask
        .data
        .par_chunks(w)
        .map(|row| {
            let mut p = vec![0u32; w + 1];
            for x in 0..w {
                p[x + 1] = p[x] + (row[x] != 0) as u32;
            }
            p
        })
        .collect();
    let rows = ellipse_rows(n);
    let mut out = Gray8::new(w, h);
    out.data.par_chunks_mut(w).enumerate().for_each(|(y, o)| {
        for &(dy, half) in &rows {
            let yy = y as isize + dy;
            if yy < 0 || yy >= h as isize {
                continue;
            }
            let p = &prefix[yy as usize];
            for (x, v) in o.iter_mut().enumerate() {
                if *v == 0 {
                    let x0 = (x as isize - half).max(0) as usize;
                    let x1 = (x as isize + half + 1).min(w as isize) as usize;
                    if p[x1] > p[x0] {
                        *v = 1;
                    }
                }
            }
        }
    });
    out
}

/// Reflect a coordinate into 0..n without repeating the edge sample ("reflect 101": -1 -> 1, n -> n-2).
#[inline]
pub fn reflect101(i: isize, n: usize) -> usize {
    let n = n as isize;
    let mut i = i;
    while i < 0 || i >= n {
        i = if i < 0 { -i } else { 2 * n - 2 - i };
    }
    i as usize
}

/// Gaussian blur of an f32 plane, like `cv2.GaussianBlur(img, (0, 0), sigma)` on float32 data:
/// kernel size round(8*sigma + 1) | 1, reflect-101 border, horizontal pass then vertical pass.
pub fn gaussian_blur_f32(src: &GrayF32, sigma: f64) -> GrayF32 {
    let n = ((sigma * 4.0 * 2.0 + 1.0).round_ties_even() as usize) | 1;
    let half = (n / 2) as isize;
    // the kernel, normalised to sum to 1 (computed in f64, stored as f32 like OpenCV)
    let raw: Vec<f64> = (0..n)
        .map(|i| {
            let x = i as f64 - (n - 1) as f64 * 0.5;
            (-0.5 / (sigma * sigma) * x * x).exp()
        })
        .collect();
    let sum: f64 = raw.iter().sum();
    let k: Vec<f32> = raw.iter().map(|v| (v / sum) as f32).collect();

    let (w, h) = (src.width, src.height);
    let mut tmp = vec![0f32; w * h];
    tmp.par_chunks_mut(w).enumerate().for_each(|(y, o)| {
        let line = &src.data[y * w..][..w];
        for (x, v) in o.iter_mut().enumerate() {
            let mut s = 0f32;
            for (j, kj) in k.iter().enumerate() {
                s += kj * line[reflect101(x as isize + j as isize - half, w)];
            }
            *v = s;
        }
    });
    let mut out = vec![0f32; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, o)| {
        let rows: Vec<&[f32]> = (0..n)
            .map(|j| &tmp[reflect101(y as isize + j as isize - half, h) * w..][..w])
            .collect();
        for (x, v) in o.iter_mut().enumerate() {
            let mut s = 0f32;
            for (kj, row) in k.iter().zip(&rows) {
                s += kj * row[x];
            }
            *v = s;
        }
    });
    GrayF32 {
        width: w,
        height: h,
        data: out,
    }
}

// ---------------------------------------------------------------------------------------------------------
// Resize
// ---------------------------------------------------------------------------------------------------------

/// One entry of OpenCV's area-resize table: source sample `si` contributes `alpha` to output sample `di`.
struct AreaTap {
    di: usize,
    si: usize,
    alpha: f32,
}

/// OpenCV's `computeResizeAreaTab`: for each output position, which source positions it covers and how
/// much of each (partial pixels at both ends of the cell get a fractional weight).
fn area_taps(ssize: usize, dsize: usize, scale: f64) -> Vec<AreaTap> {
    let mut taps = Vec::new();
    for dx in 0..dsize {
        let fsx1 = dx as f64 * scale;
        let fsx2 = fsx1 + scale;
        let cell = scale.min(ssize as f64 - fsx1);
        let sx2 = (fsx2.floor() as usize).min(ssize - 1);
        let sx1 = (fsx1.ceil() as usize).min(sx2);
        if sx1 as f64 - fsx1 > 1e-3 {
            taps.push(AreaTap {
                di: dx,
                si: sx1 - 1,
                alpha: ((sx1 as f64 - fsx1) / cell) as f32,
            });
        }
        for sx in sx1..sx2 {
            taps.push(AreaTap {
                di: dx,
                si: sx,
                alpha: (1.0 / cell) as f32,
            });
        }
        if fsx2 - sx2 as f64 > 1e-3 {
            taps.push(AreaTap {
                di: dx,
                si: sx2,
                alpha: ((fsx2 - sx2 as f64).min(1.0).min(cell) / cell) as f32,
            });
        }
    }
    taps
}

/// `cv2.resize(img, (out_w, out_h), interpolation=cv2.INTER_AREA)` on a float32 3-channel image, for a
/// non-integer shrink (OpenCV's `resizeArea_<float, float>`): each output pixel is the area-weighted
/// average of the source pixels its cell covers. Rows are summed horizontally first, then vertically.
pub fn resize_area_f32(src: &RgbF32, out_w: usize, out_h: usize) -> RgbF32 {
    let (w, h) = (src.width, src.height);
    // cv::resize computes inv_scale = dsize / ssize, then the area code uses scale = 1 / inv_scale
    let scale_x = 1.0 / (out_w as f64 / w as f64);
    let scale_y = 1.0 / (out_h as f64 / h as f64);
    let xtab = area_taps(w, out_w, scale_x);
    let ytab = area_taps(h, out_h, scale_y);
    // group the vertical taps by output row so each row can be computed independently (in parallel)
    let mut rows: Vec<Vec<(usize, f32)>> = vec![Vec::new(); out_h];
    for t in &ytab {
        rows[t.di].push((t.si, t.alpha));
    }
    let mut out = vec![0f32; out_w * out_h * 3];
    out.par_chunks_mut(out_w * 3)
        .zip(rows.par_iter())
        .for_each(|(o, taps)| {
            let mut buf = vec![0f32; out_w * 3];
            for (j, &(sy, beta)) in taps.iter().enumerate() {
                buf.iter_mut().for_each(|v| *v = 0.0);
                let line = &src.data[sy * w * 3..][..w * 3];
                for t in &xtab {
                    for c in 0..3 {
                        buf[t.di * 3 + c] += line[t.si * 3 + c] * t.alpha;
                    }
                }
                for (ov, bv) in o.iter_mut().zip(&buf) {
                    if j == 0 {
                        *ov = beta * bv;
                    } else {
                        *ov += beta * bv;
                    }
                }
            }
        });
    RgbF32 {
        width: out_w,
        height: out_h,
        data: out,
    }
}

// ---------------------------------------------------------------------------------------------------------
// Tests against real cv2 output. Generate the reference files first:
//     rawdev develop --dump-stages --out OUT RAW && tools/ref_primitives.py OUT/stages/<name>/s1_tone.bin W H
// then run:  cargo test --release -- --ignored
// ---------------------------------------------------------------------------------------------------------
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;

    pub fn prim(name: &str) -> Vec<u8> {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("scratch/prim")
            .join(format!("{name}.bin"));
        std::fs::read(&p)
            .unwrap_or_else(|_| panic!("missing {} - run tools/ref_primitives.py", p.display()))
    }

    /// every 8-bit triple as a 4096 x 4096 image: pixel i = (i >> 16, (i >> 8) & 255, i & 255)
    fn grid() -> Rgb8 {
        let data = (0u32..1 << 24)
            .flat_map(|i| [(i >> 16) as u8, (i >> 8) as u8, i as u8])
            .collect();
        Rgb8::from_vec(4096, 4096, data)
    }

    fn count_mismatch(a: &[u8], b: &[u8]) -> usize {
        assert_eq!(a.len(), b.len());
        a.iter().zip(b).filter(|(x, y)| x != y).count()
    }

    #[test]
    #[ignore]
    fn colour_conversions_match_cv2() {
        let g = grid();
        assert_eq!(
            count_mismatch(&rgb_to_lab(&g, Gamma::Srgb).data, &prim("lab_srgb")),
            0,
            "RGB2LAB"
        );
        assert_eq!(
            count_mismatch(&rgb_to_lab(&g, Gamma::Linear).data, &prim("lab_lin")),
            0,
            "LBGR2Lab"
        );
        assert_eq!(
            count_mismatch(
                &lab_to_rgb(&g, Gamma::Srgb).data,
                &prim("rgb_from_lab_srgb")
            ),
            0,
            "LAB2RGB"
        );
        assert_eq!(
            count_mismatch(
                &lab_to_rgb(&g, Gamma::Linear).data,
                &prim("rgb_from_lab_lin")
            ),
            0,
            "Lab2LBGR"
        );
        assert_eq!(
            count_mismatch(&rgb_to_ycrcb(&g).data, &prim("ycc")),
            0,
            "BGR2YCrCb"
        );
        assert_eq!(
            count_mismatch(&ycrcb_to_rgb(&g).data, &prim("rgb_from_ycc")),
            0,
            "YCrCb2RGB"
        );
    }

    pub fn crop() -> Rgb8 {
        Rgb8::from_vec(600, 400, prim("crop"))
    }

    #[test]
    #[ignore]
    fn filters_match_cv2() {
        let c = crop();
        assert_eq!(
            count_mismatch(&median_blur_rgb(&c, 3).data, &prim("median3")),
            0,
            "median 3"
        );
        assert_eq!(
            count_mismatch(&median_blur(&c.channel(1), 5).data, &prim("median5")),
            0,
            "median 5"
        );
        let core = Gray8::from_vec(600, 400, prim("core"));
        assert!(core.data.iter().any(|&v| v != 0), "test mask is empty");
        assert_eq!(
            count_mismatch(&dilate_ellipse(&core, 15).data, &prim("dilate15")),
            0,
            "dilate"
        );

        let f: Vec<f32> = prim("gauss_in")
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        let want: Vec<f32> = prim("gauss15")
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        let got = gaussian_blur_f32(
            &GrayF32 {
                width: 600,
                height: 400,
                data: f,
            },
            1.5,
        );
        let worst = got
            .data
            .iter()
            .zip(&want)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(worst < 1e-5, "gaussian differs by {worst}");
    }
}
