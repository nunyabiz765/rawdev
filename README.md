# rawdev

`rawdev` develops RAW photos into JPEGs on Linux. It decodes with LibRaw (`dcraw_emu`), runs the
tone/colour/noise-reduction pipeline in Rust, and writes JPEGs with EXIF and GPS carried over from the
RAW. It is built to be driven by scripts and AI agents as much as by a person at a terminal: every
command can print one JSON object per line, `--dry-run` shows the plan before anything is written, every
real run leaves a manifest with SHA-256 hashes of every input and output, exit codes are documented, and
a set of safety guards make it hard to write into the wrong place. Noise reduction can optionally run on
the GPU; that path is designed to be bit-exact with the CPU, not just close.

## Install

### Prebuilt binary (x86-64 Linux)

Download the tarball and its checksum from the [Releases](../../releases) page. The main build needs a
CPU with AVX2 (x86-64-v3: Intel Haswell / AMD Excavator, ~2015, or newer); for older CPUs use
`rawdev-0.1.2-x86_64-baseline-linux.tar.gz` instead (same output, ~20% slower noise reduction).

```sh
curl -LO https://github.com/nunyabiz765/rawdev/releases/download/v0.1.2/rawdev-0.1.2-x86_64-linux.tar.gz
curl -LO https://github.com/nunyabiz765/rawdev/releases/download/v0.1.2/rawdev-0.1.2-x86_64-linux.tar.gz.sha256
sha256sum -c rawdev-0.1.2-x86_64-linux.tar.gz.sha256
tar xzf rawdev-0.1.2-x86_64-linux.tar.gz
install -Dm755 rawdev-0.1.2-x86_64-linux/rawdev ~/.local/bin/rawdev
```

The binary is built on Debian 12 (glibc 2.36), so it runs on that or anything newer (Ubuntu 24.04,
Fedora 37+, current Arch); on older systems (e.g. Ubuntu 22.04) build from source. libjpeg-turbo is linked in; LibRaw and exiftool are not (see below).

### AUR

`rawdev` (builds from source) or `rawdev-bin` (prebuilt), once published.

### From source

```sh
cargo build --release
```

Needs Rust 1.88+ and `libturbojpeg` + `pkg-config` on the system. To build libjpeg-turbo from source
and link it statically instead (what the release binaries do), add `--features vendored-jpeg` (needs
`cmake` and `nasm`). `packaging/release.sh` builds the release tarball in a Debian 12 container.

### Runtime dependencies

rawdev shells out to two external tools rather than linking them, so install them separately:

| Tool | Arch | Debian / Ubuntu | Fedora |
|---|---|---|---|
| `dcraw_emu` (from LibRaw) | `libraw` | `libraw-bin` | check your distro |
| `exiftool` | `perl-image-exiftool` | `libimage-exiftool-perl` | `perl-Image-ExifTool` |

A Vulkan driver is optional, only needed for `--gpu`: `mesa-vulkan-drivers` (generic Mesa), or the
vendor-specific `vulkan-radeon` (AMD), `vulkan-intel` (Intel), or the proprietary NVIDIA driver.

Run `rawdev check` after installing to confirm both tools are found and to see what GPUs (if any) are
usable.

## Quick start

```sh
rawdev check

# see the plan, write nothing
rawdev develop --dry-run --out developed ~/Photos/card/*.NEF

# the real run
rawdev develop --out developed ~/Photos/card/*.NEF

# machine-readable output
rawdev develop --json --out developed ~/Photos/card/*.NEF

# noise reduction on the GPU (bit-exact with the CPU)
rawdev develop --gpu --out developed ~/Photos/card/*.NEF
```

If your files are laid out as a "shoot" with RAWs in `DIR/raw/` and camera JPEGs in `DIR/jpg/`, the
`night` and `day` subcommands take frame numbers or stems directly:

```sh
rawdev night --shoot ~/Photos/2026-10-18-park --out developed 1 2 3
rawdev day   --shoot ~/Photos/2026-10-18-park --out developed 1 2 3
```

Getting photos off a card and sorting out rejects:

```sh
rawdev ingest /run/media/$USER/CARD --to ~/Photos --name park --unmount
rawdev cull ~/Photos/2026-10-18-park --reject 12,15
rawdev cull ~/Photos/2026-10-18-park --undo
```

## Outputs

For each input `IMG_0001.CR3` written to `--out DIR`:

| File | What it is |
|---|---|
| `DIR/IMG_0001.jpg` | Full size, quality 92, 4:4:4 chroma, sRGB profile embedded, EXIF + GPS copied from the RAW. Orientation is reset to 1, because LibRaw already rotated the pixels to match it. |
| `DIR/share/IMG_0001.jpg` | 2000 px long edge, quality 90, for sharing. |
| `DIR/progress.log` | One line per frame, human-readable. |
| `DIR/rawdev-manifest.json` | One entry per run: tool versions, the look used, and the SHA-256 of every input and output. See `docs/JSON.md`. |

GPS is copied by default, the same as the camera's own JPEG would carry it. If you're about to share a
photo and don't want its location attached, strip it yourself before sending it anywhere:

```sh
exiftool -gps:all= IMG_0001.jpg
```

## Safety

These are guarantees, not just defaults:

- rawdev only ever **reads** RAW files. It never deletes anything.
- It never overwrites an existing output unless you pass `--overwrite`.
- It refuses to write into the folder holding the RAWs you're developing.
- It refuses to write into a shoot's `dev/` folder (`<shoot>/dev`), including when an input path is
  itself inside a `<shoot>/raw/` folder — that folder is assumed to belong to a different pipeline.
- It refuses to write into any folder listed under `protected` in the config file.
- Paths are resolved through symlinks first, so a symlink can't be used to get around any of the above.
- `rawdev ingest` verifies every copy by reading it back and comparing its SHA-256 to the source, makes
  the copies read-only, never writes to the card itself, and only unmounts if every file verified.
- `rawdev cull` only *moves* files into `DIR/_rejects/`, and logs every move so `--undo` can put them
  back.
- rawdev never rotates, levels, or crops a frame. There is no code path for it: `--rotate`, `--level`,
  `--straighten`, and `--crop` are all rejected with a message explaining why.

## How the tone curve works

The tone curve is fitted to the camera's own rendering of the frame, so colours and brightness start
from what the camera already decided, not a guess. In order of preference:

1. a sidecar JPEG the camera wrote next to the RAW (RAW+JPEG shooting, or `<shoot>/jpg/` for `--shoot`
   layouts);
2. the RAW's embedded preview, extracted without writing anything to disk;
3. a fixed neutral curve, when there's neither.

`--tone-ref auto|preview|neutral` controls this (default `auto`). The sky-colour matrix (`--match`, day
looks only) needs its reference to line up with the frame pixel-for-pixel, so it additionally requires
the reference to be the same orientation and a close enough aspect ratio; otherwise it's skipped and
noted in the output.

## Looks

A look is a TOML file holding every number that shapes the image: the noise-reduction table, chroma
lift, defringing, sharpening, and export sizes. Four looks ship built in:

| Look | Recipe | Default for | Description |
|---|---|---|---|
| `neutral-day` | day | `rawdev develop` | Camera-matched or neutral curve, light NR above ISO 400, gentle colour and sharpening |
| `neutral-night` | night | — | Camera-matched or neutral curve pulled 3%, NLM by ISO, small chroma lift, mild defringe |
| `rich-day` | day | `rawdev day` | Camera-matched curve, NR above ISO 200, +30% muted chroma, sky matrix, CA cleanup |
| `rich-night` | night | `rawdev night` | Camera-matched curve pulled 3%, NLM by ISO, +30% muted chroma, ring defringe |

A look is just a file:

```sh
rawdev looks rich-day > mine.toml
# edit mine.toml
rawdev develop --look mine.toml --out developed ~/Photos/card/*.NEF
```

Unknown keys in a look file are errors, so a typo doesn't get silently ignored.

### The layered recipe

The built-in looks use the `night` and `day` recipes: a tone curve fitted to the camera's JPEG, then
8-bit noise reduction and colour stages. A look can instead say `recipe = "layered"`: LibRaw's own
display rendering (BT.709 curve, auto white point, brightness matched to the camera JPEG, optionally a
darker rendering blended into the highlights), followed by an ordered list of float steps, like layers
in an image editor:

```toml
recipe = "layered"

[display]
clip_share = 0.01          # auto white: 1% of pixels may land above white
match_power = 1.8          # brightness = (camera JPEG mean / frame mean) ^ 1.8 ...
brightness_min = 0.5       # ... clamped
brightness_max = 3.0

[[step]]
op = "levels"
black = 0.01
white = 0.98

[[step]]
op = "sigmoid"
contrast = 3.0
midpoint = 0.45
```

Steps: `levels`, `sigmoid`, `saturation` (HSL, weighted towards muted colours), `chroma-blur` (Lab
a/b), `stretch` (per-channel contrast stretch), `unsharp` (large sigma = local contrast, small =
sharpening), `fringe` (purple-fringe desaturation in the top of the frame, on edges only),
`bright-smooth` (median-smooth clipped skies and bright areas), and `clamp`. Every parameter is
documented in `src/lookfile.rs` (`enum Step`). The maths follows ImageMagick 7 (Q16 HDRI) operator for
operator, Gaussian kernels and Lab/HSL formulas included, so a look first prototyped as a `magick`
pipeline can be moved over and give the same picture: a seven-command ImageMagick develop (24 MP,
~2.5 min per frame) was reproduced at 44–56 dB PSNR (SSIM ≥ 0.994, mean ΔE2000 ≤ 0.25) on 39 frames,
~3.7 s each. Values may go above 1 or below 0 between steps, as in ImageMagick's HDRI build; `clamp` is
where a `magick` pipeline would have written a 16-bit file. Layered looks have no per-frame flags (`--match`, `--strong`, ...): make a second look
file for frames that need different settings.

Per-frame choices (`--match`, `--shadow-lift`, `--recover-hl` for day looks; `--strong`, `--nofringe`
for night looks) can also go in a recipe file instead of flags:

```toml
# recipe.toml
look = "rich-day"

[frames.IMG_0007]
match = true
shadow_lift = true
```

```sh
rawdev day --shoot ~/Photos/2026-10-18-park --out developed --recipe recipe.toml 7
```

## Config file and environment variables

`~/.config/rawdev/config.toml` (or `$XDG_CONFIG_HOME/rawdev/config.toml`, or the path in
`RAWDEV_CONFIG`):

```toml
exiftool = "/opt/exiftool/exiftool"
dcraw_emu = "/usr/local/bin/dcraw_emu"
protected = ["~/Photos/archive", "/mnt/nas/originals"]
```

Environment variables: `RAWDEV_EXIFTOOL`, `RAWDEV_DCRAW_EMU` (override the tool paths),
`RAWDEV_CONFIG` (override the config file location), `RAWDEV_GPU_TIMING` (print GPU step timings).

## For agents

- `docs/JSON.md` — the full JSON event and manifest schema, with exit codes.
- Background batches: add `--threads N` (e.g. half the CPU's threads) to leave room for the user. Two
  batches may run at the same time; there's no need to queue them (see "Benchmarks").
- The OpenClaw skill at `packaging/openclaw/rawdev/SKILL.md` covers card ingest, culling, and
  developing from an agent.
- `docs/BUILD-WITH-AI.md` — a prompt for handing this project's spec to a coding agent.

## GPU notes

`--gpu` runs noise reduction (NLM) on the GPU through Vulkan. `--gpu-adapter NAME` picks a specific
adapter by (partial) name. `rawdev gpus` lists what's available.

The GPU shader does the same integer-only math as the CPU path, using the same lookup table, so its
output is designed to be byte-identical to the CPU's — not an approximation. A software Vulkan renderer
(e.g. Mesa's llvmpipe) is refused unless you name it explicitly with `--gpu-adapter`, since it's slower
than the CPU and exists mainly for compatibility testing.

The CPU path is the default and is the reference implementation: if a GPU run and a CPU run ever
disagree, the CPU is right.

## Cameras tested

| Camera | Format | Sensor | Tone reference used (`--tone-ref auto`) |
|---|---|---|---|
| Canon EOS R100 | CR3 | APS-C Bayer | sidecar JPEG (the camera rawdev was tuned on) |
| Canon EOS 5D Mark II | CR2 | full-frame Bayer | embedded preview |
| Nikon D7000 | NEF | APS-C Bayer | embedded preview |
| Sony DSLR-A200 | ARW | APS-C Bayer | embedded preview |
| Fujifilm X-T2 | RAF | APS-C X-Trans | embedded preview |
| Olympus E-M10 | ORF | Four Thirds Bayer | embedded preview |
| Panasonic DMC-GX85 | RW2 | Four Thirds Bayer | embedded preview |
| Pentax K-7 | PEF | APS-C Bayer | embedded preview |
| Ricoh GR II | DNG | APS-C Bayer | embedded preview |
| Samsung Galaxy S8 (SM-G950U) | DNG (phone, portrait) | phone Bayer | neutral curve (no JPEG preview inside) |

Each was checked for: decodes without errors, upright orientation (the portrait phone frame comes out
portrait), sensible exposure and colour, EXIF copied, and the forced `--tone-ref neutral` fallback.
Every frame was also developed on the CPU and on three Vulkan devices and compared byte for byte (see
"GPU notes").

**Fujifilm X-Trans is decoded single-threaded.** LibRaw's X-Trans demosaic gives a slightly different
image on every run when it uses several threads (three runs of the same file, three different results).
rawdev runs it on one thread so output is reproducible; that makes an X-T2 frame's decode take ~10–13 s
instead of ~3 s. Bayer formats were checked the same way and are deterministic.

Test RAWs come from [raw.pixls.us](https://raw.pixls.us) (CC0) via `tools/fetch-samples.sh`; they are
not shipped in this repository.

## Benchmarks

Measured 2026-10-18 on one desktop: Ryzen 5 8600G (6 cores / 12 threads), RTX 2060 (NVIDIA 615
driver), Radeon 760M iGPU (Mesa RADV 26.2), 32 GB RAM. Other work was running at the same time (load
average around 11), so treat these as conservative. Times are whole batches, wall clock, including
decode, exiftool and JPEG encode; "NR" is the noise-reduction step alone, per frame.

| Batch | CPU (12 threads) | RTX 2060 | Radeon 760M | llvmpipe (software Vulkan) |
|---|---|---|---|---|
| 3 × 24 MP Canon R100 frames, `rich-night` (NR on all) | 23.7 s (NR 6.6 s) | 10.1 s (NR 2.0 s) | 11.4 s (NR 2.5 s) | 268 s (NR 88 s) |
| 4 × 24 MP Canon R100 frames, `rich-day` (NR on 3) | 27.6 s (NR 6.7 s) | 13.1 s (NR 2.0 s) | 14.2 s (NR 2.4 s) | 276 s (NR 90 s) |
| 9 public samples, 10–24 MP, `rich-night` | 51.0 s (NR 4.0 s) | 25.6 s (NR 1.0 s) | 28.3 s (NR 1.3 s) | 530 s (NR 58 s) |

- **Bit-exact:** every JPEG from every GPU run above is byte-identical (`cmp`) to the CPU run's: 32 files
  per device, 96 comparisons, 0 differences (`tools/gpu-parity.sh`). llvmpipe is only there to show the
  shader is exact on a third, completely different Vulkan implementation; never use it for real work.
- **Without NR** (daylight frames at low ISO) a 24 MP frame takes about 1.5–2.5 s on the CPU; the GPU
  doesn't help there.
- The 9-sample GPU batches are held back by the single-threaded Fujifilm X-Trans decode (~10 s for that
  one frame; see "Cameras tested").
- **CPU builds:** the release binary is built for x86-64-v3 (AVX2), which measured the same as a
  `-C target-cpu=native` build here (23.1–23.4 s vs 23.1–23.5 s on the night batch). The baseline x86-64
  build is about 22% slower on NR-heavy frames (28.8–29.1 s). All three produce byte-identical output.
- **Threads and parallel batches** (measured 2026-10-21, same machine): `--threads N` caps a batch at N CPU threads; output is byte-identical
  at any count. The 3-frame night batch took 23.0 s on all 12 threads and 44.9 s with `--threads 3`; one
  frame alone took 41.2 s with `--threads 1`. Two batches started side by side share the CPU well and
  finished ~10% sooner than running them one after the other (42.6 s vs 46.9 s for 3 + 3 night frames;
  10.2 s vs 12.8 s for 4 + 4 low-ISO day frames), so scripts don't need to queue batches. Use
  `--threads` to keep a desktop responsive, not to go faster.
- For scale: the Python/numpy/OpenCV prototype this engine was ported from took 75–130 s per 24 MP night
  frame on the same machine; rawdev's CPU path is about 8 s, with the same output to within rounding.

## Limitations

- The `rich-*` looks were tuned on one sensor (a Canon EOS R100), and the neutral looks have only been
  checked on the ten cameras above. Other cameras may want a custom look; see "Looks" above.
- Byte-identical output is only promised for the same rawdev version, LibRaw version, libjpeg-turbo
  version, and exiftool version as a previous run — the manifest records all four so you can check.
- No Intel GPU has been tested with `--gpu`.
- Binaries are Linux x86-64 only. Other platforms need a source build.
- This is a small open-source project; support is best-effort.

## Licence

Dual-licensed under MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`) — pick whichever suits you. The
`NOTICE` file credits OpenCV and Pillow, whose colour and filtering algorithms were ported into this
codebase, and LittleCMS, which generated the embedded sRGB profile. The `neutral-day` and
`neutral-night` looks are CC0.

Built by Center Point Labs (NICKSOSA.TECH LLC) — https://centerpointlabs.dev/

## Contributing

Issues and pull requests are welcome; this is a best-effort open-source project, not a supported
product. Before sending a change, run:

```sh
cargo test --release
tools/fetch-samples.sh scratch/samples                       # public CC0 test RAWs
RAWDEV=target/release/rawdev tools/gpu-parity.sh /tmp/parity rich-night scratch/samples
```
