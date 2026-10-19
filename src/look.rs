//! The "look" stages that run on the 8-bit image: noise reduction, the Lab chroma lift, and the purple
//! fringe cleanup around blown lights. Each function mirrors one block of develop.py, including where
//! numpy does its maths in f32 versus f64, so the rounding lands in the same places.

use crate::image::{Gray8, GrayF32, Rgb8};
use crate::lookfile;
use crate::nlm;
use crate::opencv::{self, Gamma};
use rayon::prelude::*;

/// Noise-reduction settings for one frame (worked out from the look's `[nr]` table by
/// `lookfile::Look::nr_params`).
#[derive(Clone, Copy, Debug)]
pub struct NrParams {
    /// NLM strength for luma and for colour (cv2's `h` and `hColor`).
    pub h_luma: f32,
    pub h_color: f32,
    /// 3x3 RGB median before NLM (the "strong" coloured-LED frames: kills isolated specks NLM keeps).
    pub pre_median: bool,
    /// Median window for Cr/Cb after NLM (3, or 5 for strong frames).
    pub chroma_median: usize,
    /// How much of the un-denoised luma to blend back for texture (0.2, or 0.1 for strong frames).
    pub luma_blend: f64,
    /// Where that luma comes from when `pre_median` is on: the night recipe takes it after the median,
    /// the day recipe from the untouched input.
    pub blend_from_input: bool,
}

/// Noise reduction: NLM, then a small median on the colour channels for isolated magenta/green specks,
/// then some of the original luma blended back so texture doesn't go plastic.
pub fn noise_reduction(img: &Rgb8, p: &NrParams) -> Rgb8 {
    let src = if p.pre_median {
        opencv::median_blur_rgb(img, 3)
    } else {
        img.clone()
    };
    let den = nlm::denoise_colored(&src, p.h_luma, p.h_color);
    let y_orig = opencv::rgb_to_ycrcb(if p.blend_from_input { img } else { &src }).channel(0);
    let ycc = opencv::rgb_to_ycrcb(&den);
    let cr = opencv::median_blur(&ycc.channel(1), p.chroma_median);
    let cb = opencv::median_blur(&ycc.channel(2), p.chroma_median);

    // numpy: (1 - bb) * yd.astype(f32) is f32 maths, bb * yo (uint8 array) is f64, the sum is f64
    let keep = (1.0 - p.luma_blend) as f32;
    let mut y = Gray8::new(img.width, img.height);
    let yd = ycc.channel(0);
    y.data.par_iter_mut().enumerate().for_each(|(i, v)| {
        let mixed = (keep * yd.data[i] as f32) as f64 + p.luma_blend * y_orig.data[i] as f64 + 0.5;
        *v = mixed.clamp(0.0, 255.0) as u8;
    });
    opencv::ycrcb_to_rgb(&Rgb8::from_planes(&y, &cr, &cb))
}

/// numpy's `np.uint8(np.clip(x, 0, 255) + .5)`: round half up, saturating.
#[inline]
pub fn to_u8(x: f32) -> u8 {
    (x.clamp(0.0, 255.0) + 0.5) as u8
}

/// Chroma lift in Lab (house look: plain renders read grey). Stronger on muted colours, tapering off for
/// already-strong ones so lit signs don't go neon, and sparing near-whites.
pub fn chroma_lift(img: &Rgb8, p: &lookfile::Chroma) -> Rgb8 {
    chroma_lift_weighted(img, None, p)
}

/// The chroma lift, optionally scaled down per pixel by `1 - w` (the day recipe passes its blue-sky
/// mask here, so sky that the colour matrix already matched to the camera JPEG isn't lifted a second time).
pub fn chroma_lift_weighted(img: &Rgb8, w: Option<&GrayF32>, p: &lookfile::Chroma) -> Rgb8 {
    let mut lab = opencv::rgb_to_lab(img, Gamma::Srgb);
    lab.data
        .par_chunks_exact_mut(3)
        .enumerate()
        .for_each(|(i, px)| {
            let l = px[0] as f32;
            let a = px[1] as f32 - 128.0;
            let b = px[2] as f32 - 128.0;
            let c = a.hypot(b);
            let strength = match w {
                None => p.amount,
                Some(w) => p.amount * (1.0 - w.data[i]),
            };
            let boost = 1.0
                + strength
                    * (1.0 - c / p.fade_chroma).clamp(0.0, 1.0)
                    * ((p.white_l - l / 2.55) / p.white_width).clamp(0.0, 1.0);
            px[1] = to_u8(128.0 + a * boost);
            px[2] = to_u8(128.0 + b * boost);
        });
    opencv::lab_to_rgb(&lab, Gamma::Srgb)
}

/// Purple/magenta fringe cleanup in a thin ring just outside blown lights: find the bright cores,
/// grow them by an ellipse (`grow_px`, 15 in the rich looks), and pull a/b toward neutral where the ring
/// is purple.
/// Note it round-trips *every* pixel through 8-bit Lab (as develop.py does), not just the ring.
pub fn defringe(img: &Rgb8, p: &lookfile::Defringe) -> Rgb8 {
    let (w, h) = (img.width, img.height);
    // bright cores: luminance (Rec.709 weights on the 8-bit values) above `core_luma` (235)
    let core_data: Vec<u8> = img
        .data
        .par_chunks_exact(3)
        .map(|q| {
            let yy = q[0] as f32 * 0.2126 + q[1] as f32 * 0.7152 + q[2] as f32 * 0.0722;
            (yy > p.core_luma) as u8
        })
        .collect();
    let core = Gray8::from_vec(w, h, core_data);
    let grown = opencv::dilate_ellipse(&core, p.grow_px);

    let mut lab = opencv::rgb_to_lab(img, Gamma::Srgb);
    // ring * purpleness, as an f32 mask
    let mask: Vec<f32> = lab
        .data
        .par_chunks_exact(3)
        .enumerate()
        .map(|(i, px)| {
            let ring = grown.data[i] != 0 && core.data[i] == 0;
            if !ring {
                return 0.0;
            }
            let a = px[1] as f32 - 128.0;
            let b = px[2] as f32 - 128.0;
            ((a - p.a_start) / p.a_width).clamp(0.0, 1.0)
                * ((p.b_end - b) / p.b_width).clamp(0.0, 1.0)
        })
        .collect();
    let k = opencv::gaussian_blur_f32(
        &GrayF32 {
            width: w,
            height: h,
            data: mask,
        },
        p.blur_sigma,
    );

    lab.data
        .par_chunks_exact_mut(3)
        .zip(k.data.par_iter())
        .for_each(|(px, &kv)| {
            let kk = kv * p.strength;
            let a = px[1] as f32 - 128.0;
            let b = px[2] as f32 - 128.0;
            px[1] = to_u8(128.0 + a * (1.0 - kk));
            px[2] = to_u8(128.0 + b * (1.0 - kk));
        });
    opencv::lab_to_rgb(&lab, Gamma::Srgb)
}
