# Building rawdev with an AI coding agent

This is for anyone who wants **rawdev** — a RAW-to-JPEG develop tool built to be driven by scripts
and AI agents — on their own machine, either as-is or customised, using a coding agent such as
Claude Code, Codex, Cursor, or OpenClaw.

You don't need to read the Rust source first. Copy the whole prompt below into your agent and let
it do the work. It covers both building the existing project and, if you'd rather not use Rust,
writing a compatible tool from the spec.

## The prompt

Copy everything in the fenced block and hand it to your agent as-is. Fill in the one bracketed
choice near the top first.

````text
You are building "rawdev", a command-line RAW-photo developer designed to be driven by scripts and
AI agents. Follow this prompt in order. Stop and ask me before anything public or destructive (see
the rules below) — don't guess on those.

CHOOSE ONE PATH before you start:
  [ ] Path A — build or customise the real project (default, do this unless I say otherwise)
  [ ] Path B — write a new, compatible tool from the spec below, in Rust or another language

====================================================================
PATH A: build / customise the existing project
====================================================================

1. Clone the repository: `git clone https://github.com/nunyabiz765/rawdev` and `cd` into it.
2. Check build dependencies before compiling: run `rawdev check --json` if a prebuilt binary is on
   PATH, otherwise read the README's "Runtime dependencies" section. You need a Rust toolchain, LibRaw's
   `dcraw_emu`, and `exiftool`. Report which are present/missing before continuing.
3. Build it: `cargo build --release`.
4. Run the test suite: `cargo test --release`. Report pass/fail counts; do not silently skip
   failures.
5. If I asked for a custom look rather than a code change: generate a starting point with
   `./target/release/rawdev looks neutral-day > ~/rawdev-looks/mine.toml` (keep custom looks outside
   the repo's `looks/` folder, which holds only the built-ins), edit the copy, and use it with
   `--look ~/rawdev-looks/mine.toml`. Looks are plain TOML — the fields are documented in
   `src/lookfile.rs` and the comments in `looks/*.toml`; unknown keys are rejected.
6. If I asked for a change to defaults or to add a protected folder: protected folders live under
   `protected = [...]` in `~/.config/rawdev/config.toml` (create it if absent); confirm with
   `rawdev check --json` that your new entry shows up under `"protected"`.
7. Any other code change: keep it the smallest diff that does the job, and keep the safety rules
   below intact — they are not something to refactor away for convenience.

====================================================================
PATH B: build a compatible tool from scratch
====================================================================

Implement a CLI with this behaviour, in whatever language you and I agreed on (ask me if we
didn't). Then verify it against the real `rawdev` binary on the same input files and report any
difference in behaviour (exact pixel match is not required for a from-scratch tool, but the
contract below — commands, JSON shape, exit codes, safety refusals — must match).

SPEC SUMMARY

- Decode: shell out to LibRaw's `dcraw_emu` the same way rawdev does:
  `dcraw_emu -w -W -H 0 -o 1 -q 3 -6 -g 1 1 -Z - FILE`
  This writes a 16-bit linear PPM to stdout (camera white balance, no auto-bright, highlights
  clipped, sRGB primaries, linear gamma, AHD demosaic). For Fujifilm RAF set OMP_NUM_THREADS=1 on
  that command: LibRaw's X-Trans demosaic is not deterministic when multithreaded. Parse the PPM
  header, then process in 16-bit linear until the final tone/gamma step.
- Tone curve: fit the curve to a reference rendering of the frame, in this order of preference:
  (1) a sidecar JPEG the camera wrote next to the RAW; (2) the RAW's own embedded preview, pulled
  with `exiftool -b -JpgFromRaw FILE` or `-PreviewImage` (try the biggest tag first; only trust a
  preview if its long edge is >= 320 px and its aspect ratio matches the frame within ~3%); (3) a
  fixed neutral curve (gain/knee constants) when there is neither. The fit only needs an ~800 px
  thumbnail of the reference.
- Then: noise reduction (luma + chroma, strength varies by ISO per the look file), a small chroma
  boost, defringe (purple-ring cleanup around blown highlights), unsharp mask.
- Export: full-size JPEG at quality 92, 4:4:4 chroma subsampling, plus a second "share" copy resized
  to a 2000 px long edge at quality 90, 4:2:0. Copy EXIF and GPS from the original with
  `exiftool -TagsFromFile RAW -EXIF:all -GPS:all -Orientation=1 OUTPUT` (orientation is reset to 1
  because the pixels are already rotated upright by the decode step — never rotate the pixels a
  second time).
- JSON contract and exit codes: implement this exactly as documented in `docs/JSON.md` from the
  real repo (read that file; don't guess the shape) — one JSON object per line on stdout with
  `--json`, human text to stderr, events `batch_start`/`frame_planned`/`frame_done`/`frame_failed`/
  `batch_end` for develop-style commands, `ingest_start`/`file_planned`/`file_done`/`ingest_end` for
  ingest, `cull_start`/`move_planned`/`moved`/`cull_end` for cull, and a `rawdev-manifest.json` /
  `ingest-manifest.json` written after every real (non-dry) run. Exit codes: 0 ok, 1 some
  frames/files failed, 2 bad arguments or a safety refusal (nothing written), 3 a missing tool
  (nothing written).

NON-NEGOTIABLE SAFETY RULES — do not weaken, bypass, or "simplify away" any of these:

  - NEVER modify or delete RAW originals, or anything else on a camera card. Only ever read them.
  - NEVER overwrite an existing output file unless the user passed an explicit opt-in flag
    (`--overwrite`). Default is refuse.
  - REFUSE to write into: the folder that holds the input RAWs; a shoot's `dev/` folder (`<shoot>/dev`
    for `--shoot DIR`, and, for any input inside a folder named `raw/`, the `dev/` next to that
    `raw/`); or any folder listed under `protected` in the user's config file. Resolve all symlinks and
    check these BEFORE creating any file or directory — a symlink must not be usable to sneak past
    a guard, and nothing should be created even transiently before the check passes.
  - NEVER rotate, level, or crop a frame. Pixel orientation comes only from what LibRaw already
    applied during decode.
  - `--dry-run` must write nothing at all — not even a temp file — while still running every
    check and printing the full plan.
  - Ingest must verify every copy by re-reading it from disk and comparing a SHA-256 hash against
    the source's hash, and must only report success (and only unmount, if asked) after everything
    verified. A card is never unmounted if any file failed.
  - Cull only ever MOVES files into a `_rejects/` subfolder (never deletes), and logs every move so
    `--undo` can reverse it exactly.
  - NEVER strip or silently add metadata. EXIF/GPS is always copied through from the original
    unless the user explicitly asked otherwise.

TEST PLAN — run all of these and report the results, don't skip any:

  1. Unit tests (Path A: `cargo test --release`; Path B: your own test suite covering the safety
     guards and JSON shapes).
  2. CLI refusal tests: attempt to write into the RAW folder, into a shoot's `dev/`, and into a
     configured protected folder. Each must exit 2, print an `error` event with `"kind":"refused"`,
     and leave the filesystem untouched (check with `find`/`ls` before and after).
  3. A dry run on a real folder of RAWs: confirm it prints a full plan and that nothing new exists
     on disk afterward.
  4. A real run on 2-3 public CC0 sample RAWs. Fetch them with `tools/fetch-samples.sh` (downloads
     from raw.pixls.us, verifies checksums, never commits the files). Confirm output JPEGs exist,
     open, and have sane EXIF (orientation 1, GPS present if the sample had it).
  5. If a usable GPU is present: run `tools/gpu-parity.sh OUTDIR rich-night SAMPLES_DIR` (it tests
     every adapter `rawdev gpus` lists, including slow software ones like llvmpipe; limit it with
     e.g. `ADAPTERS=nvidia` or `ADAPTERS=radeon`) and confirm every output byte is
     identical between the CPU path and the GPU path (`cmp` reports 0 differences for every file).
  6. If you customised an existing look or the engine itself (Path A): confirm the unmodified
     stock looks (`neutral-day`, `neutral-night`, and any others shipped) still produce
     byte-identical output to an unmodified build — `cmp` the JPEGs from before and after your
     change, on the same input files. Any difference in a look you did NOT intend to change is a
     bug, not a feature. (If you only added a look file and didn't touch the code, the binary is
     unchanged, so this reduces to running the stock looks before and after and comparing.)

BEFORE ANYTHING PUBLIC OR DESTRUCTIVE — stop and ask me first. This includes: pushing or publishing
anything, deleting any file, writing output outside the folder I told you to use, changing files
outside the repo (e.g. `~/.config/rawdev/config.toml`) without telling me first, or installing
system packages. Don't route around a safety refusal by changing paths — if rawdev refuses, tell me
why and let me decide.

WHEN YOU'RE DONE, report:
  - What you changed (files, and a one-line summary of why).
  - The result of every item in the test plan above (pass/fail, not just "looks fine").
  - What you did NOT test and why (e.g. no GPU available, no second camera brand to test with).
````

## Checking what you got

Once the agent reports back, verify these yourself before trusting the build:

- **It actually built.** Run `./target/release/rawdev version` and `rawdev check --json` yourself —
  don't just take the agent's word for a green build.
- **The refusal tests really refused.** Try writing into the RAW folder yourself once; confirm exit
  code 2 and that no file appeared.
- **Dry run wrote nothing.** `find <outdir>` before and after a `--dry-run`; it should be
  byte-for-byte the same listing.
- **The manifest matches reality.** Open `rawdev-manifest.json` after a real run and spot-check one
  `outputs[].sha256` against `sha256sum` on the actual file.
- **GPS survived.** `exiftool -GPS:all output.jpg` on a sample that had location data in the RAW.
- **If you asked for a custom look:** confirm the *other* built-in looks are untouched — diff the
  look TOML files against the upstream repo, not just the one you meant to change.
- **No RAW got modified.** `sha256sum` your original sample files before and after the whole run;
  they must not move.
- **The agent asked before anything outward-facing.** If it pushed, deleted, or wrote somewhere you
  didn't name without asking, treat that as a bug in how it was briefed, not a one-off.
