#!/usr/bin/env bash
# The M8 syscall census (#337): run the gate workloads on stock Alpine x86-64
# under strace and write tests/linux/m8-census.toml.
#
#   tools/syscall-census.sh
#
# Needs bash, git and Docker on an x86-64 Linux kernel: a Linux machine, or
# Windows with Docker Desktop's WSL2 backend (clone the repo inside the WSL
# filesystem, not under /mnt/c). Not an Apple Silicon Mac: an amd64 container
# there runs under Rosetta, where strace sees the translator's calls, not the
# program's. Not run in CI. Raw traces stay in build/census/.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
# alpine:3.21 (3.21.10 at the pin, the minirootfs' release in tools/fetch-deps.sh).
IMAGE=alpine:3.21@sha256:bc21487101ae47172509130ee1092cc273a831968d64b617776d1f69c81e42ab
OUT="$ROOT/build/census"
DEST="$ROOT/tests/linux/m8-census.toml"

arch=$(docker info --format '{{.Architecture}}')
if [[ "$arch" != "x86_64" ]]; then
    echo "syscall-census: Docker runs on $arch; the census needs an x86-64 Linux kernel" >&2
    echo "  (an amd64 container under Rosetta or qemu-user traces the translator, not the program)" >&2
    exit 1
fi

rm -rf "$OUT"
mkdir -p "$OUT"
echo "$IMAGE" >"$OUT/image"

docker run --rm --platform linux/amd64 \
    --cap-add SYS_PTRACE --security-opt seccomp=unconfined \
    -v "$ROOT/tools/census:/census:ro" \
    -v "$ROOT/tools/harness/lumen_harness:/harness:ro" \
    -v "$OUT:/out" \
    "$IMAGE" sh /census/workloads.sh

mkdir -p "$(dirname "$DEST")"
cp "$OUT/m8-census.toml" "$DEST"
echo "syscall-census: wrote ${DEST#"$ROOT"/}"
