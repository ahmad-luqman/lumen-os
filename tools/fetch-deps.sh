#!/usr/bin/env bash
# Download pinned, checksummed boot and userspace test dependencies into build/deps/.
#
#   build/deps/limine/            Limine bootloader binaries (BOOTX64.EFI, limine-uefi-cd.bin, ...)
#   build/deps/ovmf/              OVMF UEFI firmware for QEMU (code.fd, vars.fd)
#   build/deps/busybox/           Unmodified Alpine static musl busybox (GPL, never committed to repo)
#   build/deps/alpine-minirootfs/ Unmodified Alpine x86-64 minirootfs (never committed to repo)
#
# All dependencies are pinned by version and sha256 so that macOS and CI boot
# the exact same firmware, bootloader, and userspace binaries (golden
# screenshots depend on it). To bump a version, change it here, run the script,
# and update the checksum it prints.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
DEPS="$ROOT/build/deps"

LIMINE_VERSION=12.9.4
LIMINE_URL="https://github.com/limine-bootloader/limine/releases/download/v${LIMINE_VERSION}/limine-binary.tar.gz"
LIMINE_SHA256=847334bc4bc142de996fbd132806b89ed73ac9ab34dbc1e90cfebf1fcc68c2b1

OVMF_VERSION=edk2-stable202605-r1
OVMF_URL="https://github.com/rust-osdev/ovmf-prebuilt/releases/download/${OVMF_VERSION}/${OVMF_VERSION}-bin.tar.xz"
OVMF_SHA256=8ae4d2d73161cc2335f5675d3b8b6edfa0642301679764a246940488ea3ce20d

# Note: Alpine point-release bumps remove older `-rN` .apk URLs from dl-cdn.
# If this URL returns 404 in the future, bump BUSYBOX_VERSION and BUSYBOX_SHA256
# to the current v3.21 (or newer) busybox-static package.
BUSYBOX_VERSION=1.37.0-r14
BUSYBOX_URL="https://dl-cdn.alpinelinux.org/alpine/v3.21/main/x86_64/busybox-static-${BUSYBOX_VERSION}.apk"
BUSYBOX_SHA256=feef45f02f31ee30b6b47eed9b643a0f99e63a21a08a90e7c680066a712405bd

ALPINE_MINIROOTFS_VERSION=3.21.10
ALPINE_MINIROOTFS_URL="https://dl-cdn.alpinelinux.org/alpine/v3.21/releases/x86_64/alpine-minirootfs-${ALPINE_MINIROOTFS_VERSION}-x86_64.tar.gz"
ALPINE_MINIROOTFS_SHA256=d225c786541b5ae5ba006ef541701d9ecc7e732bd16317c08adfb93212345efa

sha256() {
    if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# fetch <name> <version> <url> <sha256> <archive-name>
fetch() {
    local name=$1 version=$2 url=$3 want=$4 archive=$5
    local dir="$DEPS/$name"
    if [[ -f "$dir/.version" && "$(cat "$dir/.version")" == "$version" ]]; then
        return 0
    fi
    echo "fetch-deps: downloading $name $version"
    rm -rf "$dir" && mkdir -p "$dir"
    curl -fsSL --retry 3 -o "$DEPS/$archive" "$url"
    local got
    got=$(sha256 "$DEPS/$archive")
    if [[ "$got" != "$want" ]]; then
        echo "fetch-deps: checksum mismatch for $name: expected $want, got $got" >&2
        rm -f "$DEPS/$archive"
        exit 1
    fi
    tar -xf "$DEPS/$archive" -C "$dir" --strip-components=1
    rm -f "$DEPS/$archive"
    echo "$version" > "$dir/.version"
}

# Alpine .apk packages are concatenated gzip-compressed tar streams (signature,
# metadata, then payload). GNU tar stops at the first tar EOF marker unless
# given --ignore-zeros, whereas BSD tar (macOS) reads concatenated streams
# natively and does not accept --ignore-zeros.
fetch_busybox() {
    local dir="$DEPS/busybox"
    if [[ -f "$dir/.version" && "$(cat "$dir/.version")" == "$BUSYBOX_VERSION" && -f "$dir/busybox" ]]; then
        return 0
    fi
    echo "fetch-deps: downloading busybox $BUSYBOX_VERSION"
    rm -rf "$dir" && mkdir -p "$dir"
    local apk="$DEPS/busybox-static.apk"
    trap 'rm -f "$DEPS/busybox-static.apk"' EXIT
    curl -fsSL --retry 3 -o "$apk" "$BUSYBOX_URL"
    local got
    got=$(sha256 "$apk")
    if [[ "$got" != "$BUSYBOX_SHA256" ]]; then
        echo "fetch-deps: checksum mismatch for busybox: expected $BUSYBOX_SHA256, got $got" >&2
        exit 1
    fi
    if tar --version 2>/dev/null | grep -qi 'GNU tar'; then
        tar --ignore-zeros --warning=no-unknown-keyword -xzf "$apk" -C "$dir" bin/busybox.static
    else
        tar -xzf "$apk" -C "$dir" bin/busybox.static
    fi
    mv "$dir/bin/busybox.static" "$dir/busybox"
    rmdir "$dir/bin"
    chmod +x "$dir/busybox"
    rm -f "$apk"
    trap - EXIT
    if [[ ! -s "$dir/busybox" ]]; then
        echo "fetch-deps: failed to extract busybox binary" >&2
        exit 1
    fi
    echo "$BUSYBOX_VERSION" > "$dir/.version"
}

fetch_alpine_minirootfs() {
    local dir="$DEPS/alpine-minirootfs"
    if [[ -f "$dir/.version" && "$(cat "$dir/.version")" == "$ALPINE_MINIROOTFS_VERSION" \
        && -e "$dir/lib/ld-musl-x86_64.so.1" && -x "$dir/bin/busybox" ]]; then
        return 0
    fi
    echo "fetch-deps: downloading alpine-minirootfs $ALPINE_MINIROOTFS_VERSION"
    rm -rf "$dir" && mkdir -p "$dir"
    local tarball="$DEPS/alpine-minirootfs.tar.gz"
    trap 'rm -f "$DEPS/alpine-minirootfs.tar.gz"' EXIT
    curl -fsSL --retry 3 -o "$tarball" "$ALPINE_MINIROOTFS_URL"
    local got
    got=$(sha256 "$tarball")
    if [[ "$got" != "$ALPINE_MINIROOTFS_SHA256" ]]; then
        echo "fetch-deps: checksum mismatch for alpine-minirootfs: expected $ALPINE_MINIROOTFS_SHA256, got $got" >&2
        exit 1
    fi
    # The tarball has no single top-level directory and includes /dev/* device nodes
    # that fail non-root mknod; exclude dev/* and recreate empty mountpoints below.
    COPYFILE_DISABLE=1 tar --no-same-owner --exclude='dev/*' --exclude='./dev/*' -xzf "$tarball" -C "$dir"
    rm -rf "$dir/dev"
    mkdir -p "$dir/dev" "$dir/proc" "$dir/tmp"
    chmod 0755 "$dir/dev"
    chmod 1777 "$dir/tmp"
    rm -f "$tarball"
    trap - EXIT
    if [[ ! -s "$dir/lib/ld-musl-x86_64.so.1" || ! -x "$dir/bin/busybox" ]]; then
        echo "fetch-deps: failed to extract alpine-minirootfs (missing ld-musl-x86_64.so.1 or bin/busybox)" >&2
        exit 1
    fi
    echo "$ALPINE_MINIROOTFS_VERSION" > "$dir/.version"
}

mkdir -p "$DEPS"
fetch limine "$LIMINE_VERSION" "$LIMINE_URL" "$LIMINE_SHA256" limine.tar.gz
fetch ovmf "$OVMF_VERSION" "$OVMF_URL" "$OVMF_SHA256" ovmf.tar.xz
# The OVMF archive nests firmware per architecture; expose the x86-64 files.
if [[ -d "$DEPS/ovmf/x64" ]]; then
    cp "$DEPS/ovmf/x64/code.fd" "$DEPS/ovmf/OVMF_CODE.fd"
    cp "$DEPS/ovmf/x64/vars.fd" "$DEPS/ovmf/OVMF_VARS.fd"
fi
fetch_busybox
fetch_alpine_minirootfs
