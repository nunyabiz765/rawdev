# Changelog

## 0.1.4 (2026-11-13)

Film and black and white in layered looks, and your own looks by name. Built-in looks and every
existing look file develop exactly as in 0.1.3.

- Three new layered steps:
  - `channel-levels`: output levels per channel (ImageMagick `-channel X +level black,white`), for
    faded blacks and split toning.
  - `mono`: black and white from a weighted R/G/B mix (weights are normalised). Red-heavy mixes darken
    blue sky like a red filter; green-heavy mixes lighten foliage.
  - `grain`: monochrome Gaussian film grain (`amount` = standard deviation, `size` = blur in px),
    overlay-blended so it is strongest in the midtones and absent at pure black and white. `long_edge`
    lays the grain out for an image of that size (e.g. the 2000 px share copy) so it looks the same
    there as it was tuned; `seed` makes it repeatable: the same frame always gets the same grain,
    whatever the thread count.
- `--look NAME` also finds `NAME.toml` in `~/.config/rawdev/looks/` (the `looks/` folder next to the
  config file), so your own looks work by name. Built-in names win.
- Checked: film and B&W looks built from these steps match an ImageMagick finish of the same develop
  to within about 1% mean absolute error (the difference is the random grain); two built-in looks and
  a 14-step layered look give `cmp`-identical JPEGs to 0.1.3 (rich-day, neutral-day and the layered look on
  three frames: two Canon CR3s and a 5D Mark II CR2); runs are byte-identical run to run.

## 0.1.3 (2026-11-09)

A speed release. Every output is byte-identical to 0.1.2; nothing about looks, options, JSON or exit
codes changed. Details, method and numbers: `docs/PERF.md`.

- About 2.2–2.6x faster on the CPU for frames with noise reduction: one 26 MP ISO 12800 frame 9.2 s ->
  4.2 s; nine of them 91.8 s -> 35.3 s; three 24 MP night frames 25.7 s -> 11.0 s (12 threads, x86-64-v3
  release build). The baseline x86-64 build gains about the same (31.2 s -> 15.8 s), and `--threads 3`
  more (49.0 s -> 17.1 s).
- With `--gpu`, 1.2–1.4x faster (the GPU noise reduction itself is unchanged; the rest of the frame got
  faster): three night frames 10.8 s -> 7.6 s on an RTX 2060, 11.7 s -> 9.0 s on a Radeon 760M.
- Layered looks, decode, `ingest` and `cull` are unchanged in speed. Peak memory is about the same.
- Noise reduction (NLM) on the CPU: the patch-distance sum now compiles to SIMD instructions, and
  search offsets o and -o share their patch distances, which nearly halves that work.
- The Cr/Cb median filter after NLM is a sorting network over 64 pixels at a time (about 20x faster).
- The unsharp mask's box blur adds whole shifted lines instead of walking each line sample by sample,
  and no longer transposes the image (about 3x faster).
- In a batch, each frame's EXIF copy (exiftool) runs while the next frame develops. JSON events and log
  lines keep their order; a frame's `seconds` is its develop time plus its EXIF copy.
- Checked: 178 JPEGs per build (89 frames, all four built-in looks with their per-frame options, plus
  layered looks) are `cmp`-identical to 0.1.2 for the x86-64-v3 and baseline builds and for `--gpu` on an
  RTX 2060 and a Radeon 760M; 12 more on llvmpipe. New unit tests check each rewrite against the plain
  version (the median network for every 0/1 input).
- New `examples/stage_bench.rs`: times the pixel stages on one frame and prints a hash of each output.

## 0.1.2 (2026-10-21)

- New `layered` recipe (`recipe = "layered"` in a look file): LibRaw's display rendering (auto white
  point, BT.709 curve, brightness matched to the camera JPEG, optional darker rendering blended into the
  highlights) followed by an ordered `[[step]]` list of float operations: `levels`, `sigmoid`,
  `saturation`, `chroma-blur`, `stretch`, `unsharp`, `fringe`, `bright-smooth`, `clamp`. The maths
  follows ImageMagick 7 Q16-HDRI (Gaussian kernel construction, Lab/HSL formulas, which operators clamp
  and which don't), so an ImageMagick-prototyped develop can be carried over as a look file: a
  seven-command `magick` develop was reproduced at 44–56 dB PSNR (SSIM ≥ 0.994, mean ΔE2000 ≤ 0.25) on
  39 tuning and held-out 24 MP frames, ~3.7 s per frame instead of ~2.5 min.
- The new stages only run in layered looks. The four built-in looks are unchanged: every output of
  `neutral-day`, `neutral-night`, `rich-day` and `rich-night` (with `--match`, `--shadow-lift`,
  `--recover-hl`, `--strong`, `--nofringe`) is byte-identical to 0.1.1.
- Look files: the night/day tables (`tone`, `nr`, `chroma`, `defringe`, `sharpen`) are still required in
  night/day looks and are refused in layered looks; `[display]` / `[[step]]` the other way round.
- `--dump-stages` on a layered look writes a 16-bit PPM after every step.

## 0.1.1 (not released on its own; included in 0.1.2)

- `--threads N` for `develop` / `night` / `day`: use at most N CPU threads (the pixel work and LibRaw's
  decode). Default is still every logical CPU. Output is byte-identical at any thread count (checked at
  12, 3 and 1 thread on 24 MP night frames with NR on). `batch_start` gains a `threads` field and the
  progress log names the thread count.
- Docs: running batches side by side is fine. Two rawdev batches at once finished ~10% sooner than the
  same batches one after the other (12 threads; 46.9 s vs 42.6 s, NR-heavy night frames), so there is
  no need to queue them. Use `--threads` when the point is to leave room for other work, not for speed.

## 0.1.0 (2026-10-18)

First public release.

- `rawdev develop`: any LibRaw-supported RAW (tested: Canon CR3/CR2, Nikon NEF, Sony ARW, Fujifilm RAF
  X-Trans, Olympus ORF, Panasonic RW2, Pentax PEF, DNG from Ricoh and a phone), by file or folder.
- Tone curve fitted to the camera's own rendering: sidecar JPEG, else the RAW's embedded preview, else a
  fixed neutral curve (`--tone-ref auto|preview|neutral`).
- Four built-in looks as TOML files: `neutral-day` (default), `neutral-night`, `rich-day`, `rich-night`;
  custom looks with `--look FILE`; per-frame options by flag or `--recipe recipe.toml`.
- Agent contract: `--json` event lines, `--dry-run`, `rawdev-manifest.json` with SHA-256 of every input
  and output, exit codes 0/1/2/3 (docs/JSON.md).
- Safety guards: never writes next to the RAWs, into a shoot's `dev/`, or into configured protected
  folders (symlinks resolved, checked before anything is created); no overwrite without `--overwrite`;
  never rotates, levels or crops.
- `rawdev ingest`: SHA-256 verified copy of a card (read-back verify, fsync, read-only copies, manifest,
  optional unmount only after everything verified). `rawdev cull`: move rejects to `_rejects/` with an
  undo log.
- `--gpu` / `--gpu-adapter`: NLM noise reduction on Vulkan (wgpu), integer-exact, byte-identical to the
  CPU on NVIDIA (proprietary driver) and AMD (RADV) and Mesa llvmpipe.
- `rawdev check`, `rawdev gpus`, `rawdev looks`; exiftool / dcraw_emu found on PATH or set by flag,
  `RAWDEV_EXIFTOOL` / `RAWDEV_DCRAW_EMU`, or the config file.
- Fujifilm RAF is decoded single-threaded: LibRaw's X-Trans demosaic is not deterministic when
  multithreaded.
