#!/usr/bin/env bash
# The M8 syscall census (#337): run the gate workloads on stock Alpine x86-64
# under strace and write tests/linux/m8-census.toml.
#
#   tools/syscall-census.sh
#
# Needs bash, git and Docker or Podman on an x86-64 Linux kernel: a Linux
# machine, or WSL2 with Docker Desktop's integration or Podman (clone the repo
# inside the WSL filesystem, not under /mnt/c). The engine is docker if it
# runs, else podman; CONTAINER_ENGINE=podman (or docker) picks one. Not an Apple Silicon Mac: an amd64 container
# there runs under Rosetta, where strace sees the translator's calls, not the
# program's. Not run in CI. Raw traces stay in build/census/.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
# alpine:3.21 (3.21.10 at the pin, the minirootfs' release in tools/fetch-deps.sh).
IMAGE=docker.io/library/alpine:3.21@sha256:bc21487101ae47172509130ee1092cc273a831968d64b617776d1f69c81e42ab
OUT="$ROOT/build/census"
DEST="$ROOT/tests/linux/m8-census.toml"

engine=${CONTAINER_ENGINE:-}
if [[ -z "$engine" ]]; then
    if docker info >/dev/null 2>&1; then
        engine=docker
    elif command -v podman >/dev/null 2>&1; then
        engine=podman
    else
        echo "syscall-census: neither docker nor podman runs here" >&2
        exit 1
    fi
fi
case "$engine" in
docker) arch=$(docker info --format '{{.Architecture}}') ;;
podman) arch=$(podman info --format '{{.Host.Arch}}') ;;
*)
    echo "syscall-census: CONTAINER_ENGINE must be docker or podman, not $engine" >&2
    exit 1
    ;;
esac
if [[ "$arch" != "x86_64" && "$arch" != "amd64" ]]; then
    echo "syscall-census: $engine runs on $arch; the census needs an x86-64 Linux kernel" >&2
    echo "  (an amd64 container under Rosetta or qemu-user traces the translator, not the program)" >&2
    exit 1
fi

rm -rf "$OUT"
mkdir -p "$OUT"
echo "$IMAGE" >"$OUT/image"

"$engine" run --rm --platform linux/amd64 \
    --cap-add SYS_PTRACE --security-opt seccomp=unconfined \
    -v "$ROOT/tools/census:/census:ro" \
    -v "$ROOT/tools/harness/lumen_harness:/harness:ro" \
    -v "$OUT:/out" \
    "$IMAGE" sh /census/workloads.sh

mkdir -p "$(dirname "$DEST")"
cp "$OUT/m8-census.toml" "$DEST"
echo "syscall-census: wrote ${DEST#"$ROOT"/}"
