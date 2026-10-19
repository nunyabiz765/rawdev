//! Non-local means (NLM) denoising, written from scratch.
//!
//! The idea: to clean up a pixel, look at every pixel in a 21x21 neighbourhood ("search window") and ask
//! how similar the 7x7 patch around it is to the 7x7 patch around ours. Similar patches get a big weight,
//! different ones a tiny weight, and the new pixel is the weighted average of all the candidates' centres.
//! Noise averages out, while edges and texture survive because only patches that *look alike* get mixed.
//!
//! This is the same algorithm as `cv2.fastNlMeansDenoising` (7x7 template, 21x21 search), with the same
//! integer patch distance, the same weight table (scaled by 19096 and rounded), and the same rounding when
//! dividing by the weight sum, so the output is meant to match cv2 sample for sample. It is organised
//! differently, though, to run well on many cores:
//!
//! * the image is cut into tiles (32 rows x 256 columns); rayon spreads the tiles over all threads;
//! * inside a tile we loop over the 441 search offsets on the outside. For one offset (dy, dx), every
//!   pixel's patch distance is a 7x7 box-sum of the *same* squared-difference image, which we get with
//!   running sums: a vertical 7-sum per column, then a sliding horizontal 7-sum. That's a handful of
//!   additions per pixel per offset instead of 49 multiplies.

use crate::image::{Gray8, Rgb8};
use crate::opencv::{self, Gamma, reflect101};
use rayon::prelude::*;

const T_HALF: usize = 3; // template (patch) is 7x7
const S_HALF: usize = 10; // search window is 21x21
const SEARCH: usize = 2 * S_HALF + 1;
const PAD: usize = S_HALF + T_HALF; // border needed so every patch we read exists
const TILE_H: usize = 32;
const TILE_W: usize = 256;

/// A plane with a PAD-pixel border added on all sides by mirroring (OpenCV's BORDER_REFLECT_101).
pub(crate) struct Padded {
    pub(crate) width: usize,
    pub(crate) data: Vec<u8>,
}

pub(crate) fn pad(p: &Gray8) -> Padded {
    let (w, h) = (p.width, p.height);
    let pw = w + 2 * PAD;
    let ph = h + 2 * PAD;
    let mut data = vec![0u8; pw * ph];
    data.par_chunks_mut(pw).enumerate().for_each(|(py, row)| {
        let sy = reflect101(py as isize - PAD as isize, h);
        let src = &p.data[sy * w..][..w];
        for (px, v) in row.iter_mut().enumerate() {
            *v = src[reflect101(px as isize - PAD as isize, w)];
        }
    });
    Padded { width: pw, data }
}

/// Weight for each "almost average" patch distance (sum of squared differences >> 6), like OpenCV's
/// `almost_dist2weight_`. Weights fall with distance; the table stops at the first zero, and any larger
/// distance is clamped onto that last (zero) entry.
pub(crate) fn weight_table(h: f32, channels: usize) -> Vec<u32> {
    let fixed_point_mult = (i32::MAX / (SEARCH * SEARCH * 255) as i32) as f64; // 19096
    // OpenCV divides the 49-pixel patch sum by 64 (a shift) and corrects with 64/49 here.
    let shift_fix = 64.0 / 49.0;
    let denom = (h * h * channels as f32) as f64;
    let almost_max = ((255 * 255 * channels) as f64 / shift_fix + 1.0) as usize;
    let mut table = Vec::new();
    for almost in 0..almost_max {
        let dist = almost as f64 * shift_fix;
        let w = (-dist / denom).exp();
        let mut weight = (fixed_point_mult * w).round_ties_even() as u32;
        if (weight as f64) < 0.001 * fixed_point_mult {
            weight = 0;
        }
        table.push(weight);
        if weight == 0 {
            break;
        }
    }
    table
}

/// Scratch buffers reused for every tile a thread works on.
struct Scratch<const C: usize> {
    dist: Vec<i32>,     // squared differences around the tile, (th+6) x (tw+6)
    col: Vec<i32>,      // vertical 7-sums, th x (tw+6)
    wrow: Vec<u32>,     // weights for one row of the tile, for the current offset
    wsum: Vec<u32>,     // sum of weights per pixel
    est: [Vec<u32>; C], // sum of weight * value per pixel, per channel
}

/// Denoise C planes together (C = 1 for luma, 2 for the a/b pair): the patch distance adds up the squared
/// differences of all C channels, and each channel is averaged with the shared weights.
/// `h` is the filter strength (bigger = smoother), as in cv2.
pub fn denoise<const C: usize>(planes: [&Gray8; C], h: f32) -> [Gray8; C] {
    let (w, ht) = (planes[0].width, planes[0].height);
    let padded: [Padded; C] = planes.map(pad);
    let table = weight_table(h, C);

    // One task per band of TILE_H rows; each band walks its tiles left to right.
    let bands: Vec<[Vec<u8>; C]> = (0..ht.div_ceil(TILE_H))
        .into_par_iter()
        .map(|band| {
            let y0 = band * TILE_H;
            let th = TILE_H.min(ht - y0);
            let mut out: [Vec<u8>; C] = std::array::from_fn(|_| vec![0u8; th * w]);
            let mut s = Scratch::<C> {
                dist: Vec::new(),
                col: Vec::new(),
                wrow: Vec::new(),
                wsum: Vec::new(),
                est: std::array::from_fn(|_| Vec::new()),
            };
            for x0 in (0..w).step_by(TILE_W) {
                let tw = TILE_W.min(w - x0);
                denoise_tile(&padded, &table, y0, th, x0, tw, &mut s);
                // divide by the weight sum, rounding like OpenCV ((sum + wsum/2) / wsum)
                for r in 0..th {
                    for j in 0..tw {
                        let ws = s.wsum[r * tw + j];
                        for c in 0..C {
                            let v = (s.est[c][r * tw + j] + ws / 2) / ws;
                            out[c][r * w + x0 + j] = v.min(255) as u8;
                        }
                    }
                }
            }
            out
        })
        .collect();

    // stitch the bands back into whole planes
    std::array::from_fn(|c| {
        let mut data = Vec::with_capacity(w * ht);
        for band in &bands {
            data.extend_from_slice(&band[c]);
        }
        Gray8::from_vec(w, ht, data)
    })
}

/// Accumulate weights and weighted sums for one tile (output rows y0..y0+th, columns x0..x0+tw).
#[allow(clippy::too_many_arguments)]
fn denoise_tile<const C: usize>(
    pad: &[Padded; C],
    table: &[u32],
    y0: usize,
    th: usize,
    x0: usize,
    tw: usize,
    s: &mut Scratch<C>,
) {
    let pw = pad[0].width;
    let dw = tw + 2 * T_HALF; // width of the squared-difference area
    let dh = th + 2 * T_HALF;
    let cap = table.len() - 1;
    s.dist.resize(dw * dh, 0);
    s.col.resize(th * dw, 0);
    s.wrow.resize(tw, 0);
    s.wsum.clear();
    s.wsum.resize(th * tw, 0);
    for e in s.est.iter_mut() {
        e.clear();
        e.resize(th * tw, 0);
    }

    for oy in -(S_HALF as isize)..=S_HALF as isize {
        for ox in -(S_HALF as isize)..=S_HALF as isize {
            // 1. squared differences between the image and the image shifted by (oy, ox).
            //    Row r of `dist` is image row y0 + r - 3 (in padded coordinates: + PAD).
            for r in 0..dh {
                let py = y0 + r + PAD - T_HALF;
                let px = x0 + PAD - T_HALF;
                let qy = (py as isize + oy) as usize;
                let qx = (px as isize + ox) as usize;
                let drow = &mut s.dist[r * dw..][..dw];
                drow.fill(0);
                for p in pad.iter() {
                    let a = &p.data[py * pw + px..][..dw];
                    let b = &p.data[qy * pw + qx..][..dw];
                    for i in 0..dw {
                        let d = a[i] as i32 - b[i] as i32;
                        drow[i] += d * d;
                    }
                }
            }
            // 2. vertical 7-sums: col[r] = dist[r] + ... + dist[r + 6]
            {
                let (first, _) = s.col.split_at_mut(dw);
                first.fill(0);
                for k in 0..2 * T_HALF + 1 {
                    for (c, d) in first.iter_mut().zip(&s.dist[k * dw..][..dw]) {
                        *c += d;
                    }
                }
            }
            for r in 1..th {
                let (prev, rest) = s.col.split_at_mut(r * dw);
                let prev = &prev[(r - 1) * dw..];
                let cur = &mut rest[..dw];
                let add = &s.dist[(r + 2 * T_HALF) * dw..][..dw];
                let sub = &s.dist[(r - 1) * dw..][..dw];
                for i in 0..dw {
                    cur[i] = prev[i] + add[i] - sub[i];
                }
            }
            // 3. horizontal 7-sum = the full patch distance; look up its weight; accumulate.
            //    Written as separate simple loops (no running total carried from one pixel to the
            //    next) so the compiler can turn each one into SIMD instructions.
            for r in 0..th {
                let col = &s.col[r * dw..][..dw];
                let wrow = &mut s.wrow[..tw];
                for j in 0..tw {
                    let d: i32 = col[j..j + 2 * T_HALF + 1].iter().sum();
                    wrow[j] = table[((d >> 6) as usize).min(cap)];
                }
                let wsum = &mut s.wsum[r * tw..][..tw];
                for j in 0..tw {
                    wsum[j] += wrow[j];
                }
                let qy = (y0 as isize + r as isize + PAD as isize + oy) as usize;
                let qx = (x0 as isize + PAD as isize + ox) as usize;
                for c in 0..C {
                    let cand = &pad[c].data[qy * pw + qx..][..tw];
                    let est = &mut s.est[c][r * tw..][..tw];
                    for j in 0..tw {
                        est[j] += wrow[j] * cand[j] as u32;
                    }
                }
            }
        }
    }
}

/// `cv2.fastNlMeansDenoisingColored(img, None, h_luma, h_color, 7, 21)`: convert to Lab (OpenCV treats
/// the 8-bit values as linear here), denoise L with `h_luma` and a/b together with `h_color`, convert back.
pub fn denoise_colored(img: &Rgb8, h_luma: f32, h_color: f32) -> Rgb8 {
    let lab = opencv::rgb_to_lab(img, Gamma::Linear);
    let (l, a, b) = (lab.channel(0), lab.channel(1), lab.channel(2));
    let ([l], [a, b]) = match crate::gpu::active() {
        // `--gpu`: same maths on the graphics card (gpu.rs). If the GPU call fails, say so and redo
        // the frame's NLM on the CPU rather than failing the frame.
        Some(g) => match (g.denoise([&l], h_luma), g.denoise([&a, &b], h_color)) {
            (Ok(l), Ok(ab)) => (l, ab),
            (Err(e), _) | (_, Err(e)) => {
                eprintln!("rawdev: GPU NLM failed ({e}); using the CPU for this frame");
                (denoise([&l], h_luma), denoise([&a, &b], h_color))
            }
        },
        None => (denoise([&l], h_luma), denoise([&a, &b], h_color)),
    };
    opencv::lab_to_rgb(&Rgb8::from_planes(&l, &a, &b), Gamma::Linear)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencv::tests::{crop, prim};

    #[test]
    fn weight_table_shape() {
        let t = weight_table(6.0, 1);
        assert_eq!(t[0], 19096);
        assert_eq!(*t.last().unwrap(), 0);
    }

    #[test]
    #[ignore]
    fn nlm_matches_cv2() {
        let c = crop();
        let g = c.channel(1);
        let [d] = denoise([&g], 6.0);
        let bad = d
            .data
            .iter()
            .zip(prim("nlm_gray"))
            .filter(|(a, b)| *a != b)
            .count();
        assert_eq!(bad, 0, "gray NLM: {bad} pixels differ");

        let lab = opencv::rgb_to_lab(&c, Gamma::Linear);
        let [a, b] = denoise([&lab.channel(1), &lab.channel(2)], 9.0);
        let want = prim("nlm_ab");
        let bad = (0..a.data.len())
            .filter(|&i| a.data[i] != want[2 * i] || b.data[i] != want[2 * i + 1])
            .count();
        assert_eq!(bad, 0, "ab NLM: {bad} pixels differ");

        let col = denoise_colored(&c, 6.0, 9.0);
        let bad = col
            .data
            .iter()
            .zip(prim("nlm_color"))
            .filter(|(a, b)| *a != b)
            .count();
        assert_eq!(bad, 0, "coloured NLM: {bad} samples differ");
    }
}
