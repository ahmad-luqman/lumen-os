#!/usr/bin/env bash
# Download pinned, checksummed boot dependencies into build/deps/.
#
#   build/deps/limine/   Limine bootloader binaries (BOOTX64.EFI, limine-uefi-cd.bin, ...)
#   build/deps/ovmf/     OVMF UEFI firmware for QEMU (code.fd, vars.fd)
#
# Both are pinned by version and sha256 so that macOS and CI boot the exact
# same firmware and bootloader (golden screenshots depend on it). To bump a
# version, change it here, run the script, and update the checksum it prints.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
DEPS="$ROOT/build/deps"

LIMINE_VERSION=12.9.4
LIMINE_URL="https://github.com/limine-bootloader/limine/releases/download/v${LIMINE_VERSION}/limine-binary.tar.gz"
LIMINE_SHA256=847334bc4bc142de996fbd132806b89ed73ac9ab34dbc1e90cfebf1fcc68c2b1

OVMF_VERSION=edk2-stable202605-r1
OVMF_URL="https://github.com/rust-osdev/ovmf-prebuilt/releases/download/${OVMF_VERSION}/${OVMF_VERSION}-bin.tar.xz"
OVMF_SHA256=8ae4d2d73161cc2335f5675d3b8b6edfa0642301679764a246940488ea3ce20d

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

mkdir -p "$DEPS"
fetch limine "$LIMINE_VERSION" "$LIMINE_URL" "$LIMINE_SHA256" limine.tar.gz
fetch ovmf "$OVMF_VERSION" "$OVMF_URL" "$OVMF_SHA256" ovmf.tar.xz
# The OVMF archive nests firmware per architecture; expose the x86-64 files.
if [[ -d "$DEPS/ovmf/x64" ]]; then
    cp "$DEPS/ovmf/x64/code.fd" "$DEPS/ovmf/OVMF_CODE.fd"
    cp "$DEPS/ovmf/x64/vars.fd" "$DEPS/ovmf/OVMF_VARS.fd"
fi
