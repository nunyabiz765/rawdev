# Developing rawdev

A code tour for someone learning Rust from a real, working codebase. The teaching voice here matches
the project's internal style: short explanations next to the code they describe, no hand-waving about
what's "obvious."

`main.rs` is the binary; everything else lives in the library (`lib.rs`), so unit tests and the examples
under `examples/` can call the same code the CLI does. That's the standard Rust split between a thin
binary and a library crate.

## Project layout

```
Cargo.toml          package manifest: name, edition, dependencies, build profiles
Cargo.lock          exact dependency versions (committed, so builds are repeatable)
assets/srgb-lcms.icc the sRGB ICC profile, embedded in the binary with include_bytes!
src/
  main.rs     the CLI: subcommands, argument parsing, the safety checks, the batch loop, the log
  lib.rs      the library root: declares the modules below and the shared Result type
  image.rs    Rgb8 / Gray8 / GrayF32: plain Vec-of-samples image containers
  decode.rs   runs dcraw_emu and parses its 16-bit PPM output
  tone.rs     linear RAW -> 8-bit sRGB: curve fit, lookup tables, vibrance
  day.rs      the day recipe's extra stages: sky matrix, shadow lift, CA fringe pass
  look.rs     noise_reduction, chroma_lift, defringe: the look stages both recipes share
  lookfile.rs the look-file format (serde + toml): structs, loading, validation, NR table lookup
  layered.rs  the layered recipe: LibRaw display rendering + float steps that follow ImageMagick 7 HDRI
  nlm.rs      non-local means denoising, from scratch, multithreaded (the CPU reference)
  nlm.wgsl    the GPU compute shader (WGSL) that gpu.rs compiles at startup
  gpu.rs      the same NLM on the GPU through wgpu (--gpu): device setup, buffers, dispatch, readback
  opencv.rs   bit-exact ports of the cv2 pieces: Lab, YCrCb, median, dilate, Gaussian
  pillow.rs   bit-exact ports of the PIL pieces: thumbnail/resize/reduce, box blur, unsharp mask
  reference.rs the tone reference: sidecar JPEG, embedded preview, or neutral curve
  export.rs   JPEG read/write (libjpeg-turbo), ICC insertion, exiftool calls
  develop.rs  one frame start to finish (night, day or layered), timing each step
  recipe.rs   per-frame choices: command-line selector lists and the recipe.toml format
  manifest.rs the --json event lines and the run manifest: hashing, timestamps, atomic writes
  safety.rs   the write guards: protected folders, dev/ detection, symlink resolution
  ingest.rs   card ingest (verified copy) and cull (move to _rejects/, with undo)
  tools.rs    finding exiftool/dcraw_emu, the optional config file
looks/        the built-in looks (neutral-day, neutral-night, rich-day, rich-night), compiled in with
              include_str!
docs/         JSON.md (the agent contract), BUILD-WITH-AI.md, this file
tests/cli.rs  command-line contract tests: exit codes, JSON events, refusals, dry runs write nothing
examples/nlm_bench.rs   times NLM alone, CPU vs GPU, with a --check mode that counts differing samples
tools/        fetch-samples.sh + samples.tsv (public CC0 test RAWs), gpu-parity.sh (CPU vs every GPU,
              byte for byte), ref_primitives.py (cv2/PIL reference data for the ignored tests)
packaging/    release.sh (portable release tarball), AUR PKGBUILDs, the OpenClaw skill
```

## Rust ideas you'll meet, and where

- **Ownership and borrowing.** Functions take `&Rgb8` (a read-only borrow) and return a new `Rgb8`;
  nothing is mutated behind your back. `develop.rs` calls `drop(raw)` to free the RAW buffer as soon as
  the tone stage no longer needs it.
- **`Result` and `?`.** Every fallible call returns `Result<T>`. A trailing `?` means "if this failed,
  return the error now." `lib.rs` defines `Result<T>` as "T, or any boxed error," which keeps function
  signatures short without a custom error enum for every module.
- **rayon.** `par_chunks_mut(width * 3)` hands each image row to a different thread. Swap `par_` for
  plain `chunks_mut` and the same code runs single-threaded — most of the speed comes from this, not
  from clever math.
- **Lookup tables.** LibRaw outputs 16-bit integers, so `tone.rs` computes the whole per-sample chain
  (normalize, clip, curve, scale) once for each of the 65,536 possible values. After that, each pixel
  costs a few array reads.
- **Const generics.** `nlm::denoise::<C>` is one function that works for 1 channel (luma) or 2 channels
  (a/b of Lab), and the compiler builds a specialized copy for each `C`.
- **Enums that carry data.** `develop::Recipe` is either `Night(FrameOptions)` or `Day(DayOptions)`, and
  `match job.recipe { ... }` picks the code path. The compiler refuses to build if a `match` forgets a
  case, so adding a third recipe later can't silently fall through.
- **`Option`.** "Maybe there's a value," without null: `Look::nr_params` returns `None` at ISOs where the look turns NR
  off, and `day::match_sky` returns `None` when a frame has no usable blue sky.
- **`&mut` borrows.** `day::shadow_lift(&mut rgb)` edits the image in place. While that mutable borrow
  is live, nothing else can read `rgb` — that's how Rust rules out data races at compile time, not just
  at runtime.
- **`OnceLock`.** In `opencv.rs`, lookup tables (like the Lab conversion tables) are built the first time
  they're needed and then shared read-only by every thread.
- **A process-wide "maybe" value.** `gpu.rs` keeps the open GPU in a `static OnceLock<Option<GpuNlm>>`:
  `--gpu` fills it once (with `None` if no usable GPU was found), and `nlm::denoise_colored` asks
  `gpu::active()` every time it needs to decide CPU vs GPU. It and the two tool paths in `tools.rs` (found once at
  start-up) are the only global state in rawdev; threading a GPU handle through every recipe function by argument would touch a lot of code
  for one flag.
- **`include_str!` / `include_bytes!`.** The shader source `nlm.wgsl` and the sRGB ICC profile are baked
  into the binary at compile time, the same way the built-in looks are. `str::replace` fills in the
  channel count before the GPU compiles the shader.
- **`as` casts.** `x as i64` truncates toward zero, the same as C's `(int)x`. The Pillow port relies on
  that to match PIL's own fixed-point rounding.
- **Tests.** `cargo test --release` runs the quick tests. `cargo test --release -- --ignored` runs the
  heavier ones, which check every primitive against real cv2/PIL reference output. See "Testing" below.

## Decoding: why shell out to `dcraw_emu` instead of linking libraw

Two options were on the table: bind libraw's C API through FFI and set the same decode options in code,
or run LibRaw's own `dcraw_emu` tool with the exact flags the reference recipe uses and read its output
from a pipe. rawdev does the second.

- **Parity is guaranteed, not re-derived.** It's the same binary with the same flags, and its output is
  checked against the reference pixel for pixel. Through FFI, every flag would have to be mapped by
  hand onto libraw's `params` struct, and a mistake there would be subtle and easy to miss.
- **No `unsafe` code.** FFI means raw C pointers and `unsafe` blocks — the hardest part of Rust to learn
  first, and unnecessary here.
- **It's cheap.** Decoding takes well under a second, and `main.rs` runs it on a background thread for
  the *next* frame while the current one develops, so in a batch it's mostly hidden behind other work.

Moving to FFI later would save the pipe copy and open up options like half-size decode for previews. The
natural time to revisit that is if a future GPU phase needs the raw data in device memory anyway.

JPEG reading and writing goes through libjpeg-turbo (the system library, via the `turbojpeg` crate) —
the same library Pillow uses, with the same settings, so camera JPEGs decode to identical pixels and
rawdev's own output encodes identically to what the Python/Pillow prototype produced. An earlier attempt
used a pure-Rust JPEG decoder; it differed from libjpeg-turbo on a small fraction of camera-thumbnail
samples, which was enough to nudge the fitted tone curve and shift pixels by one level. Switching to the
system library fixed it.

## The night recipe, step by step

| Step | Module |
|---|---|
| Decode RAW (`dcraw_emu`, same flags, read from a pipe) | `decode.rs` |
| Green-clip neutralize | `tone.rs` |
| Tone curve fitted to the reference image (max-RGB), Hermite shoulder, x0.97 pull for night | `tone.rs::fit_curve` |
| sRGB encode + vibrance | `tone.rs` (lookup tables) |
| Noise reduction: optional pre-median, NLM, Cr/Cb median, luma blend-back | `look.rs::noise_reduction` + `nlm.rs` |
| Lab chroma lift on muted colours | `look.rs::chroma_lift` |
| Purple-fringe ring cleanup around blown lights (skippable per frame) | `look.rs::defringe` |
| Unsharp mask | `pillow.rs::unsharp_mask` |
| Full + share JPEG export | `pillow.rs::thumbnail` + `export.rs` |
| EXIF/GPS copy, Orientation reset to 1 | `export.rs::copy_exif` |

## The day recipe, step by step

The day recipe is the night engine with these changes — ISO decides most of noise reduction on its own;
the rest come from per-frame flags.

| Step | Module |
|---|---|
| Decode RAW (optionally `-H 2` highlight blend for `--recover-hl` frames) | `decode::decode_raw_with` |
| Green clip, curve fitted to the reference (no night pull, curve x1.0) | `tone::tone_linear` |
| Sky matrix (`--match`): resize both images, fit a 3x3 colour matrix on blue-sky pixels, blend through a soft blue mask | `day::match_sky` + `opencv::resize_area_f32` + `pillow::Filter::Box` |
| Shadow lift (`--shadow-lift`): linear-light gamma lift, fading out above a threshold | `day::shadow_lift` |
| sRGB encode + vibrance (f32, not a LUT, so the matrix and lift can run first) | `tone::encode_srgb` |
| Noise reduction by ISO, scaled up for `--recover-hl`/hot frames | `look::NrParams::for_day` + `look::noise_reduction` |
| Chroma lift, scaled down on `--match` frames where the sky matrix already worked | `look::chroma_lift_weighted` |
| Ring defringe (always on in the day recipe) | `look::defringe` |
| CA fringe pass on high-contrast edges | `day::ca_fringe` |
| Unsharp, export, EXIF | shared with night, `develop::finish` |

Not ported, on purpose: automatic horizon-leveling/rotation, and anything that would crop or rotate a
frame. Those are rejected at the command line (see the README's "Safety" section), not quietly skipped —
there is no code path for them at all. This is a project rule, not a default that a flag can override.


## The layered recipe, step by step

A different engine for looks first written as an ImageMagick pipeline. No tone-curve fit, no NLM: the
look file lists the steps.

| Step | Module |
|---|---|
| Decode RAW (same `dcraw_emu` linear decode as the other recipes) | `decode.rs` |
| Auto white point from the 8192-bin histogram (LibRaw `write_ppm_tiff`) | `layered::auto_white` |
| BT.709 display curve (LibRaw `gamma_curve`), brightness `(ref mean / frame mean)^power` | `layered::display_curve`, `render` |
| Optional darker rendering blended in through a blurred luma mask | `layered::render` |
| The `[[step]]` list, in order, on f32 planes (values may leave 0..1 until a `clamp`) | `layered::apply` |
| 8-bit rounding, full + share JPEG export, EXIF/GPS copy | `develop::export_tail` |

What makes it match ImageMagick (each found by comparing against `magick` intermediates): the Gaussian
kernel is built ImageMagick's way (`kernel_width`, sampled at thirds of a pixel); `-level` clamps but
`-sigmoidal-contrast`, `-unsharp` and `-modulate` don't (HDRI); Lab → sRGB lifts all channels by the
most negative one when out of gamut; `-contrast-stretch` with `-channel RGB` is per channel on a
65536-bin histogram; `-statistic median` takes index n/2 of the 16-bit window (sliding histogram here).

What can't match pixel for pixel: HSL saturation is `c / (2 - 2L)` above mid-grey, so where a large
`unsharp` pushes a near-white pixel's lightness to 1.00001 the saturation jumps to about -20 and a
following `saturation` step turns it pink or green; at 0.99999 it is left alone. Which side a pixel lands
on depends on the last bits of everything before it (ImageMagick's sRGB decode uses an approximate pow),
so both engines produce these specks along dark edges against blown sky, but not on the same pixels.
## The GPU path and why it's bit-exact

**Compute shaders and wgpu, briefly.** A GPU runs one small program (a *shader*) on thousands of threads
at once, each on its own piece of data. A *compute* shader just reads and writes buffers of numbers;
nothing gets drawn to a screen. Threads come in *workgroups* — here, 16x16 = 256 threads, one per output
pixel. A workgroup shares a small, fast on-chip memory, and `workgroupBarrier()` makes every thread in
the group wait until all of them reach that line, so no thread reads shared data before another thread
has written it. The shader lives in `src/nlm.wgsl`, written in WGSL (WebGPU's shading language); its
syntax is close to Rust: `let`, `var`, `fn`, `u32`, `array<u32, N>`.

[wgpu](https://wgpu.rs) is the Rust library that drives the GPU — Vulkan here, Metal or DirectX
elsewhere. `gpu.rs` follows the standard sequence: open an *adapter* (the physical card), then a
*device* + *queue*, compile the shader into a *pipeline*, create *buffers* and upload the input, record
"run this many workgroups" into a *command encoder* and submit it, then copy the result into a buffer
the CPU can *map*, wait for it, and read it back.

**How the shader stays exact.** Each workgroup loads its neighbourhood of the image once from GPU
memory. For each of the 441 search offsets, it builds a squared-difference image for the block, sums it
in two passes (columns, then rows) to get each thread's patch distance — the same running-sum trick the
CPU code uses in `nlm.rs`. Each thread then looks up its weight in the *same* integer table computed on
the CPU and uploaded once, and accumulates weighted sums in registers. At the end it divides with the
same rounding OpenCV uses. All of the math is 32-bit integer, and the scale factor is chosen so the sums
can't overflow a `u32` — so there is nothing to round differently between CPU and GPU, and the result is
exact, not merely close.

## How parity with the prototype was proven

rawdev's engine is a port of a Python prototype (numpy, OpenCV/cv2, and Pillow), which is not included
in this repository. "Port," not "reimplementation from a spec": the goal throughout was the same numbers
out, not just a visually similar result. Each primitive OpenCV or Pillow routine rawdev needed
(`opencv.rs`, `pillow.rs`) was translated from the library's own C source — the same lookup tables, the
same fixed-point arithmetic, the same rounding — rather than rewritten against general colour-science
references. That is why the ported primitives are integer-exact against the libraries they replace, not
merely close.

The tests that prove this need reference data that isn't checked into the repo, generated by
`tools/ref_primitives.py` against a real OpenCV/Pillow installation. Those tests are marked `#[ignore]`
so a normal `cargo test` doesn't need Python or the reference files; run them deliberately when you've
changed one of the ported primitives.

## Testing

```sh
cargo test --release                    # the quick tests: no GPU, no external reference data needed
cargo test --release -- --ignored       # the heavy tests: need a GPU, and/or reference data from
                                         # tools/ref_primitives.py
tools/gpu-parity.sh                     # CPU vs GPU output on a small batch, compared byte for byte
```

The `--release` profile matters here: the reference-data tests compare multi-megabyte buffers and are
slow enough in debug mode to be impractical.

## Building: portable vs native

The default release build is portable — it doesn't assume a particular CPU's instruction set. To build
for *this* machine's CPU specifically (a worthwhile speed gain for local use, not for anything you plan
to copy to another machine), build with:

```sh
RUSTFLAGS="-C target-cpu=native" cargo build --release
```

Binaries built this way may use instructions your other machines don't have and can crash with an
"illegal instruction" error if copied elsewhere. Release binaries published for others are always built
portable.
