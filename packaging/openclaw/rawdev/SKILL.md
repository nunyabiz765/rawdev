---
name: rawdev
description: "Ingest a camera card, cull rejects, or develop RAW photos to JPEG with rawdev: verified copy, dry-run plans, JSON events, manifest lookups"
homepage: https://github.com/nunyabiz765/rawdev
metadata:
  {
    "openclaw":
      {
        "emoji": "🎞️",
        "requires": { "bins": ["rawdev", "exiftool", "dcraw_emu"] },
      },
  }
---

# rawdev

`rawdev` develops RAW photos (CR2/CR3/NEF/ARW/RAF/ORF/RW2/DNG and other LibRaw formats) to JPEG, and
can ingest a camera card and cull rejects. It's built for agents: every command takes `--json` for
machine-readable events, `--dry-run` to preview without writing anything, and refuses outright
rather than risk damaging a photo library. Read `docs/JSON.md` in the rawdev repo for the full event
schema if you need a field this skill doesn't mention.

Install: no `install` entries are confirmed for this skill yet. If `rawdev` is missing, check for an
AUR package (`rawdev` or `rawdev-bin`) on Arch, or point the user to the GitHub releases page at the
homepage above. Don't guess at a package manager you haven't verified.

## Setup check

1. Run `rawdev check --json`.
2. Parse the single `check` event. If `"ok":true`, tools and GPUs are ready — proceed.
3. If the process exits 3, the `error` event names the missing tool (`exiftool` or `dcraw_emu`).
   Tell the user which package provides it (Arch: `perl-image-exiftool`, `libraw`; Debian/Ubuntu:
   `libimage-exiftool-perl`, `libraw-bin`) and stop until it's installed.

## Ingest a card

1. Find the card: list block devices with `lsblk -o NAME,MOUNTPOINT,LABEL` or look under
   `/run/media/$USER/*`. Confirm the mount point with the user if more than one removable volume is
   present.
2. Plan first: `rawdev ingest /run/media/$USER/EOS_DIGITAL --to ~/Photos --name SHOOT_NAME --dry-run --json`.
   Read the `ingest_start` event for file count and total bytes, and show the user the planned
   destination shoot folder before doing anything real.
3. Run it for real with the same flags minus `--dry-run`. Confirm success by checking the
   `ingest_end` event has `"failed":0`.
4. Only add `--unmount` if the user asked to eject the card. It only unmounts when every file
   verified; if `ingest_end.unmounted` is `null`, say so rather than assuming it worked.
5. Report back the shoot folder path (`ingest_end.shoot`) and the manifest path
   (`ingest_end.manifest`) so the user can look up files later.

## Cull rejects

1. Never decide which frames are rejects yourself. Only act on a list the user gave you (by file
   stem like `IMG_0007` or by trailing number like `7`).
2. Plan first: `rawdev cull SHOOT_DIR --reject IMG_0007,IMG_0012 --dry-run --json`, and show the
   user the `move_planned` entries before proceeding.
3. Run it for real without `--dry-run`. Confirm `cull_end.failed == 0` and tell the user the files
   moved to `SHOOT_DIR/_rejects/`, not away.
4. Mention `rawdev cull SHOOT_DIR --undo` moves back everything recorded in the cull log — rawdev never
   deletes files, only moves them.

## Develop RAW to JPEG

1. Pick a look with the user: `neutral-day` is the default for daylight frames; a `night` look
   (e.g. `neutral-night`) for after-dark; a `rich-*` look if the user specifically wants punchier,
   more processed output. List options with `rawdev looks --json` if unsure.
2. Plan first: `rawdev develop --out OUTDIR --look LOOK --dry-run --json SOURCE...` (or `rawdev
   night`/`rawdev day --shoot SHOOT_DIR FRAME...` for the shoot-layout commands). Read every
   `frame_planned` event: check `tone_ref` (what the tone curve will be fitted to), `nr` (noise
   reduction settings, or `null` if off at that ISO), and `outputs` (destination paths) before
   running for real.
3. Run for real with `--json` (same flags, minus `--dry-run`). Add `--gpu` only if `rawdev gpus
   --json` lists a real adapter (not just llvmpipe/software). If the user is working on the same
   machine, add `--threads N` (about half the CPU's threads) so the desktop stays responsive; the
   output is identical. Two batches may run at once; don't queue them.
4. Parse `frame_done` / `frame_failed` events as they stream. Report each output path from
   `frame_done.outputs[].path`, and surface any `frame_failed.error` to the user rather than
   retrying silently.
5. On exit code 2, read the `error` event (`kind` will be `"refused"` for a safety guard or
   `"usage"` for a bad argument) and explain it to the user in plain terms. Never work around a
   safety refusal by picking a different `--out` path to dodge it — ask the user what they actually
   want instead.

## Per-frame options with a recipe file

For runs that need different treatment per frame (sky-match on some day frames, strong NR on noisy
night frames, etc.), write a `recipe.toml` instead of long comma lists on the command line, matching
the format rawdev's `recipe` module expects:

```toml
look = "rich-day"            # optional; --look on the command line still wins if given

[frames.IMG_0007]             # keyed by file stem, or by the trailing number alone, e.g. [frames.7]
match = true                  # day looks: sky colour matrix fit to the camera's own JPEG
shadow_lift = true            # day looks: gentle linear-light shadow lift
recover_hl = true              # day looks: decode with highlight blend to hold texture in bright areas

[frames.IMG_0012]
strong = true                  # night looks: stronger NR for saturated coloured-LED light
nofringe = true                 # night looks: skip the purple-ring cleanup (real purple/magenta light)
```

Pass it with `--recipe recipe.toml`. A frame gets an option if either the recipe or a matching
command-line list names it — they add, they don't override each other.

## Reading the manifest

To answer "what produced this file": hash the JPEG (`sha256sum`), open
`<outdir>/rawdev-manifest.json`, and find the `outputs[].sha256` that matches inside one of the
`runs[].frames[]` entries. That frame's `input_sha256` and `options`, plus its run's `look` (name and
SHA-256) and `tools` (paths and versions), say exactly what produced it. Byte-identical output is only promised for
the same rawdev version, look hash, and LibRaw/exiftool versions — all recorded in the same entry.

## Rules

- RAW originals and camera cards are read-only. Never pass `--overwrite` unless the user explicitly
  asked for it — the default refusal to clobber an existing file is intentional.
- Never rotate, crop, or level a frame; rawdev doesn't support it and won't add it as a workaround.
- Outputs keep GPS and EXIF from the original by default. Mention that before the user shares a
  developed photo publicly, in case the location should come out first.
- Treat the user's photos as their work: don't rank frames as good/bad, don't suggest which to keep
  or delete, unless they specifically ask for that judgment. A cull list is theirs to make.
- Use generic paths in examples with the user (`~/Photos`, `/run/media/$USER/EOS_DIGITAL`) — don't
  assume a specific card label or folder layout until you've confirmed it on their machine.
