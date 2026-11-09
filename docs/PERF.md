# Performance: 0.1.3

0.1.3 is a speed release. No look, option, JSON field or exit code changed. Every JPEG it writes is
byte-identical to 0.1.2 (checked with `cmp`, see "Same output" below). This page covers how it was
measured, what changed, what was tried and dropped, and the limits of the numbers.

## Hardware and method

- One desktop: AMD Ryzen 5 8600G (Zen 4, 6 cores / 12 threads), 32 GB RAM, NVIDIA RTX 2060 (driver
  615.78), Radeon 760M iGPU (Mesa RADV 26.2.4), Linux 7.2, LibRaw 0.22.2, exiftool 13.55.
- The desktop was in normal use while measuring (browser, chat apps, a Node process). Load average was
  between 3 and 14 (a browser, chat apps, a video player, an editor and a Node process). Treat absolute times as "a busy workstation". 0.1.2 and 0.1.3 were measured back to back on
  the same inputs, so the comparison is fair, but expect a few percent of noise.
- Binaries: the release tarballs. 0.1.2 is the published x86-64-v3 build; 0.1.3 is this release's
  build from the same Debian 12 container script (`packaging/release.sh`).
- Batches are whole `rawdev develop` runs timed by [hyperfine](https://github.com/sharkdp/hyperfine)
  (wall clock, mean of the runs shown, inputs already in the page cache). They include decode, exiftool
  and JPEG encode. Single steps were timed with `examples/stage_bench.rs` on one 26 MP frame (best of
  3) and profiled with `perf record`.
- Inputs: 24–26 MP Canon CR3 frames (ISO 125 to 25600) and the nine public sample RAWs from
  `tools/fetch-samples.sh` (Nikon, Sony, Fujifilm X-Trans, Olympus, Panasonic, Pentax, Ricoh and phone
  DNG, Canon CR2).

## Where the time went in 0.1.2

`perf record` on two ISO 12800 CR3 frames with `neutral-night` on the CPU:

| Share of CPU samples | What |
|---|---|
| 73% | non-local means (NLM) noise reduction: luma 36%, colour 37% |
| 7.6% | the median filter on Cr/Cb after NLM |
| 2.6% | the box blur inside the unsharp mask |
| ~3% | LibRaw decode (`dcraw_emu`, a separate process) |
| the rest | colour conversions, defringe, resize, JPEG, small steps |

Per frame, NLM was 7.3–7.6 s of a 8.8–9.5 s frame. Inside NLM, the hot loop that adds up the 7-pixel
patch distance and looks up its weight had compiled to scalar code that reloaded its pointers from
the stack on every pixel.

## What changed

All of these are exact rewrites: integer maths in the same order of operations per sample (or, for
sums, a different order of whole-number additions, which can't change the total), or a different way
of finding the same value (a median is a median). Each has a unit test against the plain version.

1. **NLM: patch-distance sum and weight lookup in separate loops** (`src/nlm.rs`). The 7-sum now
   compiles to SIMD instructions; the table lookup runs in a small function of its own so it keeps its
   registers.
2. **NLM: offsets in pairs.** The patch distance between pixel p and p + o is the same as between p + o
   and p. For each of the 220 offsets in one half of the 21 x 21 search window, rawdev now works out the
   weights once over the tile plus the tile shifted back, and uses each weight twice. Offset (0, 0) is a
   constant. That nearly halves the distance work. Tiles grew from 32 to 64 rows (a few percent
   faster with the pairing).
3. **Median filter: a sorting network on 64 pixels at once** (`src/opencv.rs`). The 3 x 3 and 5 x 5
   median used to copy each window and partially sort it. It now runs a fixed list of compare-and-swap
   steps (Batcher's merge network, pruned to the steps the middle value depends on) as `min`/`max` over
   64 neighbouring pixels. The network is checked for every 0/1 input (all 2^25 for 5 x 5; by the 0-1
   principle that covers every input) and against the old filter on random images.
4. **Unsharp mask: box blur as sums of shifted lines** (`src/pillow.rs`). Pillow's three horizontal
   and three vertical box passes now add whole shifted rows (vertical) or shifted row slices
   (horizontal), which vectorises and no longer needs two transposes of the image.
5. **EXIF copy overlaps the next frame.** In a batch, frame i's exiftool run, output hashes, log line
   and JSON event happen on a second thread while frame i + 1 develops. Events and log lines keep their
   order. A frame's `seconds` is now its develop time plus its EXIF copy.

`examples/stage_bench.rs` (new) times the pixel stages and prints a hash of each output, so a future
change can be checked for speed and identical output in one run.

## Results

### Single steps (one 6264 x 4180 frame, CPU, 12 threads, best of 3)

Measured on a `-C target-cpu=native` build while developing (the release build is x86-64-v3):

| Step | 0.1.2 | 0.1.3 | |
|---|---|---|---|
| NLM luma (h 9) | 3.39 s | 1.05 s | 3.2x |
| NLM colour (h 14) | 3.28 s | 1.29 s | 2.5x |
| Whole noise-reduction step | 6.68 s | 2.55 s | 2.6x |
| Median 3 x 3 (one plane) | 0.153 s | 0.006 s | 25x |
| Median 5 x 5 (one plane) | 0.391 s | 0.017 s | 23x |
| Unsharp mask | 0.432 s | 0.131 s | 3.3x |

Output hashes of every step were identical before and after.

### Whole batches (release binaries)

Wall clock, mean ± standard deviation of the runs (n). "CPU s" is user + system time summed over all
threads (less sensitive to the other load on the machine than wall clock).

| Batch | Device | n | 0.1.2 | 0.1.3 | Speedup | CPU s 0.1.2 → 0.1.3 |
|---|---|---|---|---|---|---|
| 1 × 26 MP Canon RP, ISO 12800, `neutral-night` | CPU | 5 | 9.20 ± 0.16 s | 4.25 ± 0.10 s | 2.17x | 80.7 → 31.5 |
| same | RTX 2060 | 5 | 4.03 ± 0.20 s | 3.48 ± 0.04 s | 1.16x | 14.2 → 10.1 |
| 3 × 24 MP Canon R100 night, ISO 1600, `rich-night` (`--strong`, `--nofringe`) | CPU | 3 | 25.71 ± 0.54 s | 10.95 ± 0.37 s | 2.35x | 232.9 → 83.1 |
| same | RTX 2060 | 3 | 10.79 ± 0.25 s | 7.62 ± 0.37 s | 1.41x | 44.5 → 24.7 |
| same | Radeon 760M | 3 | 11.69 ± 0.03 s | 8.97 ± 0.09 s | 1.30x | 44.8 → 25.0 |
| same, `--threads 3` | CPU | 2 | 49.04 ± 0.57 s | 17.07 ± 0.17 s | 2.87x | 141.1 → 50.3 |
| same, baseline x86-64 build | CPU | 3 | 31.21 ± 0.08 s | 15.83 ± 0.45 s | 1.97x | 279.9 → 138.4 |
| 4 × 24 MP Canon R100 day, ISO 125–6400, `rich-day` (`--match`, `--shadow-lift`, `--recover-hl`) | CPU | 3 | 32.32 ± 0.21 s | 14.08 ± 0.66 s | 2.30x | 248.0 → 105.3 |
| same | RTX 2060 | 3 | 14.56 ± 0.66 s | 12.83 ± 1.57 s | 1.13x | 64.8 → 46.8 |
| 9 × 26 MP Canon RP, ISO 12800–25600, `neutral-night` | CPU | 2 | 91.81 ± 0.89 s | 35.25 ± 0.05 s | 2.60x | 755.6 → 278.9 |
| same, `--threads 6` | RTX 2060 | 3+3 | best 34.6 s | best 26.4 s | ~1.3x | 101–106 → 74–77 |
| 9 public samples, 10–24 MP, `rich-night` | CPU | 2 | 58.04 ± 0.10 s | 30.88 ± 0.27 s | 1.88x | 415.0 → 173.1 |
| same | RTX 2060 | 2 | 27.85 ± 0.20 s | 23.59 ± 0.31 s | 1.18x | 83.3 → 62.6 |
| 4 × 24 MP Canon R100 day, a 14-step layered look | CPU | 3 | 24.51 ± 0.10 s | 23.95 ± 0.53 s | 1.02x (no change) | 134.2 → 127.8 |

- The RP `--threads 6` GPU batch was the noisiest case: across two sessions its runs ranged from 26 to
  94 s for both versions whenever another program was busy on the GPU or CPU, so the table gives the best
  runs (0.1.2: 34.6, 35.1 s; 0.1.3: 26.4, 26.5 s). The sample batch on the RTX 2060 was also measured
  at 46 s for both versions during a busy spell; the row above is the quieter rerun.
- The sample batches wait ~10–13 s on the single-threaded Fujifilm X-Trans decode (kept single-threaded
  on purpose: LibRaw's multithreaded X-Trans demosaic isn't deterministic).

### Memory

Peak resident memory (hyperfine / `time -v`) is unchanged within a few percent: 0.74 GB for one 26 MP
frame on the CPU (0.88 GB with the GPU), 1.0–1.3 GB for the batches above, 1.9 GB with the layered look.
The finisher thread holds no image data, only paths.

### Ingest and cull

Unchanged code, measured for the record. `ingest` of a 488 MB "card" (26 RAW + JPEG files, from RAM)
to the archive disk took 9.1 s with either version (53 MB/s): the time is the `fsync` and the disk, not
the hashing. To a RAM disk the same ingest takes 0.78 s (~625 MB/s for copy + two SHA-256 passes; the
`sha2` crate uses the CPU's SHA extensions), so a real card reader will be the limit. `cull` of 3 of 9
frames and its `--undo` take under 0.1 s.

## Same output

- **CPU:** 89 frames x 5 looks/variants (`neutral-day`, `neutral-night`, `rich-day` with `--match`,
  `--shadow-lift` and `--recover-hl`, `rich-night` with `--strong` and `--nofringe`, and three layered
  looks), 178 JPEGs (full size + share copy): every one `cmp`-identical to 0.1.2, for both the
  x86-64-v3 and the baseline build.
- **GPU:** the same 178 JPEGs with `--gpu` on the RTX 2060 and on the Radeon 760M, and 12 (two
  night batches) on llvmpipe, are `cmp`-identical to 0.1.2's CPU output: 724 comparisons, 0 differences.
- `rich-day` / `rich-night` output is unchanged, so it still matches the private engine they came from.
- All unit and CLI tests pass (`cargo test --release`), plus the ignored GPU-vs-CPU NLM test.

## Tried and dropped

- **AVX2 gather for the NLM weight lookup.** Slower than the plain loop on Zen 4 (2.05–2.10 s vs
  1.89–1.95 s on luma, at that point in the work) and it needed `unsafe`; dropped. The crate still has no `unsafe` code.
- **Smaller NLM tiles to fit L1 cache** (16 x 64, 32 x 64): no gain or slower. Larger tiles help only
  a little; 64 x 256 was kept, 64 x 512 measured about the same.
- **A persistent exiftool process (`-stay_open`).** Would save exiftool's ~0.2 s start per frame on
  single-frame runs, but the batch overlap already hides it; not worth the extra protocol code yet.
- **Faster colour conversions (Lab, YCrCb).** They are table lookups per pixel (about 5% each of a GPU
  frame). Larger tables or SIMD gathers didn't look worth the risk in this release.
- **Changing the GPU shader.** The CPU pairing trick would also apply on the GPU (about 0.6 s per NLM
  call on the RTX 2060), but the shader is the part whose bit-exactness was hardest won across three
  Vulkan drivers. Left for a later release, with the parity matrix rerun on all three.

## Caveats

- One machine, one CPU family. The release build targets x86-64-v3 (AVX2); the single-step numbers above
  came from a native Zen 4 build (AVX-512), so a release build can differ a little from them.
- With `--gpu`, NLM runs on the graphics card and the CPU-side speedups matter less: the GPU NLM itself
  is unchanged, so GPU batches gain from the median, unsharp and EXIF changes only.
- The layered recipe uses its own float steps (ImageMagick-style); 0.1.3 doesn't change them.
- NLM still loops forever on a plane 1 pixel wide or tall (its mirrored border has nothing to mirror).
  Found while writing the new test; not reachable with real photos, and unchanged from 0.1.2.
