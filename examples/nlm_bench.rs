//! Time the noise-reduction core on one full-size frame, on the CPU and (with --gpu) on the GPU.
//!
//!     cargo run --release --example nlm_bench -- OUT/stages/IMG_0001/s1_tone.bin WIDTH HEIGHT [runs]
//!         [--gpu] [--check] [--h LUMA,COLOR]
//!
//! The input is a raw 8-bit RGB stage dump (`rawdev develop --dump-stages` writes these; WIDTH and
//! HEIGHT are the frame's `size` from the JSON output).
//! `--check` runs both paths once and compares them sample by sample (L, a/b and the final RGB).

use rawdev::gpu;
use rawdev::image::Rgb8;
use rawdev::nlm;
use rawdev::opencv::{self, Gamma};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args
        .first()
        .expect("usage: nlm_bench <s1_tone.bin> WIDTH HEIGHT [runs] [--gpu] [--check] [--h L,C]");
    let dim = |i: usize| -> usize {
        args.get(i)
            .and_then(|s| s.parse().ok())
            .expect("usage: nlm_bench <s1_tone.bin> WIDTH HEIGHT ...")
    };
    let (w, h) = (dim(1), dim(2));
    let runs: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(3);
    let use_gpu = args.iter().any(|a| a == "--gpu");
    let check = args.iter().any(|a| a == "--check");
    let (hl, hc) = args
        .iter()
        .position(|a| a == "--h")
        .map(|i| {
            let v: Vec<f32> = args[i + 1].split(',').map(|x| x.parse().unwrap()).collect();
            (v[0], v[1])
        })
        .unwrap_or((6.0, 9.0));
    let img = Rgb8::from_vec(w, h, std::fs::read(path).expect("read input"));

    if check {
        let g = gpu::GpuNlm::new().expect("no GPU");
        println!("GPU: {}  h = {hl}/{hc}", g.name);
        let lab = opencv::rgb_to_lab(&img, Gamma::Linear);
        let (l, a, b) = (lab.channel(0), lab.channel(1), lab.channel(2));
        let t = Instant::now();
        let [lc] = nlm::denoise([&l], hl);
        let [ac, bc] = nlm::denoise([&a, &b], hc);
        let cpu_s = t.elapsed().as_secs_f64();
        let t = Instant::now();
        let [lg] = g.denoise([&l], hl).unwrap();
        let [ag, bg] = g.denoise([&a, &b], hc).unwrap();
        let gpu_s = t.elapsed().as_secs_f64();
        let diff = |x: &[u8], y: &[u8]| x.iter().zip(y).filter(|(p, q)| p != q).count();
        let maxd = |x: &[u8], y: &[u8]| {
            x.iter()
                .zip(y)
                .map(|(p, q)| p.abs_diff(*q))
                .max()
                .unwrap_or(0)
        };
        println!(
            "L  differing samples: {} (max {})   a: {} (max {})   b: {} (max {})   of {} each",
            diff(&lc.data, &lg.data),
            maxd(&lc.data, &lg.data),
            diff(&ac.data, &ag.data),
            maxd(&ac.data, &ag.data),
            diff(&bc.data, &bg.data),
            maxd(&bc.data, &bg.data),
            lc.data.len()
        );
        println!(
            "NLM (L + a/b) CPU {cpu_s:.2}s  GPU {gpu_s:.2}s (first GPU call includes warm-up)"
        );
        return;
    }

    if use_gpu {
        println!("GPU: {}", gpu::enable().expect("no GPU"));
    }
    for _ in 0..runs {
        let t0 = Instant::now();
        let out = nlm::denoise_colored(&img, hl, hc);
        println!(
            "denoise_colored ({}): {:.2}s (checksum {})",
            if use_gpu { "GPU" } else { "CPU" },
            t0.elapsed().as_secs_f64(),
            out.data.iter().map(|&v| v as u64).sum::<u64>()
        );
    }
}
