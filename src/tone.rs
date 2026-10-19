//! The tone stage: linear 16-bit RAW -> 8-bit sRGB, before noise reduction.
//!
//! Steps (same order and maths as develop.py):
//! 1. normalise by the brightest sample, then clip every channel at green's saturation level;
//! 2. fit a tone curve so the RAW's brightness distribution (on the max-RGB "norm") matches the camera's own
//!    JPEG, with a soft shoulder up to the RAW clip point, x0.97 so night stays night;
//! 3. apply it by scaling R, G, B by the same factor (keeps hue and saturation of lit signs);
//! 4. sRGB encode, a light vibrance, round to 8 bits.
//!
//! The speed trick: LibRaw gives 16-bit integers, so a channel can only take 65,536 values. Everything in
//! steps 1-3 that depends on one sample (normalising, clipping, the curve, the scale factor) is computed
//! once per *possible value* into lookup tables (LUTs), and the per-pixel work is a few table reads.

use crate::decode::Linear16;
use crate::image::{Rgb8, RgbF32};
use crate::lookfile::NeutralCurve;
use rayon::prelude::*;

/// What the tone stage measured (printed in the log, like the Python's `clipG=... joint r=...`).
pub struct ToneInfo {
    pub clip_g: f64,
    pub curve: String,
    /// the fitted curve itself, sampled at `lut_x()` (kept for parity checks)
    pub lut: Vec<f32>,
}

/// The x positions of the tone-curve LUT: 0, then 4095 points spaced evenly in log from 1e-5 to 1
/// (numpy: `np.concatenate([[0], np.geomspace(1e-5, 1, 4095)]).astype(np.float32)`).
pub fn lut_x() -> Vec<f32> {
    let n = 4095;
    let (lo, hi) = (-5.0f64, 0.0f64);
    let step = (hi - lo) / (n - 1) as f64;
    let mut x = vec![0f32];
    for i in 0..n {
        let v = if i == 0 {
            1e-5
        } else if i == n - 1 {
            1.0
        } else {
            10f64.powf(i as f64 * step + lo)
        };
        x.push(v as f32);
    }
    x
}

/// `numpy.percentile(data, q)` (default "linear" method) on data that is already sorted.
fn percentile_sorted(sorted: &[f32], q: f64) -> f64 {
    let n = sorted.len();
    let qf = q / 100.0;
    // numpy's "virtual index" for the linear method: n*q + (1 - q) - 1 (= q * (n - 1))
    let vi = n as f64 * qf + (1.0 - qf) - 1.0;
    if vi >= (n - 1) as f64 {
        return sorted[n - 1] as f64;
    }
    let lo = vi.floor();
    let t = vi - lo;
    let (a, b) = (sorted[lo as usize], sorted[lo as usize + 1]);
    let diff = (b - a) as f64; // numpy subtracts in the data's own type (f32)
    // numpy's _lerp: interpolate from whichever end is nearer
    if t >= 0.5 {
        b as f64 - diff * (1.0 - t)
    } else {
        a as f64 + diff * t
    }
}

/// `numpy.interp(x, xp, fp)` for one value (xp increasing).
fn interp(x: f64, xp: &[f64], fp: &[f64]) -> f64 {
    let n = xp.len();
    if x < xp[0] {
        return fp[0];
    }
    if x >= xp[n - 1] {
        return fp[n - 1];
    }
    // index of the last xp <= x
    let j = xp.partition_point(|&v| v <= x) - 1;
    if xp[j] == x {
        return fp[j];
    }
    let slope = (fp[j + 1] - fp[j]) / (xp[j + 1] - xp[j]);
    slope * (x - xp[j]) + fp[j]
}

/// Fit the tone curve (develop.py's `fit_curve`). `yr` = RAW max-RGB samples, `yc` = camera JPEG max-RGB
/// (linearised). Returns the curve sampled at `lut_x()` and a short description.
pub fn fit_curve(yr: &mut [f32], yc: &mut [f32], night: f64) -> (Vec<f32>, String) {
    let lx = lut_x();
    yr.par_sort_unstable_by(f32::total_cmp);
    yc.par_sort_unstable_by(f32::total_cmp);

    // 300 matching quantiles, 1% .. 99.8%  (numpy linspace: i * step + start, last one exact)
    let step = (99.8 - 1.0) / 299.0;
    let qs: Vec<f64> = (0..300)
        .map(|i| {
            if i == 299 {
                99.8
            } else {
                i as f64 * step + 1.0
            }
        })
        .collect();
    let r_all: Vec<f64> = qs.iter().map(|&q| percentile_sorted(yr, q)).collect();
    let c_all: Vec<f64> = qs.iter().map(|&q| percentile_sorted(yc, q)).collect();

    // keep the trustworthy middle: camera not clipping, RAW not clipping, RAW not pure black
    let mut pairs: Vec<(f64, f64)> = r_all
        .iter()
        .zip(&c_all)
        .filter(|&(&r, &c)| c < 0.80 && r < 0.97 && r > 1e-5)
        .map(|(&r, &c)| (r, c))
        .collect();
    // np.unique(r, return_index=True): sorted unique r values, each with its *first* camera value
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0)); // stable, so equal r keep their order
    pairs.dedup_by(|later, earlier| later.0 == earlier.0);
    let r: Vec<f64> = pairs.iter().map(|p| p.0).collect();
    let mut cc: Vec<f64> = pairs.iter().map(|p| p.1).collect();
    running_max(&mut cc);

    if r.len() < 8 {
        // degenerate: fall back to the identity
        return (
            lx.iter().map(|v| v.clamp(0.0, 1.0)).collect(),
            "linear".to_string(),
        );
    }

    // smooth the camera curve in log-log space with a 9-tap moving average (edges repeated)
    let lr: Vec<f64> = r.iter().map(|v| v.ln()).collect();
    let lc_raw: Vec<f64> = cc.iter().map(|v| v.max(1e-6).ln()).collect();
    let k = 9;
    let pad = k / 2;
    let n = lc_raw.len();
    let at = |i: isize| lc_raw[i.clamp(0, n as isize - 1) as usize];
    let mut lc: Vec<f64> = (0..n as isize)
        .map(|i| {
            (0..k as isize)
                .map(|j| at(i + j - pad as isize) * (1.0 / k as f64))
                .sum()
        })
        .collect();
    running_max(&mut lc);

    // the shoulder: from the last fitted point (rk -> ck) a monotone Hermite curve up to (1, 1)
    let rk = r[n - 1];
    let ck = lc[n - 1].exp();
    let j = n.saturating_sub(12);
    let s0 = (ck - lc[j].exp()) / (rk - r[j]).max(1e-6);
    let (dx, dy) = (1.0 - rk, 1.0 - ck);
    let m0 = (s0 * dx).max(0.0).min(2.5 * dy);
    let m1 = 0.25 * dy;

    let mut out: Vec<f64> = lx
        .iter()
        .map(|&xf| {
            let x = xf as f64;
            if x < r[0] {
                x * (lc[0].exp() / r[0]) // straight line from black to the first fitted point
            } else if x <= rk {
                interp(x.ln(), &lr, &lc).exp()
            } else {
                let u = (x - rk) / dx;
                let (u2, u3) = (u.powi(2), u.powi(3));
                (2.0 * u3 - 3.0 * u2 + 1.0) * ck
                    + (u3 - 2.0 * u2 + u) * m0
                    + (-2.0 * u3 + 3.0 * u2) * 1.0
                    + (u3 - u2) * m1
            }
        })
        .collect();
    running_max(&mut out);
    let lut = out
        .iter()
        .map(|v| (v * night).clamp(0.0, 1.0) as f32)
        .collect();
    (lut, format!("joint r={rk:.3}->{ck:.3}"))
}

/// The fixed fallback curve (no camera JPEG, no usable preview), sampled at `lut_x()` and scaled by
/// `night` like a fitted curve: `y = gain * x` up to the knee, then the same monotone Hermite shoulder
/// the fit uses, up to (1, 1).
pub fn neutral_curve(p: NeutralCurve, night: f64) -> (Vec<f32>, String) {
    let rk = p.knee / p.gain;
    let ck = p.knee;
    let (dx, dy) = (1.0 - rk, 1.0 - ck);
    let m0 = (p.gain * dx).min(2.5 * dy);
    let m1 = 0.25 * dy;
    let mut out: Vec<f64> = lut_x()
        .iter()
        .map(|&xf| {
            let x = xf as f64;
            if x <= rk {
                x * p.gain
            } else {
                let u = (x - rk) / dx;
                let (u2, u3) = (u.powi(2), u.powi(3));
                (2.0 * u3 - 3.0 * u2 + 1.0) * ck
                    + (u3 - 2.0 * u2 + u) * m0
                    + (-2.0 * u3 + 3.0 * u2) * 1.0
                    + (u3 - u2) * m1
            }
        })
        .collect();
    running_max(&mut out);
    let lut = out
        .iter()
        .map(|v| (v * night).clamp(0.0, 1.0) as f32)
        .collect();
    (lut, format!("neutral g={:.2} knee={:.2}", p.gain, p.knee))
}

/// In-place cumulative maximum (numpy's `np.maximum.accumulate`): makes a sequence non-decreasing.
fn running_max(v: &mut [f64]) {
    for i in 1..v.len() {
        if v[i] < v[i - 1] {
            v[i] = v[i - 1];
        }
    }
}

/// sRGB encode (linear 0..1 -> 0..1), sampled finely enough that linear interpolation between
/// entries is accurate to ~1e-7 (far below one 8-bit level).
struct SrgbLut {
    table: Vec<f32>,
}

impl SrgbLut {
    const N: usize = 1 << 16;

    fn new() -> SrgbLut {
        let table = (0..=Self::N)
            .map(|i| {
                let x = i as f64 / Self::N as f64;
                let g = if x <= 0.0031308 {
                    12.92 * x
                } else {
                    1.055 * x.powf(1.0 / 2.4) - 0.055
                };
                g as f32
            })
            .collect();
        SrgbLut { table }
    }

    #[inline]
    fn encode(&self, x: f32) -> f32 {
        let pos = x * Self::N as f32;
        let i = (pos as usize).min(Self::N - 1);
        let t = pos - i as f32;
        self.table[i] + (self.table[i + 1] - self.table[i]) * t
    }
}

/// The per-code tables shared by the night (fused) and day (float) tone paths.
struct Prepared {
    /// lin[v] = the final linear value of a channel whose 16-bit code is v
    lin: Vec<f32>,
    /// ratio[v] = curve(Y) / Y for a pixel whose largest code is v
    ratio: Vec<f32>,
    info: ToneInfo,
}

/// Steps 1-3 as lookup tables: normalise, green clip, fit the curve (or use the neutral one when `cam` is
/// `None`), per-code scale factor.
fn prepare(raw: &Linear16, cam: Option<&Rgb8>, night: f64, neutral: NeutralCurve) -> Prepared {
    let (w, h) = (raw.width, raw.height);
    let m = raw.maxval as f32;

    // 1. normalise: L = (v / maxval) / max(L), done in f32 like numpy
    let max_code = raw.data.par_iter().copied().max().unwrap_or(1);
    let lmax = (max_code as f32 / m).max(1e-6);
    let norm: Vec<f32> = (0..=u16::MAX as u32)
        .map(|v| (v as f32 / m) / lmax)
        .collect();

    // -H 0 can leave green saturating a little below red/blue; find where green clips (90th percentile
    // of G among pixels whose R and B are clipped) and clip every channel there.
    let mut g_clipped: Vec<f32> = raw
        .data
        .par_chunks_exact(3)
        .filter(|p| norm[p[0] as usize] > 0.995 && norm[p[2] as usize] > 0.995)
        .map(|p| norm[p[1] as usize])
        .collect();
    let clip_g = if g_clipped.len() > 50 {
        g_clipped.par_sort_unstable_by(f32::total_cmp);
        percentile_sorted(&g_clipped, 90.0)
    } else {
        1.0
    };
    let clip_g = clip_g.clamp(0.6, 1.0);
    let cl = clip_g as f32;
    let lin: Vec<f32> = norm.iter().map(|&v| v.min(cl) / cl).collect();

    // 2. tone curve fitted to the camera JPEG, on the max-RGB norm (every 6th pixel each way)
    let Some(cam) = cam else {
        let (lut, curve) = neutral_curve(neutral, night);
        return finish_prepare(lin, lut, curve, clip_g);
    };
    let mut yr: Vec<f32> = (0..h)
        .step_by(6)
        .flat_map(|y| (0..w).step_by(6).map(move |x| (y, x)))
        .map(|(y, x)| {
            let p = &raw.data[(y * w + x) * 3..][..3];
            lin[p[0].max(p[1]).max(p[2]) as usize]
        })
        .collect();
    let srgb_to_lin = srgb_decode_table();
    let mut yc: Vec<f32> = cam
        .data
        .chunks_exact(3)
        .map(|p| {
            srgb_to_lin[p[0] as usize]
                .max(srgb_to_lin[p[1] as usize])
                .max(srgb_to_lin[p[2] as usize])
        })
        .collect();
    let (lut, curve) = fit_curve(&mut yr, &mut yc, night);
    finish_prepare(lin, lut, curve, clip_g)
}

/// 3. per-code scale factor
fn finish_prepare(lin: Vec<f32>, lut: Vec<f32>, curve: String, clip_g: f64) -> Prepared {
    let lx: Vec<f64> = lut_x().iter().map(|&v| v as f64).collect();
    let ly: Vec<f64> = lut.iter().map(|&v| v as f64).collect();
    let ratio: Vec<f32> = lin
        .par_iter()
        .map(|&y| {
            let yt = interp(y as f64, &lx, &ly) as f32;
            yt / y.max(1e-6)
        })
        .collect();
    Prepared {
        lin,
        ratio,
        info: ToneInfo { clip_g, curve, lut },
    }
}

/// 8-bit sRGB code -> linear light, in f32 like numpy:
/// `np.where(c <= .04045, c / 12.92, ((c + .055) / 1.055) ** 2.4)` with `c = v / 255`.
pub fn srgb_decode_table() -> Vec<f32> {
    (0..256)
        .map(|i| {
            let c = i as f32 / 255f32;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        })
        .collect()
}

/// The night tone stage, fused into one pass straight to 8 bits (the fast path: the sRGB encode is a
/// lookup table). `cam` is the camera JPEG already shrunk to 800 px (see `develop.rs`); `night` scales
/// the fitted curve and `vibrance` is the look's saturation boost (`[tone]` in the look file).
pub fn tone_with(
    raw: &Linear16,
    cam: Option<&Rgb8>,
    night: f64,
    vibrance: f32,
    neutral: NeutralCurve,
) -> (Rgb8, ToneInfo) {
    let (w, h) = (raw.width, raw.height);
    let Prepared { lin, ratio, info } = prepare(raw, cam, night, neutral);

    // 4. per pixel: scale, sRGB encode, vibrance, round to 8 bits
    let srgb = SrgbLut::new();
    let mut out = Rgb8::new(w, h);
    out.data
        .par_chunks_exact_mut(3)
        .zip(raw.data.par_chunks_exact(3))
        .for_each(|(o, p)| {
            let k = ratio[p[0].max(p[1]).max(p[2]) as usize];
            let g = [0, 1, 2].map(|c| srgb.encode((lin[p[c] as usize] * k).clamp(0.0, 1.0)));
            // light vibrance (+6% saturation in the rich looks), tapering off for already-saturated colours
            let gray = g[0] * 0.2126 + g[1] * 0.7152 + g[2] * 0.0722;
            let sat = g[0].max(g[1]).max(g[2]) - g[0].min(g[1]).min(g[2]);
            let boost = 1.0 + vibrance * (1.0 - sat);
            for c in 0..3 {
                let v = gray + (g[c] - gray) * boost;
                o[c] = (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            }
        });
    (out, info)
}

/// Steps 1-3 only, keeping the result as linear-light f32 RGB (`rgb = clip(L * ratio, 0, 1)` in the
/// Python). The day recipe edits this (colour matrix, shadow lift) before calling [`encode_srgb`].
pub fn tone_linear(
    raw: &Linear16,
    cam: Option<&Rgb8>,
    night: f64,
    neutral: NeutralCurve,
) -> (RgbF32, ToneInfo) {
    let Prepared { lin, ratio, info } = prepare(raw, cam, night, neutral);
    let mut data = vec![0f32; raw.data.len()];
    data.par_chunks_exact_mut(3)
        .zip(raw.data.par_chunks_exact(3))
        .for_each(|(o, p)| {
            let k = ratio[p[0].max(p[1]).max(p[2]) as usize];
            for c in 0..3 {
                o[c] = (lin[p[c] as usize] * k).clamp(0.0, 1.0);
            }
        });
    (
        RgbF32 {
            width: raw.width,
            height: raw.height,
            data,
        },
        info,
    )
}

/// sRGB encode + the light vibrance + round to 8 bits, on a float image. Unlike the night path this
/// calls `powf` per sample (no lookup table), which is what numpy does, so it rounds the same way.
pub fn encode_srgb(img: &RgbF32, vibrance: f32) -> Rgb8 {
    let inv_gamma = (1.0f64 / 2.4) as f32;
    let mut out = Rgb8::new(img.width, img.height);
    out.data
        .par_chunks_exact_mut(3)
        .zip(img.data.par_chunks_exact(3))
        .for_each(|(o, p)| {
            let g = [0, 1, 2].map(|c| {
                let x = p[c];
                if x <= 0.0031308 {
                    12.92 * x
                } else {
                    1.055 * x.max(0.0).powf(inv_gamma) - 0.055
                }
            });
            let gray = g[0] * 0.2126 + g[1] * 0.7152 + g[2] * 0.0722;
            let sat = g[0].max(g[1]).max(g[2]) - g[0].min(g[1]).min(g[2]);
            let boost = 1.0 + vibrance * (1.0 - sat);
            for c in 0..3 {
                let v = gray + (g[c] - gray) * boost;
                o[c] = (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            }
        });
    out
}
