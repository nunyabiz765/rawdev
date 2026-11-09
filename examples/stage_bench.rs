//! Time the CPU pixel stages on one full-size frame and print a hash of each result, so a change to a
//! stage can be checked for speed *and* for identical output in one run.
//!
//!     cargo run --release --example stage_bench -- OUT/stages/IMG_0001/s1_tone.bin WIDTH HEIGHT [runs]
//!
//! The input is a raw 8-bit RGB stage dump (`rawdev develop --dump-stages` writes these; WIDTH and
//! HEIGHT are the frame's `size` from the JSON output). Each stage runs `runs` times (default 3); the
//! best time is printed with the SHA-256 of its output.

use rawdev::image::Rgb8;
use rawdev::look::{self, NrParams};
use rawdev::nlm;
use rawdev::opencv::{self, Gamma};
use rawdev::pillow;
use sha2::{Digest, Sha256};
use std::time::Instant;

fn best<T>(runs: usize, mut f: impl FnMut() -> T) -> (f64, T) {
    let mut best = f64::MAX;
    let mut out = None;
    for _ in 0..runs {
        let t = Instant::now();
        let r = f();
        best = best.min(t.elapsed().as_secs_f64());
        out = Some(r);
    }
    (best, out.unwrap())
}

fn hash(bytes: &[u8]) -> String {
    let d = Sha256::digest(bytes);
    d.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: stage_bench <s1_tone.bin> WIDTH HEIGHT [runs]";
    let path = args.first().expect(usage);
    let dim = |i: usize| -> usize { args.get(i).and_then(|s| s.parse().ok()).expect(usage) };
    let (w, h) = (dim(1), dim(2));
    let runs: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(3);
    let img = Rgb8::from_vec(w, h, std::fs::read(path).expect("read input"));
    println!(
        "{w}x{h}, {} threads, best of {runs}",
        rayon::current_num_threads()
    );
    let row = |name: &str, s: f64, bytes: &[u8]| println!("{name:<22} {s:8.3} s  {}", hash(bytes));

    let lab = opencv::rgb_to_lab(&img, Gamma::Linear);
    let (l, a, b) = (lab.channel(0), lab.channel(1), lab.channel(2));
    let (s, [dl]) = best(runs, || nlm::denoise([&l], 9.0));
    row("nlm L (C=1)", s, &dl.data);
    let (s, [da, db]) = best(runs, || nlm::denoise([&a, &b], 14.0));
    row("nlm ab (C=2)", s, &[da.data, db.data].concat());

    let cr = opencv::rgb_to_ycrcb(&img).channel(1);
    for k in [3, 5] {
        let (s, m) = best(runs, || opencv::median_blur(&cr, k));
        row(&format!("median {k}x{k}"), s, &m.data);
    }
    let (s, m) = best(runs, || opencv::median_blur_rgb(&img, 3));
    row("median rgb 3x3", s, &m.data);

    for (name, p) in [
        (
            "noise reduction",
            NrParams {
                h_luma: 9.0,
                h_color: 14.0,
                pre_median: false,
                chroma_median: 3,
                luma_blend: 0.25,
                blend_from_input: false,
            },
        ),
        (
            "noise reduction strong",
            NrParams {
                h_luma: 9.0,
                h_color: 14.0,
                pre_median: true,
                chroma_median: 5,
                luma_blend: 0.1,
                blend_from_input: true,
            },
        ),
    ] {
        let (s, o) = best(runs, || look::noise_reduction(&img, &p));
        row(name, s, &o.data);
    }
    let (s, o) = best(runs, || pillow::unsharp_mask(&img, 2.0, 60, 2));
    row("unsharp", s, &o.data);
}
