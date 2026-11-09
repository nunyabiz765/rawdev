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
//! * the image is cut into tiles (64 rows x 256 columns); rayon spreads the tiles over all threads;
//! * inside a tile we loop over the 441 search offsets on the outside. For one offset (dy, dx), every
//!   pixel's patch distance is a 7x7 box-sum of the *same* squared-difference image, which we get with
//!   running sums: a vertical 7-sum per column, then a sliding horizontal 7-sum. That's a handful of
//!   additions per pixel per offset instead of 49 multiplies;
//! * offsets o and -o share their patch distances (see `denoise_tile`), so only half of the 441 offsets
//!   need them worked out.

use crate::image::{Gray8, Rgb8};
use crate::opencv::{self, Gamma, reflect101};
use rayon::prelude::*;

const T_HALF: usize = 3; // template (patch) is 7x7
const S_HALF: usize = 10; // search window is 21x21
const SEARCH: usize = 2 * S_HALF + 1;
const PAD: usize = S_HALF + T_HALF; // border needed so every patch we read exists
const TILE_H: usize = 64;
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
    idx: Vec<u32>,      // weight-table index for one row of the tile, for the current offset
    wrow: Vec<u32>, // weights over the region (tile + tile shifted back), for the current offset
    wsum: Vec<u32>, // sum of weights per pixel
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
                idx: Vec::new(),
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
///
/// The search offsets come in pairs: the patch distance between pixel p and p + o is the same as between
/// pixel p + o and (p + o) - o. So for each offset o in one half of the window we work out the weights
/// over the tile *and* the tile shifted by -o, and use every weight twice: at p for offset o, and at
/// p + o for offset -o. That nearly halves steps 1-3 below. (The sums are whole numbers, so adding the
/// candidates up in this order instead of row by row gives exactly the same totals.)
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
    let cap = (table.len() - 1) as u32;
    s.wsum.clear();
    s.wsum.resize(th * tw, 0);
    for e in s.est.iter_mut() {
        e.clear();
        e.resize(th * tw, 0);
    }

    // offset (0, 0): every patch matches itself exactly (distance 0)
    let w0 = table[0];
    for r in 0..th {
        let py = y0 + r + PAD;
        for v in &mut s.wsum[r * tw..][..tw] {
            *v += w0;
        }
        for c in 0..C {
            let cand = &pad[c].data[py * pw + x0 + PAD..][..tw];
            let est = &mut s.est[c][r * tw..][..tw];
            for j in 0..tw {
                est[j] += w0 * cand[j] as u32;
            }
        }
    }

    // the other half: oy > 0, or oy == 0 and ox > 0
    for oy in 0..=S_HALF as isize {
        for ox in -(S_HALF as isize)..=S_HALF as isize {
            if oy == 0 && ox <= 0 {
                continue;
            }
            let (uy, ux) = (oy as usize, ox.unsigned_abs());
            let lx = ox.max(0) as usize; // how far the region reaches left of the tile
            // The region: the tile together with the tile shifted by -o, i.e. rows y0-oy..y0+th and
            // columns x0-lx..x0-lx+rw (in image coordinates; padded coordinates add PAD).
            let (rh, rw) = (th + uy, tw + ux);
            let dw = rw + 2 * T_HALF; // width of the squared-difference area
            let dh = rh + 2 * T_HALF;
            s.dist.resize(dw * dh, 0);
            s.col.resize(rh * dw, 0);
            s.idx.resize(rw, 0);
            s.wrow.resize(rh * rw, 0);

            // 1. squared differences between the image and the image shifted by (oy, ox).
            //    Row r of `dist` is region row r - 3; in padded coordinates the region starts at
            //    row y0 - oy + PAD, column x0 - lx + PAD.
            for r in 0..dh {
                let py = y0 + r + PAD - uy - T_HALF;
                let px = x0 + PAD - lx - T_HALF;
                let qy = py + uy;
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
            for r in 1..rh {
                let (prev, rest) = s.col.split_at_mut(r * dw);
                let prev = &prev[(r - 1) * dw..];
                let cur = &mut rest[..dw];
                let add = &s.dist[(r + 2 * T_HALF) * dw..][..dw];
                let sub = &s.dist[(r - 1) * dw..][..dw];
                for i in 0..dw {
                    cur[i] = prev[i] + add[i] - sub[i];
                }
            }
            // 3. horizontal 7-sum = the full patch distance; look up its weight (for the whole region).
            //    Written as separate simple loops (no running total carried from one pixel to the
            //    next) so the compiler can turn each one into SIMD instructions. The table lookup gets
            //    a loop of its own: in the same loop as the sum it kept the sum from vectorising.
            for r in 0..rh {
                let col = &s.col[r * dw..][..dw];
                let idx = &mut s.idx[..rw];
                for j in 0..rw {
                    let d = col[j]
                        + col[j + 1]
                        + col[j + 2]
                        + col[j + 3]
                        + col[j + 4]
                        + col[j + 5]
                        + col[j + 6];
                    idx[j] = ((d >> 6) as u32).min(cap);
                }
                lookup(table, idx, &mut s.wrow[r * rw..][..rw]);
            }
            // 4. accumulate. Tile pixel p (row r, column j) is region (r + oy, j + lx): its weight for
            //    offset o, candidate p + o. Region (r, j + lx - ox) is p - o: the same weight serves p
            //    for offset -o, candidate p - o.
            for r in 0..th {
                let wa = &s.wrow[(r + uy) * rw + lx..][..tw];
                let wb = &s.wrow[r * rw + (lx as isize - ox) as usize..][..tw];
                let wsum = &mut s.wsum[r * tw..][..tw];
                for j in 0..tw {
                    wsum[j] += wa[j] + wb[j];
                }
                let py = y0 + r + PAD;
                let ay = py + uy; // p + o
                let by = py - uy; // p - o
                let ax = (x0 as isize + PAD as isize + ox) as usize;
                let bx = (x0 as isize + PAD as isize - ox) as usize;
                for c in 0..C {
                    let ca = &pad[c].data[ay * pw + ax..][..tw];
                    let cb = &pad[c].data[by * pw + bx..][..tw];
                    let est = &mut s.est[c][r * tw..][..tw];
                    for j in 0..tw {
                        est[j] += wa[j] * ca[j] as u32 + wb[j] * cb[j] as u32;
                    }
                }
            }
        }
    }
}

/// `out[j] = table[idx[j]]`. A function of its own (not inlined) so the loop gets registers to itself;
/// inlined into `denoise_tile` it reloaded its pointers from the stack on every pixel.
#[inline(never)]
fn lookup(table: &[u32], idx: &[u32], out: &mut [u32]) {
    for (o, &i) in out.iter_mut().zip(idx) {
        *o = table[i as usize];
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

    /// NLM straight from its definition (every pixel, every offset, every 7x7 patch sample) on random
    /// planes, against the tiled, offset-pairing version. Sizes cover partial tiles and several bands
    /// and tiles, plus planes smaller than the search window.
    #[test]
    fn denoise_matches_definition() {
        fn direct<const C: usize>(planes: [&Gray8; C], h: f32) -> [Vec<u8>; C] {
            let (w, ht) = (planes[0].width, planes[0].height);
            let p: [Padded; C] = planes.map(pad);
            let table = weight_table(h, C);
            let cap = table.len() - 1;
            let pw = p[0].width;
            let mut out: [Vec<u8>; C] = std::array::from_fn(|_| vec![0u8; w * ht]);
            for y in 0..ht {
                for x in 0..w {
                    let (cy, cx) = (y + PAD, x + PAD);
                    let mut wsum = 0u32;
                    let mut est = [0u32; C];
                    for oy in -(S_HALF as isize)..=S_HALF as isize {
                        for ox in -(S_HALF as isize)..=S_HALF as isize {
                            let (qy, qx) =
                                ((cy as isize + oy) as usize, (cx as isize + ox) as usize);
                            let mut d = 0i32;
                            for ty in 0..7 {
                                for tx in 0..7 {
                                    for pl in &p {
                                        let a = pl.data[(cy + ty - 3) * pw + cx + tx - 3] as i32;
                                        let b = pl.data[(qy + ty - 3) * pw + qx + tx - 3] as i32;
                                        d += (a - b) * (a - b);
                                    }
                                }
                            }
                            let wt = table[((d >> 6) as usize).min(cap)];
                            wsum += wt;
                            for c in 0..C {
                                est[c] += wt * p[c].data[qy * pw + qx] as u32;
                            }
                        }
                    }
                    for c in 0..C {
                        out[c][y * w + x] = ((est[c] + wsum / 2) / wsum).min(255) as u8;
                    }
                }
            }
            out
        }
        let mut seed = 0xD1B54A32D192ED03u64;
        let mut plane = |w: usize, h: usize, spread: u64| {
            let data = (0..w * h)
                .map(|i| {
                    seed = seed
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    // a gradient plus noise, so weights are neither all zero nor all equal
                    ((i % w) as u64 * 2 + (seed >> 58) % spread).min(255) as u8
                })
                .collect();
            Gray8::from_vec(w, h, data)
        };
        // (not 1 pixel wide or tall: reflect101 has no mirror image of a single sample)
        for (w, h) in [(2, 2), (5, 3), (23, 40), (262, 37)] {
            for (hl, spread) in [(6.0, 16), (14.0, 64)] {
                let a = plane(w, h, spread);
                let b = plane(w, h, spread);
                let [d1] = denoise([&a], hl);
                assert_eq!(d1.data, direct([&a], hl)[0], "C=1 {w}x{h} h={hl}");
                let [da, db] = denoise([&a, &b], hl);
                let [ea, eb] = direct([&a, &b], hl);
                assert_eq!((da.data, db.data), (ea, eb), "C=2 {w}x{h} h={hl}");
            }
        }
    }

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
