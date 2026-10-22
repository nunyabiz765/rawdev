# rawdev JSON contract (schema v1)

Everything an agent or script needs to drive rawdev without parsing human text.

- `--json` prints **one JSON object per line** on stdout. Human-readable progress moves to stderr.
- Every line has `"v": 1` (this schema) and `"event"` (its type). New fields may be added in later
  versions; existing fields keep their meaning while `v` stays 1. Ignore fields you don't know.
- Paths are printed as given on the command line (relative stays relative), except where noted.
- Hashes are lowercase hex SHA-256. Times are seconds (floats, 3 decimals) or RFC 3339 UTC strings.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Everything worked (or the dry run found nothing wrong) |
| 1 | Some frames/files failed (the rest were written), or the manifest couldn't be written |
| 2 | Bad arguments, or a **safety refusal** (nothing was written) |
| 3 | A required tool is missing (`exiftool`, `dcraw_emu`) (nothing was written) |

On codes 2 and 3 (and any error before a batch starts) the last line is an `error` event:

```json
{"v":1,"event":"error","code":2,"kind":"refused","message":"refusing to write into /photos/2026-10-18/raw: that is where the RAWs are. Pick another --out (a subfolder is fine)"}
```

`kind` is one of `usage` (bad arguments), `refused` (a safety guard), `missing_tool`, `error`.

## `rawdev develop | night | day`

A run prints `batch_start`, then one `frame_done` / `frame_failed` per frame (or `frame_planned` with
`--dry-run`), then `batch_end`.

### `batch_start`

```json
{"v":1,"event":"batch_start","rawdev_version":"0.1.2","command":"develop","dry_run":false,
 "look":{"name":"neutral-day","recipe":"day","origin":"builtin","sha256":"…"},
 "out":"developed","frames":9,"gpu":"NVIDIA GeForce RTX 2060 (Vulkan)","threads":12,
 "tools":{"exiftool":{"path":"/usr/bin/vendor_perl/exiftool","version":"13.55"},
          "dcraw_emu":{"path":"/usr/bin/dcraw_emu","version":"LibRaw 0.22.2"}}}
```

- `look.origin`: `builtin` or the look file's path. `look.sha256` hashes the look's TOML text, so two
  runs with the same hash used the same settings.
- `gpu`: `null` without `--gpu`; the adapter name; or `"cpu (no usable GPU)"` when `--gpu` was asked
  for but no GPU could be opened (the run continues on the CPU, with identical output).
- `threads` (0.1.1+): CPU threads the batch may use: the `--threads` value, else every logical CPU (or
  `RAYON_NUM_THREADS`). Output is identical at any value.

### `frame_planned` (dry run only)

```json
{"v":1,"event":"frame_planned","index":0,"input":"card/IMG_0001.CR3","stem":"IMG_0001",
 "make":"Canon","model":"Canon EOS R100","iso":1600,
 "options":{"match_sky":false,"shadow_lift":false,"recover_hl":false},
 "tone_ref":"sidecar","tone_ref_path":"card/IMG_0001.JPG",
 "previews":[{"tag":"PreviewImage","bytes":1534567}],
 "nr":{"h_luma":4.0,"h_color":7.0,"pre_median":false,"chroma_median":3,"luma_blend":0.3,"blend_from_input":true},
 "outputs":[{"kind":"full","path":"developed/IMG_0001.jpg"},{"kind":"share","path":"developed/share/IMG_0001.jpg"}]}
```

- `options`: day looks have `match_sky`, `shadow_lift`, `recover_hl`; night looks have `defringe`,
  `strong_nr`; layered looks (0.1.2+) have none (`{}`), and their `nr` is `null`.
- `tone_ref` (the plan): `sidecar` (a camera JPEG next to the RAW), `preview` (the RAW's embedded
  JPEG, tried biggest first), or `neutral` (the fixed curve). In `frame_done` it becomes what was
  actually used, e.g. `preview:JpgFromRaw`, or `neutral` if no preview passed the size/aspect checks.
- `nr`: the noise-reduction settings for this ISO, or `null` when the look turns NR off at this ISO.

### `frame_done`

```json
{"v":1,"event":"frame_done","index":0,"input":"card/IMG_0001.CR3","input_sha256":"…","stem":"IMG_0001",
 "make":"Canon","model":"Canon EOS R100","iso":1600,"options":{…},
 "tone_ref":"sidecar","tone_ref_path":"card/IMG_0001.JPG",
 "curve":"joint r=0.243->0.769","clip_g":1.0,"nr":{…},"notes":["matrix-fit"],
 "size":[6024,4020],
 "outputs":[{"kind":"full","path":"developed/IMG_0001.jpg","bytes":14318211,"sha256":"…"},
            {"kind":"share","path":"developed/share/IMG_0001.jpg","bytes":1633842,"sha256":"…"}],
 "timings":{"decode":3.08,"camera jpeg":0.08,"tone":0.06,"noise reduction":6.63,"…":0.0},
 "seconds":7.6}
```

- `curve`: how the tone curve was made: `joint r=A->B` (fitted to the reference; the last fitted point
  maps RAW level A to B), `neutral g=2.00 knee=0.60`, or `linear` (the fit had too few usable points).
- `clip_g`: where green clips, relative to the brightest sample (1.0 = no early green clip).
- `notes`: things worth knowing, e.g. `matrix-fit`, `matrix-fit skipped (no usable sky)`,
  `matrix-fit skipped (reference doesn't line up)`, `shadow-lift`.
- `size`: output pixels, width × height, after LibRaw applied the camera's orientation. rawdev never
  rotates, levels or crops beyond that.
- `timings`: seconds per step (step names may change between versions; treat them as informational).

### `frame_failed`

```json
{"v":1,"event":"frame_failed","index":3,"input":"card/IMG_0004.CR3","stem":"IMG_0004","error":"dcraw_emu failed on card/IMG_0004.CR3 (exit status: 1)"}
```

The batch continues with the next frame; the exit code is 1.

### `batch_end`

```json
{"v":1,"event":"batch_end","dry_run":false,"frames":9,"done":9,"failed":0,"seconds":21.1,"manifest":"developed/rawdev-manifest.json"}
```

## The manifest: `<out>/rawdev-manifest.json`

Written after every real (not dry) run; each run appends one entry to `runs`. The file is replaced
atomically (temp file + rename), so it is never half-written.

```json
{
  "rawdev_manifest": 1,
  "runs": [
    {
      "rawdev_version": "0.1.2",
      "command": "develop",
      "argv": ["develop", "--out", "developed", "card"],
      "started": "2026-10-18T19:27:35Z",
      "finished": "2026-10-18T19:27:56Z",
      "look": {"name": "neutral-day", "recipe": "day", "origin": "builtin", "sha256": "…"},
      "tools": {"exiftool": {…}, "dcraw_emu": {…}},
      "gpu": null,
      "frames": [ /* the frame_done objects, without "v"/"event" */ ],
      "failed": [ /* the frame_failed objects */ ]
    }
  ]
}
```

To check later what produced a JPEG: hash it, find the `outputs[].sha256` that matches, and read that
frame's `input_sha256`, look hash, options and tool versions. Byte-identical output is promised for the
same rawdev version, look hash, LibRaw / libjpeg-turbo / exiftool versions and input; the manifest
records all of them.

## `rawdev ingest`

Events: `ingest_start`, then `file_planned` (dry run) or `file_done` per file, then `ingest_end`.

```json
{"v":1,"event":"ingest_start","dry_run":false,"sources":["/run/media/me/EOS_DIGITAL"],"shoot":"photos/2026-10-18-park","files":212,"bytes":6123456789,"unmount":{"mount":"/run/media/me/EOS_DIGITAL","device":"/dev/sdb1"}}
{"v":1,"event":"file_done","src":"/run/media/me/EOS_DIGITAL/DCIM/100CANON/IMG_0001.CR3","dest":"photos/2026-10-18-park/raw/IMG_0001.CR3","bytes":28934112,"sha256":"…","status":"copied"}
{"v":1,"event":"ingest_end","dry_run":false,"copied":211,"already":1,"failed":0,"manifest":"photos/2026-10-18-park/ingest-manifest.json","shoot":"photos/2026-10-18-park","unmounted":"/run/media/me/EOS_DIGITAL"}
```

- `status`: `copied` (copied and verified), `already` (the destination exists with the same SHA-256),
  `failed` (with `error`; e.g. the destination exists with different content: it is left alone).
- `file_planned.status`: `copy` or `exists`.
- `unmounted`: the mount point, or `null` (not asked, or not done because something failed).
- `ingest-manifest.json` (or `-2`, `-3` … for later runs into the same shoot) holds every `file_done`
  record plus the sources and times.

## `rawdev cull`

Events: `cull_start`, then `moved` (or `move_planned` with `--dry-run`) per file, then `cull_end`.

```json
{"v":1,"event":"moved","from":"shoot/raw/IMG_0007.CR3","to":"shoot/_rejects/raw/IMG_0007.CR3","at":"2026-10-18T20:01:12Z"}
{"v":1,"event":"cull_end","dry_run":false,"moved":2,"failed":0,"log":"shoot/_rejects/cull-log.jsonl"}
```

`--undo` emits the same events (with `"undo": true` in `cull_end`) moving files back.

## Other commands

- `rawdev looks --json`: one `look` event per built-in look (`name`, `recipe`, `description`,
  `sha256`); `rawdev looks NAME --json`: one `look` event with the `toml` text.
- `rawdev gpus --json`: one `gpu` event per Vulkan adapter (`name`, `kind`: discrete / integrated /
  virtual / cpu / other, `driver`).
- `rawdev check --json`: one `check` event: `ok`, `tools` (paths + versions, or `error`), `config`
  (path, exists), `protected`, `gpus`. Exit code 3 if a tool is missing.
