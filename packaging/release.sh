#!/usr/bin/env bash
# Build the x86-64 Linux release tarballs in a Debian 12 container (glibc 2.36, so the binaries run on
# most current distros), with libjpeg-turbo built from source and linked statically. Two builds:
#
#   rawdev-VERSION-x86_64-linux.tar.gz           x86-64-v3 (AVX2; any x86-64 CPU from ~2015 on). As fast
#                                                as a -C target-cpu=native build on a Zen 4.
#   rawdev-VERSION-x86_64-baseline-linux.tar.gz  plain x86-64, for older CPUs (~20% slower NR)
#
#   packaging/release.sh [OUTDIR]        (default: dist/)
#
# Needs podman (or docker: CONTAINER=docker). Each tarball gets a .sha256 file.
set -euo pipefail
cd "$(dirname "$0")/.."
OUT=${1:-dist}
ENGINE=${CONTAINER:-podman}
IMAGE=${IMAGE:-docker.io/library/rust:1-slim-bookworm}
VER=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
mkdir -p "$OUT"
# a clean copy of the tracked tree, so local build products and scratch files can't leak in
SRC=$(mktemp -d)
trap 'rm -rf "$SRC"' EXIT
git archive HEAD | tar -x -C "$SRC"
rm -rf "$SRC/.cargo"   # never ship a target-cpu=native build
"$ENGINE" run --rm -v "$SRC":/src:Z -w /src "$IMAGE" bash -euo pipefail -c '
  apt-get update -qq && apt-get install -y -qq --no-install-recommends cmake nasm pkg-config make g++ >/dev/null
  for cpu in x86-64-v3 x86-64; do
    RUSTFLAGS="-C target-cpu=$cpu" CARGO_TARGET_DIR=target/$cpu \
      cargo build --release --locked --features vendored-jpeg
    strip target/$cpu/release/rawdev
  done
  ldd target/x86-64/release/rawdev
  chmod -R a+rwX target'
for v in "x86-64-v3:x86_64-linux" "x86-64:x86_64-baseline-linux"; do
  cpu=${v%%:*}; NAME=rawdev-$VER-${v#*:}
  STAGE=$(mktemp -d)
  mkdir -p "$STAGE/$NAME/docs" "$STAGE/$NAME/looks" "$STAGE/$NAME/openclaw/rawdev"
  cp "$SRC/target/$cpu/release/rawdev" "$STAGE/$NAME/"
  (cd "$SRC" && cp README.md CHANGELOG.md LICENSE-MIT LICENSE-APACHE NOTICE "$STAGE/$NAME/" && cp docs/JSON.md docs/BUILD-WITH-AI.md "$STAGE/$NAME/docs/" && cp looks/*.toml "$STAGE/$NAME/looks/" && cp packaging/openclaw/rawdev/SKILL.md "$STAGE/$NAME/openclaw/rawdev/")
  tar -C "$STAGE" --owner=0 --group=0 --sort=name -czf "$OUT/$NAME.tar.gz" "$NAME"
  rm -rf "$STAGE"
  (cd "$OUT" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256" && cat "$NAME.tar.gz.sha256")
done
