//! Ports of the Pillow (PIL 12) routines the Python recipe calls: `thumbnail` (reduce + bicubic, or Lanczos)
//! and `ImageFilter.UnsharpMask`.
//!
//! Like `opencv.rs`, these copy Pillow's fixed-point arithmetic from `src/libImaging/{Resample,Reduce,
//! BoxBlur,UnsharpMask}.c` so the output matches PIL to the bit (see the tests at the bottom).

use crate::image::Rgb8;
use rayon::prelude::*;

// ---------------------------------------------------------------------------------------------------------
// Resampling (Image.resize / Image.thumbnail)
// ---------------------------------------------------------------------------------------------------------

/// Resampling filters we need.
#[derive(Clone, Copy, Debug)]
pub enum Filter {
    /// Pillow's default for `thumbnail`: cubic convolution with a = -0.5.
    Bicubic,
    /// 3-lobe Lanczos (`Image.LANCZOS`).
    Lanczos,
    /// `Image.BOX`: each output pixel is the plain average of the source pixels under it.
    Box,
}

impl Filter {
    /// How far (in source pixels, at 1:1 scale) the filter reaches.
    fn support(self) -> f64 {
        match self {
            Filter::Bicubic => 2.0,
            Filter::Lanczos => 3.0,
            Filter::Box => 0.5,
        }
    }

    fn weight(self, x: f64) -> f64 {
        match self {
            Filter::Bicubic => {
                let a = -0.5;
                let x = x.abs();
                if x < 1.0 {
                    ((a + 2.0) * x - (a + 3.0)) * x * x + 1.0
                } else if x < 2.0 {
                    (((x - 5.0) * x + 8.0) * x - 4.0) * a
                } else {
                    0.0
                }
            }
            Filter::Lanczos => {
                fn sinc(x: f64) -> f64 {
                    if x == 0.0 {
                        1.0
                    } else {
                        (x * std::f64::consts::PI).sin() / (x * std::f64::consts::PI)
                    }
                }
                if (-3.0..3.0).contains(&x) {
                    sinc(x) * sinc(x / 3.0)
                } else {
                    0.0
                }
            }
            // Pillow's box_filter: 1 on (-0.5, 0.5]
            Filter::Box => {
                if x > -0.5 && x <= 0.5 {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }
}

/// Fixed-point weights for one resampling direction: output sample `i` is the weighted sum of source
/// samples `first[i] .. first[i] + count[i]`, with weights `k[i * ksize ..]` scaled by 2^22.
struct Coeffs {
    ksize: usize,
    first: Vec<usize>,
    count: Vec<usize>,
    k: Vec<i32>,
}

const PRECISION_BITS: u32 = 32 - 8 - 2;

/// Pillow's `precompute_coeffs` + `normalize_coeffs_8bpc`. `in0..in1` is the source window (the "box",
/// as f32 because that's what Pillow's C code receives).
fn precompute_coeffs(
    in_size: usize,
    in0: f32,
    in1: f32,
    out_size: usize,
    filter: Filter,
) -> Coeffs {
    let scale = (in1 - in0) as f64 / out_size as f64;
    let filterscale = scale.max(1.0);
    let support = filter.support() * filterscale;
    let ksize = support.ceil() as usize * 2 + 1;
    let mut first = Vec::with_capacity(out_size);
    let mut count = Vec::with_capacity(out_size);
    let mut k = vec![0i32; out_size * ksize];
    for xx in 0..out_size {
        let center = in0 as f64 + (xx as f64 + 0.5) * scale;
        let ss = 1.0 / filterscale;
        // `as i64` truncates toward zero, exactly like C's `(int)` cast
        let xmin = ((center - support + 0.5) as i64).max(0) as usize;
        let xmax = ((center + support + 0.5) as i64).min(in_size as i64) as usize;
        let n = xmax - xmin;
        let w: Vec<f64> = (0..n)
            .map(|x| filter.weight((x as f64 + xmin as f64 - center + 0.5) * ss))
            .collect();
        let total: f64 = w.iter().sum();
        for (x, wx) in w.iter().enumerate() {
            let v = if total != 0.0 { wx / total } else { *wx };
            let scaled = v * (1u32 << PRECISION_BITS) as f64;
            k[xx * ksize + x] = if v < 0.0 {
                (scaled - 0.5) as i32
            } else {
                (scaled + 0.5) as i32
            };
        }
        first.push(xmin);
        count.push(n);
    }
    Coeffs {
        ksize,
        first,
        count,
        k,
    }
}

/// Fixed-point result back to 8 bits (Pillow's `clip8`).
#[inline]
fn clip8(acc: i32) -> u8 {
    (acc >> PRECISION_BITS).clamp(0, 255) as u8
}

/// `Image.resize((out_w, out_h), filter, box)` for an RGB image. `bx` = (left, top, right, bottom) in
/// source pixels. Pillow resizes horizontally first (only the source rows the vertical pass will need),
/// then vertically, rounding to 8 bits in between.
pub fn resize(img: &Rgb8, out_w: usize, out_h: usize, filter: Filter, bx: [f32; 4]) -> Rgb8 {
    let horiz = precompute_coeffs(img.width, bx[0], bx[2], out_w, filter);
    let vert = precompute_coeffs(img.height, bx[1], bx[3], out_h, filter);
    let y_first = vert.first[0];
    let y_last = vert.first[out_h - 1] + vert.count[out_h - 1];

    // horizontal pass over source rows y_first..y_last
    let mut tmp = Rgb8::new(out_w, y_last - y_first);
    tmp.data
        .par_chunks_mut(out_w * 3)
        .enumerate()
        .for_each(|(row, o)| {
            let line = &img.data[(y_first + row) * img.width * 3..][..img.width * 3];
            for xx in 0..out_w {
                let k = &horiz.k[xx * horiz.ksize..];
                let mut acc = [1i32 << (PRECISION_BITS - 1); 3];
                for x in 0..horiz.count[xx] {
                    let p = &line[(horiz.first[xx] + x) * 3..];
                    for c in 0..3 {
                        acc[c] += p[c] as i32 * k[x];
                    }
                }
                for c in 0..3 {
                    o[xx * 3 + c] = clip8(acc[c]);
                }
            }
        });

    // vertical pass
    let mut out = Rgb8::new(out_w, out_h);
    out.data
        .par_chunks_mut(out_w * 3)
        .enumerate()
        .for_each(|(yy, o)| {
            let k = &vert.k[yy * vert.ksize..];
            let y0 = vert.first[yy] - y_first;
            for i in 0..out_w * 3 {
                let mut acc = 1i32 << (PRECISION_BITS - 1);
                for y in 0..vert.count[yy] {
                    acc += tmp.data[(y0 + y) * out_w * 3 + i] as i32 * k[y];
                }
                o[i] = clip8(acc);
            }
        });
    out
}

/// `Image.reduce((fx, fy))`: average each fx x fy block (partial blocks at the right/bottom edge average
/// what's there). Pillow divides with a float-derived fixed-point multiplier; we copy that exactly.
pub fn reduce(img: &Rgb8, fx: usize, fy: usize) -> Rgb8 {
    let (w, h) = (img.width, img.height);
    let (ow, oh) = (w.div_ceil(fx), h.div_ceil(fy));
    let mut out = Rgb8::new(ow, oh);
    out.data
        .par_chunks_mut(ow * 3)
        .enumerate()
        .for_each(|(oy, o)| {
            let (y0, y1) = (oy * fy, ((oy + 1) * fy).min(h));
            for ox in 0..ow {
                let (x0, x1) = (ox * fx, ((ox + 1) * fx).min(w));
                let n = ((x1 - x0) * (y1 - y0)) as u32;
                // Pillow's division_UINT32(n, 8): (2^32 as f32) / (256 * n), truncated
                let multiplier = (4294967296f32 / (256 * n) as f32) as u32;
                let mut sum = [n / 2; 3];
                for y in y0..y1 {
                    for x in x0..x1 {
                        for c in 0..3 {
                            sum[c] += img.data[(y * w + x) * 3 + c] as u32;
                        }
                    }
                }
                for c in 0..3 {
                    o[ox * 3 + c] = (sum[c].wrapping_mul(multiplier) >> 24) as u8;
                }
            }
        });
    out
}

/// The output size `Image.thumbnail((max_w, max_h))` picks: fit inside the box, keep the aspect ratio,
/// rounding each side to whichever integer gives the closest aspect.
pub fn thumbnail_size(w: usize, h: usize, max_w: usize, max_h: usize) -> (usize, usize) {
    if max_w >= w && max_h >= h {
        return (w, h);
    }
    let aspect = w as f64 / h as f64;
    let (x, y) = (max_w as f64, max_h as f64);
    // round_aspect: pick floor or ceil of `number`, whichever has the smaller `key`
    let round_aspect = |number: f64, key: &dyn Fn(f64) -> f64| -> usize {
        let (lo, hi) = (number.floor(), number.ceil());
        let best = if key(hi) < key(lo) { hi } else { lo };
        best.max(1.0) as usize
    };
    if x / y >= aspect {
        (round_aspect(y * aspect, &|n| (aspect - n / y).abs()), max_h)
    } else {
        (
            max_w,
            round_aspect(x / aspect, &|n| {
                if n == 0.0 {
                    0.0
                } else {
                    (aspect - x / n).abs()
                }
            }),
        )
    }
}

/// `Image.thumbnail((max_w, max_h), filter)` on an in-memory RGB image, including Pillow's default
/// `reducing_gap=2.0` trick: when shrinking a lot it first box-averages by an integer factor
/// (`reduce`), then resamples the rest of the way.
pub fn thumbnail(img: &Rgb8, max_w: usize, max_h: usize, filter: Filter) -> Rgb8 {
    let (tw, th) = thumbnail_size(img.width, img.height, max_w, max_h);
    if (tw, th) == (img.width, img.height) {
        return img.clone();
    }
    let reducing_gap = 2.0;
    let fx = ((img.width as f64 / tw as f64 / reducing_gap) as usize).max(1);
    let fy = ((img.height as f64 / th as f64 / reducing_gap) as usize).max(1);
    if fx > 1 || fy > 1 {
        // Pillow reduces a "safe box" around the requested box; for a whole-image thumbnail that is
        // the whole image, so the remaining box is just the image size divided by the factors.
        let small = reduce(img, fx, fy);
        let bx = [
            0.0,
            0.0,
            (img.width as f64 / fx as f64) as f32,
            (img.height as f64 / fy as f64) as f32,
        ];
        resize(&small, tw, th, filter, bx)
    } else {
        resize(
            img,
            tw,
            th,
            filter,
            [0.0, 0.0, img.width as f32, img.height as f32],
        )
    }
}

// ---------------------------------------------------------------------------------------------------------
// Blur + unsharp mask
// ---------------------------------------------------------------------------------------------------------

/// Pillow approximates a Gaussian with `passes` box blurs; this is the (fractional) box radius it uses.
fn gaussian_blur_radius(radius: f32, passes: u32) -> f32 {
    let sigma2 = radius * radius / passes as f32;
    let l = ((12.0 * sigma2 as f64 + 1.0).sqrt()) as f32;
    let li = ((l as f64 - 1.0) / 2.0).floor() as f32;
    let mut a = (2.0 * li + 1.0) * (li * (li + 1.0) - 3.0 * sigma2);
    a /= 6.0 * (sigma2 - (li + 1.0) * (li + 1.0));
    li + a
}

/// One pass of Pillow's box blur along a line of `n` samples spaced `stride` apart (`ImagingLineBoxBlur`).
/// A box of fractional radius r = whole part + fraction: the inner 2*whole+1 samples get weight `ww`,
/// the two samples just outside get the leftover weight `fw` (both in 1/2^24 units). Edges are clamped.
fn box_blur_line(src: &[u8], dst: &mut [u8], n: usize, stride: usize, radius: f32) {
    let whole = radius as isize;
    let ww = ((1u32 << 24) as f32 / (radius * 2.0 + 1.0)) as u32;
    let fw = ((1u32 << 24) - (whole as u32 * 2 + 1) * ww) / 2;
    let at = |i: isize| src[i.clamp(0, n as isize - 1) as usize * stride] as u32;
    for x in 0..n as isize {
        let mut inner = 0u32;
        for k in -whole..=whole {
            inner += at(x + k);
        }
        let v = inner * ww + (at(x - whole - 1) + at(x + whole + 1)) * fw;
        dst[x as usize * stride] = ((v + (1 << 23)) >> 24) as u8;
    }
}

/// `ImageFilter.GaussianBlur(radius)` as Pillow does it: 3 horizontal box passes, then 3 vertical ones,
/// each rounded to 8 bits.
pub fn gaussian_blur(img: &Rgb8, radius: f32) -> Rgb8 {
    let passes = 3;
    let r = gaussian_blur_radius(radius, passes);
    let (w, h) = (img.width, img.height);
    let mut cur = img.clone();
    for _ in 0..passes {
        let mut next = Rgb8::new(w, h);
        next.data
            .par_chunks_mut(w * 3)
            .zip(cur.data.par_chunks(w * 3))
            .for_each(|(o, s)| {
                for c in 0..3 {
                    box_blur_line(&s[c..], &mut o[c..], w, 3, r);
                }
            });
        cur = next;
    }
    // Vertical passes: work on a transposed copy so each column becomes a contiguous row (this is
    // what Pillow does too, and it's much kinder to the CPU cache than striding down columns).
    let mut t = transpose(&cur);
    for _ in 0..passes {
        let mut next = Rgb8::new(t.width, t.height);
        next.data
            .par_chunks_mut(t.width * 3)
            .zip(t.data.par_chunks(t.width * 3))
            .for_each(|(o, s)| {
                for c in 0..3 {
                    box_blur_line(&s[c..], &mut o[c..], t.width, 3, r);
                }
            });
        t = next;
    }
    transpose(&t)
}

fn transpose(img: &Rgb8) -> Rgb8 {
    let (w, h) = (img.width, img.height);
    let mut out = Rgb8::new(h, w);
    out.data
        .par_chunks_mut(h * 3)
        .enumerate()
        .for_each(|(x, o)| {
            for y in 0..h {
                o[y * 3..y * 3 + 3].copy_from_slice(&img.data[(y * w + x) * 3..][..3]);
            }
        });
    out
}

/// `ImageFilter.UnsharpMask(radius, percent, threshold)`: where a pixel differs from its blurred self by
/// more than `threshold`, push it further away by `percent`% of the difference.
pub fn unsharp_mask(img: &Rgb8, radius: f32, percent: i32, threshold: i32) -> Rgb8 {
    let mut out = gaussian_blur(img, radius);
    out.data
        .par_iter_mut()
        .zip(img.data.par_iter())
        .for_each(|(o, &i)| {
            let diff = i as i32 - *o as i32;
            *o = if diff.abs() > threshold {
                // C integer division truncates toward zero; so does Rust's
                (i as i32 + diff * percent / 100).clamp(0, 255) as u8
            } else {
                i
            };
        });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencv::tests::{crop, prim};

    fn same(name: &str, got: &Rgb8) {
        let want = prim(name);
        assert_eq!(got.data.len(), want.len(), "{name}: size");
        let bad = got.data.iter().zip(&want).filter(|(a, b)| a != b).count();
        assert_eq!(bad, 0, "{name}: {bad} samples differ");
    }

    #[test]
    #[ignore]
    fn pillow_ops_match_pil() {
        let c = crop();
        same("usm", &unsharp_mask(&c, 1.0, 45, 4));
        same("boxblur", &gaussian_blur(&c, 1.0));
        same("lanczos", &thumbnail(&c, 200, 200, Filter::Lanczos));
        same("reduce3", &reduce(&c, 3, 3));
        same("thumb_bicubic", &thumbnail(&c, 80, 80, Filter::Bicubic));
    }

    #[test]
    fn thumbnail_sizes() {
        assert_eq!(thumbnail_size(6024, 4020, 2000, 2000), (2000, 1335));
        assert_eq!(thumbnail_size(4020, 6024, 2000, 2000), (1335, 2000));
        assert_eq!(thumbnail_size(6000, 4000, 800, 800), (800, 533));
    }
}
