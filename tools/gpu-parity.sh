#!/usr/bin/env bash
# GPU parity check: develop the same RAWs on the CPU and on each Vulkan adapter, then `cmp` every output
# against the CPU's. Bit-exact means every JPEG is byte-identical.
#
#   tools/gpu-parity.sh OUTDIR LOOK RAW_OR_DIR...        (adapters: every one `rawdev gpus` lists)
#   ADAPTERS="2060,radeon" tools/gpu-parity.sh ...       (only these, matched by name substring)
#   RAWDEV=./target/release/rawdev                       (which binary; default rawdev on PATH)
set -euo pipefail
OUT=$1; LOOK=$2; shift 2
RAWDEV=${RAWDEV:-rawdev}
if [ -n "${ADAPTERS:-}" ]; then IFS=, read -ra ADS <<<"$ADAPTERS"
else mapfile -t ADS < <("$RAWDEV" gpus --json | python3 -c 'import sys,json;[print(json.loads(l)["name"]) for l in sys.stdin]'); fi
run() { # name, extra args...
  local name=$1; shift
  local t0=$(date +%s.%N)
  "$RAWDEV" develop --json --look "$LOOK" --out "$OUT/$name" "$@" >"$OUT/$name.jsonl" 2>"$OUT/$name.log"
  local t1=$(date +%s.%N)
  python3 - "$OUT/$name.jsonl" "$name" "$t0" "$t1" <<'PY'
import json,sys
ev=[json.loads(l) for l in open(sys.argv[1])]
s=next(e for e in ev if e["event"]=="batch_start"); end=next(e for e in ev if e["event"]=="batch_end")
nr=[e["timings"].get("noise reduction") for e in ev if e["event"]=="frame_done" and e["timings"].get("noise reduction")]
print(f'{sys.argv[2]:<44} gpu={s.get("gpu")!s:<40} frames={end["done"]}/{end["frames"]} wall={float(sys.argv[4])-float(sys.argv[3]):.1f}s '
      f'NR total={sum(nr):.1f}s mean={sum(nr)/max(len(nr),1):.2f}s/frame')
PY
}
mkdir -p "$OUT"
run cpu "$@"
for a in "${ADS[@]}"; do
  tag=$(echo "$a" | tr -c 'A-Za-z0-9\n' '_' | cut -c1-40)
  run "$tag" --gpu-adapter "$a" "$@"
  same=0; diff=0
  for f in "$OUT"/cpu/*.jpg "$OUT"/cpu/share/*.jpg; do
    g=${f/\/cpu\//\/$tag\/}
    if cmp -s "$f" "$g"; then same=$((same+1)); else diff=$((diff+1)); echo "  DIFF $g"; fi
  done
  echo "  $tag vs cpu: $same identical, $diff different"
done
