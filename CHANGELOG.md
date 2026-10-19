# Changelog

## 0.1.0 (unreleased)

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
