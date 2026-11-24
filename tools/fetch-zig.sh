#!/usr/bin/env bash
# Download the pinned zig release into build/deps/zig/, for C cross-builds only.
#
#   build/deps/zig/zig            the compiler driver (`zig cc -target x86_64-linux-musl`)
#   build/deps/zig/.version       "<version> <host>": refetched when either changes
#
# zig is used only as a C cross-compiler for host-built Linux test binaries
# (the first: musl's libc-test, #345). The kernel, the apps and the Rust test
# binaries keep their rustc + rust-lld build (hard rule 7: Rust stays the only
# kernel toolchain).
#
# Which release, and why: 0.15.2 (2025-10-11, over a year old), the newest
# release whose x86_64-linux-musl libc is musl's C source for everything that
# reaches the kernel. Its lib/libc/musl is musl 1.2.5 (src/internal/version.h),
# the same musl as Alpine 3.21's /lib/ld-musl-x86_64.so.1 (its version string
# is 1.2.5), so test binaries may be linked dynamically against Alpine's
# loader as well as statically. In a static link zig 0.15.2 puts in its own
# code (lib/c/*.zig) for ten pure functions only: strcmp, strncmp, strlen,
# strcasecmp, strncasecmp (and their _l forms), bzero, abs, labs, llabs and
# imaxabs; a dynamic binary takes those from Alpine's musl at run time.
# 0.16.0 (2026-04-13) and 0.17.x replace kernel-facing parts of musl with zig's
# own for musl targets (their lib/c.zig adds malloc, fcntl, sys/file, sys/mman
# and unistd: close, access, chdir, mprotect, madvise, ...; 0.17 adds pthread),
# which would leave a libc-test failure in Lumen ambiguous between Lumen and
# zig's libc. Bump deliberately, in its own PR, and recheck both the musl
# version and lib/c.zig.
#
# One tarball per host, each pinned by sha256: the values ziglang.org's
# download index (download/index.json) publishes for 0.15.2, which this script
# checks every download against. To bump: change ZIG_VERSION and the three
# checksums, run the script on each host.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
DEPS="$ROOT/build/deps"
DIR="$DEPS/zig"

ZIG_VERSION=0.15.2

sha256() {
    if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

case "$(uname -s)-$(uname -m)" in
    Darwin-arm64 | Darwin-aarch64)
        HOST=aarch64-macos
        WANT=3cc2bab367e185cdfb27501c4b30b1b0653c28d9f73df8dc91488e66ece5fa6b
        ;;
    Darwin-x86_64)
        HOST=x86_64-macos
        WANT=375b6909fc1495d16fc2c7db9538f707456bfc3373b14ee83fdd3e22b3d43f7f
        ;;
    Linux-x86_64)
        HOST=x86_64-linux
        WANT=02aa270f183da276e5b5920b1dac44a63f1a49e55050ebde3aecc9eb82f93239
        ;;
    *)
        echo "fetch-zig: no zig pinned for $(uname -s) $(uname -m) (pinned: aarch64-macos, x86_64-macos, x86_64-linux)" >&2
        exit 1
        ;;
esac
STAMP="$ZIG_VERSION $HOST"

if [[ -f "$DIR/.version" && "$(cat "$DIR/.version")" == "$STAMP" && -x "$DIR/zig" ]]; then
    exit 0
fi

echo "fetch-zig: downloading zig $ZIG_VERSION for $HOST"
mkdir -p "$DEPS"
rm -rf "$DIR" && mkdir -p "$DIR"
ARCHIVE="$DEPS/zig.tar.xz"
trap 'rm -f "$ARCHIVE"' EXIT
curl -fsSL --retry 3 -o "$ARCHIVE" "https://ziglang.org/download/${ZIG_VERSION}/zig-${HOST}-${ZIG_VERSION}.tar.xz"
GOT=$(sha256 "$ARCHIVE")
if [[ "$GOT" != "$WANT" ]]; then
    echo "fetch-zig: checksum mismatch for zig $ZIG_VERSION $HOST: expected $WANT, got $GOT" >&2
    rm -rf "$DIR"
    exit 1
fi
tar -xf "$ARCHIVE" -C "$DIR" --strip-components=1
rm -f "$ARCHIVE"
trap - EXIT

# The archive is checksummed; this catches a bad extraction or a binary that
# won't run on this host.
REPORTED=$("$DIR/zig" version)
if [[ "$REPORTED" != "$ZIG_VERSION" ]]; then
    echo "fetch-zig: build/deps/zig/zig reports version '$REPORTED', expected $ZIG_VERSION" >&2
    rm -rf "$DIR"
    exit 1
fi
echo "$STAMP" > "$DIR/.version"
