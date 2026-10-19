//! Image containers.
//!
//! Every image in rawdev is just a `Vec` of samples plus a width and a height. Pixels are stored row by
//! row ("row-major"), and colour channels are interleaved, so an RGB image is `R G B R G B ...`.
//! Channel `c` of pixel `(x, y)` therefore lives at index `(y * width + x) * 3 + c`.
//!
//! There is deliberately no generic "Image<T, N>" type: three small concrete structs are easier to read.

use rayon::prelude::*;

/// An 8-bit image with 3 interleaved channels. Usually RGB, but the same container also holds the
/// three 8-bit channels of Lab or YCrCb (OpenCV encodes those in 0..=255 too).
#[derive(Clone, Debug)]
pub struct Rgb8 {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

impl Rgb8 {
    /// A black image of the given size.
    pub fn new(width: usize, height: usize) -> Rgb8 {
        Rgb8 {
            width,
            height,
            data: vec![0; width * height * 3],
        }
    }

    /// Wrap existing samples. Panics if the length doesn't match `width * height * 3`.
    pub fn from_vec(width: usize, height: usize, data: Vec<u8>) -> Rgb8 {
        assert_eq!(
            data.len(),
            width * height * 3,
            "RGB buffer has the wrong length"
        );
        Rgb8 {
            width,
            height,
            data,
        }
    }

    /// Copy channel `c` (0, 1 or 2) out into its own single-channel plane.
    pub fn channel(&self, c: usize) -> Gray8 {
        let data = self.data.par_chunks_exact(3).map(|px| px[c]).collect();
        Gray8 {
            width: self.width,
            height: self.height,
            data,
        }
    }

    /// Interleave three planes of the same size back into one 3-channel image.
    pub fn from_planes(p0: &Gray8, p1: &Gray8, p2: &Gray8) -> Rgb8 {
        let mut out = Rgb8::new(p0.width, p0.height);
        out.data
            .par_chunks_exact_mut(3)
            .enumerate()
            .for_each(|(i, px)| {
                px[0] = p0.data[i];
                px[1] = p1.data[i];
                px[2] = p2.data[i];
            });
        out
    }
}

/// A single-channel 8-bit image ("plane"): one channel of a colour image, or a 0/1 mask.
#[derive(Clone, Debug)]
pub struct Gray8 {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

impl Gray8 {
    pub fn new(width: usize, height: usize) -> Gray8 {
        Gray8 {
            width,
            height,
            data: vec![0; width * height],
        }
    }

    pub fn from_vec(width: usize, height: usize, data: Vec<u8>) -> Gray8 {
        assert_eq!(
            data.len(),
            width * height,
            "plane buffer has the wrong length"
        );
        Gray8 {
            width,
            height,
            data,
        }
    }
}

/// A single-channel f32 image (used for the smooth defringe mask).
#[derive(Clone, Debug)]
pub struct GrayF32 {
    pub width: usize,
    pub height: usize,
    pub data: Vec<f32>,
}

/// A float image with 3 interleaved channels: linear-light RGB in 0..1 (the day recipe's colour matrix
/// and shadow lift work on this, before the sRGB encode).
#[derive(Clone, Debug)]
pub struct RgbF32 {
    pub width: usize,
    pub height: usize,
    pub data: Vec<f32>,
}
