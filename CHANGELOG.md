# Changelog

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
