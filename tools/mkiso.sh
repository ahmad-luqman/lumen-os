#!/usr/bin/env bash
# Build a UEFI-bootable ISO with Limine and a kernel.
#
#   tools/mkiso.sh <kernel-elf> <output.iso> [cmdline...]
#
# Example: tools/mkiso.sh build/lumen-kernel build/lumen.iso lumen.log=debug
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
KERNEL=$1
OUT=$2
shift 2
CMDLINE="$*"
LIMINE="$ROOT/build/deps/limine"

if [[ ! -f "$LIMINE/BOOTX64.EFI" ]]; then
    echo "mkiso: Limine not found; run tools/fetch-deps.sh" >&2
    exit 1
fi

STAGE=$(mktemp -d "${TMPDIR:-/tmp}/lumen-iso.XXXXXX")
trap 'rm -rf "$STAGE"' EXIT

mkdir -p "$STAGE/boot/limine" "$STAGE/EFI/BOOT"
cp "$KERNEL" "$STAGE/boot/lumen-kernel"
sed "s|@CMDLINE@|$CMDLINE|" "$ROOT/boot/limine.conf" > "$STAGE/boot/limine/limine.conf"
cp "$LIMINE/limine-uefi-cd.bin" "$STAGE/boot/limine/"
cp "$LIMINE/BOOTX64.EFI" "$STAGE/EFI/BOOT/"

mkdir -p "$(dirname "$OUT")"
xorriso -as mkisofs -R -r -J \
    --efi-boot boot/limine/limine-uefi-cd.bin \
    -efi-boot-part --efi-boot-image --protective-msdos-label \
    "$STAGE" -o "$OUT" 2>/dev/null
