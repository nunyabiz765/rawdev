//! The day recipe's extra stages (ported from the Python day prototype `develop.py`, which is the same
//! engine as the night recipe with these changes):
//!
//! - `-H 2` highlight blend for some frames (`--recover-hl`), see `decode::Highlights`;
//! - no night pull (curve x1.0);
//! - **sky matrix** (`--match`): fit a 3x3 colour matrix to the camera JPEG on blue-sky pixels only, and
//!   blend it in through a soft blue-hue mask, so skies read like the camera's (no lavender) while everything
//!   else keeps the RAW colour;
//! - **shadow lift** (`--shadow-lift`): a gentle gamma-0.8 lift in linear light below ~51% sRGB;
//! - NR only above ISO 200 (`[nr] off_max_iso` in the look);
//! - the chroma lift skips what the sky matrix already handled;
//! - the blown-light ring defringe always on, plus a **CA fringe pass** on high-contrast edges.
//!
//! No rotation and no cropping, ever: there is deliberately no code for either here.
//!
//! The numbers (thresholds, strengths, blur sizes) come from the look file's `[sky_match]`,
//! `[shadow_lift]` and `[ca_fringe]` tables; see `lookfile.rs` and `looks/rich-day.toml`.

use crate::image::{GrayF32, Rgb8, RgbF32};
use crate::look::to_u8;
use crate::lookfile;
use crate::opencv::{self, Gamma};
use crate::pillow::{self, Filter};
use crate::tone;
use rayon::prelude::*;

/// Per-frame choices for the day recipe (develop.py's MATCH / SHADOW_LIFT / RECOVER_HL sets).
#[derive(Clone, Copy, Debug, Default)]
pub struct DayOptions {
    /// fit the sky colour matrix to the camera JPEG
    pub match_sky: bool,
    /// gentle shadow lift in linear light
    pub shadow_lift: bool,
    /// decode with `-H 2` (blend) instead of `-H 0` (clip)
    pub recover_hl: bool,
}

/// develop.py's `blue(x)`: how much blue sticks out above red and green, relative to brightness.
#[inline]
fn blueness(p: &[f32]) -> f32 {
    (p[2] - p[0].max(p[1])) / p[0].max(p[1]).max(p[2]).max(1e-4)
}

/// Solve the least-squares problem `a @ m ~= b` for a 3x3 `m` (numpy's `lstsq`, which works in f64),
/// via the normal equations `(aT a) m = aT b`. Returns `None` if there are too few / degenerate samples.
fn lstsq3(a: &[[f64; 3]], b: &[[f64; 3]]) -> Option<[[f64; 3]; 3]> {
    if a.len() < 3 {
        return None;
    }
    let mut ata = [[0f64; 3]; 3];
    let mut atb = [[0f64; 3]; 3];
    for (ra, rb) in a.iter().zip(b) {
        for i in 0..3 {
            for j in 0..3 {
                ata[i][j] += ra[i] * ra[j];
                atb[i][j] += ra[i] * rb[j];
            }
        }
    }
    // Gauss-Jordan elimination with partial pivoting on [ata | atb]
    for col in 0..3 {
        let piv = (col..3).max_by(|&x, &y| ata[x][col].abs().total_cmp(&ata[y][col].abs()))?;
        if ata[piv][col].abs() < 1e-12 {
            return None;
        }
        ata.swap(col, piv);
        atb.swap(col, piv);
        for row in 0..3 {
            if row != col {
                let f = ata[row][col] / ata[col][col];
                for k in 0..3 {
                    ata[row][k] -= f * ata[col][k];
                    atb[row][k] -= f * atb[col][k];
                }
            }
        }
    }
    let mut m = [[0f64; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            m[i][j] = atb[i][j] / ata[i][i];
        }
    }
    Some(m)
}

/// The sky matrix. `cam_full` is the camera's full-size JPEG (or embedded preview). Changes `rgb` in place and returns the blue mask
/// `w` (the chroma lift needs it) and the matrix, or `None` if there was no usable sky.
pub fn match_sky(
    rgb: &mut RgbF32,
    cam_full: &Rgb8,
    p: &lookfile::SkyMatch,
) -> Option<(GrayF32, [[f32; 3]; 3])> {
    // 1. both images at 600 x 400: the camera's with PIL's BOX filter, ours with cv2 INTER_AREA
    //    (the Python squashes both to 600 x 400 whatever their aspect, so we do too)
    let cam = pillow::resize(
        cam_full,
        600,
        400,
        Filter::Box,
        [0.0, 0.0, cam_full.width as f32, cam_full.height as f32],
    );
    let dec = tone::srgb_decode_table();
    let rl = opencv::resize_area_f32(rgb, 600, 400);
    let mut a = Vec::new();
    let mut b = Vec::new();
    for (cp, rp) in cam.data.chunks_exact(3).zip(rl.data.chunks_exact(3)) {
        let c = [
            dec[cp[0] as usize],
            dec[cp[1] as usize],
            dec[cp[2] as usize],
        ];
        let cmax = c[0].max(c[1]).max(c[2]);
        let cmin = c[0].min(c[1]).min(c[2]);
        let rmax = rp[0].max(rp[1]).max(rp[2]);
        let rmin = rp[0].min(rp[1]).min(rp[2]);
        if cmax < 0.95
            && rmax < 0.95
            && cmin > 0.005
            && rmin > 0.005
            && blueness(rp) > p.sample_blueness
        {
            a.push([rp[0] as f64, rp[1] as f64, rp[2] as f64]);
            b.push([c[0] as f64, c[1] as f64, c[2] as f64]);
        }
    }
    let m64 = lstsq3(&a, &b)?;
    let m = m64.map(|row| row.map(|v| v as f32));

    // 2. the soft blue mask at full size: clip((blue - 0.12) / 0.15, 0, 1), Gaussian sigma 3
    let raw_mask: Vec<f32> = rgb
        .data
        .par_chunks_exact(3)
        .map(|px| ((blueness(px) - p.mask_start) / p.mask_width).clamp(0.0, 1.0))
        .collect();
    let w = opencv::gaussian_blur_f32(
        &GrayF32 {
            width: rgb.width,
            height: rgb.height,
            data: raw_mask,
        },
        p.mask_sigma,
    );

    // 3. rgb + (rgb @ M - rgb) * w
    rgb.data
        .par_chunks_exact_mut(3)
        .zip(w.data.par_iter())
        .for_each(|(p, &wv)| {
            let src = [p[0], p[1], p[2]];
            for j in 0..3 {
                let mj = src[0] * m[0][j] + src[1] * m[1][j] + src[2] * m[2][j];
                p[j] = (src[j] + (mj - src[j]) * wv).clamp(0.0, 1.0);
            }
        });
    Some((w, m))
}

/// Gentle shadow lift in linear light: values get a gamma-0.8 boost (at most x1.5), fading smoothly to
/// no change at linear 0.18 (about 51% sRGB). Hue is kept because R, G, B are scaled together.
pub fn shadow_lift(rgb: &mut RgbF32, s: &lookfile::ShadowLift) {
    rgb.data.par_chunks_exact_mut(3).for_each(|p| {
        let y = p[0].max(p[1]).max(p[2]);
        let ratio = if y > 1e-5 {
            let yc = y.clamp(1e-5, 1.0);
            (yc.powf(s.gamma) / yc).min(s.max_boost)
        } else {
            1.0
        };
        let t = (y / s.threshold).clamp(0.0, 1.0);
        let scale = 1.0 + (ratio - 1.0) * (1.0 - t * t);
        for v in p.iter_mut() {
            *v = (*v * scale).clamp(0.0, 1.0);
        }
    });
}

/// CA fringe pass: purple/magenta at high-contrast edges (wing tips, branch edges against sky).
/// Edge strength from the Lab L gradient (numpy's `np.gradient`), purpleness from a/b, the product
/// blurred (sigma 1.5) and x0.7; a is pulled to neutral by that, b by 40% of it.
pub fn ca_fringe(img: &Rgb8, c: &lookfile::CaFringe) -> Rgb8 {
    let (w, h) = (img.width, img.height);
    let mut lab = opencv::rgb_to_lab(img, Gamma::Srgb);
    let l = |x: usize, y: usize| lab.data[(y * w + x) * 3] as f32;
    // np.gradient: central difference / 2 inside, one-sided difference at the edges
    let grad = |i: usize, n: usize, f: &dyn Fn(usize) -> f32| -> f32 {
        if n < 2 {
            0.0
        } else if i == 0 {
            f(1) - f(0)
        } else if i == n - 1 {
            f(n - 1) - f(n - 2)
        } else {
            (f(i + 1) - f(i - 1)) / 2.0
        }
    };
    let mask: Vec<f32> = (0..w * h)
        .into_par_iter()
        .map(|i| {
            let (x, y) = (i % w, i / w);
            let px = &lab.data[i * 3..][..3];
            let a = px[1] as f32 - 128.0;
            let b = px[2] as f32 - 128.0;
            let purple = ((a - c.a_start) / c.a_width).clamp(0.0, 1.0)
                * ((c.b_end - b) / c.b_width).clamp(0.0, 1.0);
            if purple == 0.0 {
                return 0.0;
            }
            let gx = grad(x, w, &|xx| l(xx, y));
            let gy = grad(y, h, &|yy| l(x, yy));
            let edge = ((gx * gx + gy * gy).sqrt() / c.edge_scale).clamp(0.0, 1.0);
            edge * purple
        })
        .collect();
    let k = opencv::gaussian_blur_f32(
        &GrayF32 {
            width: w,
            height: h,
            data: mask,
        },
        c.blur_sigma,
    );
    lab.data
        .par_chunks_exact_mut(3)
        .zip(k.data.par_iter())
        .for_each(|(px, &kv)| {
            let kk = kv * c.strength;
            let a = px[1] as f32 - 128.0;
            let b = px[2] as f32 - 128.0;
            px[1] = to_u8(128.0 + a * (1.0 - kk));
            px[2] = to_u8(128.0 + b * (1.0 - kk * c.b_factor));
        });
    opencv::lab_to_rgb(&lab, Gamma::Srgb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lstsq_recovers_a_known_matrix() {
        let m = [[0.9, 0.05, 0.02], [0.1, 1.1, -0.03], [-0.02, 0.04, 0.95]];
        let a: Vec<[f64; 3]> = (0..50)
            .map(|i| {
                let t = i as f64 / 50.0;
                [0.1 + 0.3 * t, 0.2 + 0.5 * t * t, 0.6 - 0.2 * t]
            })
            .collect();
        let b: Vec<[f64; 3]> = a
            .iter()
            .map(|r| [0, 1, 2].map(|j| r[0] * m[0][j] + r[1] * m[1][j] + r[2] * m[2][j]))
            .collect();
        let got = lstsq3(&a, &b).unwrap();
        for i in 0..3 {
            for j in 0..3 {
                assert!((got[i][j] - m[i][j]).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn shadow_lift_leaves_brights_alone() {
        let mut img = RgbF32 {
            width: 2,
            height: 1,
            data: vec![0.5, 0.4, 0.3, 0.01, 0.01, 0.01],
        };
        let look = lookfile::Look::load("rich-day").unwrap();
        shadow_lift(&mut img, look.shadow_lift.as_ref().unwrap());
        assert_eq!(&img.data[..3], &[0.5, 0.4, 0.3]);
        assert!(img.data[3] > 0.01 && img.data[3] <= 0.015 + 1e-6);
    }
}
