#!/usr/bin/env bash
# Assemble a deterministic Alpine minirootfs ext4 image into build/alpine-rootfs.ext4.
#
#   tools/mkrootfs.sh [output.ext4]
#
# Stages build/deps/alpine-minirootfs/ plus build/linux/* (/usr/local/bin and
# /usr/local/lib), normalizes timestamps, builds the ext4 filesystem with the
# exact #248-pinned mke2fs -O mask, and verifies it with e2fsck -fn.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
OUT=${1:-$ROOT/build/alpine-rootfs.ext4}
MINIROOTFS="$ROOT/build/deps/alpine-minirootfs"
LINUX_DIR="$ROOT/build/linux"
ROOTFS_SIZE=${ROOTFS_SIZE:-64M}

FIXED_EPOCH=1700000000
FIXED_TOUCH_TS=202311142213.20
FIXED_UUID=4c756d65-6e2d-4d37-8000-000000000001

if [[ ! -f "$MINIROOTFS/.version" || ! -e "$MINIROOTFS/lib/ld-musl-x86_64.so.1" || ! -x "$MINIROOTFS/bin/busybox" ]]; then
    echo "mkrootfs: Alpine minirootfs not found in $MINIROOTFS; run \`make deps\` (or tools/fetch-deps.sh)" >&2
    exit 1
fi

find_e2fsprogs_sbin() {
    local dir
    if [[ -n "${LUMEN_E2FSPROGS_SBIN:-}" ]]; then
        dir="$LUMEN_E2FSPROGS_SBIN"
        if [[ -x "$dir/mke2fs" && -x "$dir/e2fsck" && -x "$dir/debugfs" ]]; then
            printf '%s\n' "$dir"
            return 0
        fi
        echo "mkrootfs: LUMEN_E2FSPROGS_SBIN=$dir must contain executable mke2fs, e2fsck, and debugfs" >&2
        return 1
    fi
    for dir in /opt/homebrew/opt/e2fsprogs/sbin /usr/local/opt/e2fsprogs/sbin; do
        if [[ -x "$dir/mke2fs" && -x "$dir/e2fsck" && -x "$dir/debugfs" ]]; then
            printf '%s\n' "$dir"
            return 0
        fi
    done
    local -a path_dirs=()
    IFS=: read -ra path_dirs <<< "${PATH:-}"
    for dir in ${path_dirs[@]+"${path_dirs[@]}"}; do
        [[ -n "$dir" ]] || continue
        if [[ -x "$dir/mke2fs" && -x "$dir/e2fsck" && -x "$dir/debugfs" ]]; then
            printf '%s\n' "$dir"
            return 0
        fi
    done
    echo "mkrootfs: could not find matched (mke2fs, e2fsck, debugfs) in the same directory." >&2
    echo "  macOS: brew install e2fsprogs (keg-only under /opt/homebrew/opt/e2fsprogs/sbin)" >&2
    echo "  Ubuntu/Debian: sudo apt-get install e2fsprogs" >&2
    echo "  Or set LUMEN_E2FSPROGS_SBIN=/path/to/sbin" >&2
    return 1
}

SBIN=$(find_e2fsprogs_sbin)
MKE2FS="$SBIN/mke2fs"
E2FSCK="$SBIN/e2fsck"
DEBUGFS="$SBIN/debugfs"

STAGE=$(mktemp -d "${TMPDIR:-/tmp}/lumen-rootfs.XXXXXX")
DBG_DIR=$(mktemp -d "${TMPDIR:-/tmp}/lumen-rootfs-dbg.XXXXXX")
mkdir -p "$(dirname "$OUT")"
TMP_IMG="${OUT}.tmp.$$"
trap 'rm -rf "$STAGE" "$DBG_DIR" "$TMP_IMG"' EXIT

COPYFILE_DISABLE=1 cp -a "$MINIROOTFS/." "$STAGE/"
rm -f "$STAGE/.version"
rm -rf "$STAGE/dev" "$STAGE/proc" "$STAGE/tmp"
mkdir -p "$STAGE/dev" "$STAGE/proc" "$STAGE/tmp"
chmod 0755 "$STAGE/dev"
chmod 0555 "$STAGE/proc"
chmod 1777 "$STAGE/tmp"

if [[ -d "$LINUX_DIR" ]]; then
    mkdir -p "$STAGE/usr/local/bin" "$STAGE/usr/local/lib"
    for f in "$LINUX_DIR"/*; do
        [[ -f "$f" ]] || continue
        base=$(basename "$f")
        if [[ "$base" == *.so || "$base" == *.so.* ]]; then
            cp "$f" "$STAGE/usr/local/lib/$base"
            chmod 0755 "$STAGE/usr/local/lib/$base"
        else
            cp "$f" "$STAGE/usr/local/bin/$base"
            chmod 0755 "$STAGE/usr/local/bin/$base"
        fi
    done
else
    echo "mkrootfs: warning: $LINUX_DIR not found; building rootfs without build/linux/* binaries" >&2
fi

if [[ -f "$STAGE/usr/local/lib/libm7.so" ]]; then
    PYTHONPATH="$ROOT/tools/harness" uv run --frozen --project "$ROOT" \
        python -m lumen_harness.rootfs "$STAGE/usr/local/lib/libm7.so" --rootfs-dir "$STAGE" --no-interp
fi
if [[ -f "$STAGE/usr/local/bin/m7_test" ]]; then
    PYTHONPATH="$ROOT/tools/harness" uv run --frozen --project "$ROOT" \
        python -m lumen_harness.rootfs "$STAGE/usr/local/bin/m7_test" --rootfs-dir "$STAGE"
fi

# Normalize staged entry mtimes bottom-up (E2FSPROGS_FAKE_TIME does not rewrite `-d` mtimes).
find "$STAGE" -depth -print0 | xargs -0 env TZ=UTC0 touch -h -t "$FIXED_TOUCH_TS"

COPYFILE_DISABLE=1 E2FSPROGS_FAKE_TIME="$FIXED_EPOCH" "$MKE2FS" -q -F \
    -t ext4 -b 4096 -I 256 \
    -O '^orphan_file,^metadata_csum_seed,^metadata_csum,^has_journal' \
    -m 0 -L lumen-rootfs -U "$FIXED_UUID" \
    -E "hash_seed=${FIXED_UUID},root_owner=0:0,lazy_itable_init=0,no_copy_xattrs" \
    -d "$STAGE" "$TMP_IMG" "$ROOTFS_SIZE"

# Normalize uid/gid (0:0) and i_ctime/ctime_extra on every allocated inode:
# `mke2fs -d` copies host st_uid/st_gid and wall-clock st_ctime (updated by touch)
# even when E2FSPROGS_FAKE_TIME is set, and `-E root_owner=0:0` only sets `/` and `lost+found`.
# Keep E2FSPROGS_FAKE_TIME="$FIXED_EPOCH" on `debugfs -w` so superblock wtime is preserved.
"$DEBUGFS" -R "stats" "$TMP_IMG" > "$DBG_DIR/stats.out" 2> "$DBG_DIR/stats.err"
if grep -qvE '^(debugfs [0-9]|$)' "$DBG_DIR/stats.err"; then
    cat "$DBG_DIR/stats.err" >&2
    exit 1
fi
INODE_COUNT=$(awk '/^Inode count:/ { print $3; exit }' "$DBG_DIR/stats.out")
FIRST_INODE=$(awk '/^First inode:/ { print $3; exit }' "$DBG_DIR/stats.out")
FIRST_INODE=${FIRST_INODE:-11}
if [[ -z "$INODE_COUNT" || ! "$INODE_COUNT" =~ ^[0-9]+$ || ! "$FIRST_INODE" =~ ^[0-9]+$ ]]; then
    echo "mkrootfs: failed to parse inode bounds from debugfs stats" >&2
    exit 1
fi

awk -v first="$FIRST_INODE" -v n="$INODE_COUNT" \
    'BEGIN { for (i = first; i <= n; i++) printf "testi <%d>\n", i }' > "$DBG_DIR/testi.cmd"
"$DEBUGFS" -f "$DBG_DIR/testi.cmd" "$TMP_IMG" > "$DBG_DIR/testi.out" 2> "$DBG_DIR/testi.err"
if grep -qvE '^(debugfs [0-9]|$)' "$DBG_DIR/testi.err"; then
    cat "$DBG_DIR/testi.err" >&2
    exit 1
fi

awk -v epoch="$FIXED_EPOCH" '
    BEGIN {
        printf "sif <2> uid 0\nsif <2> gid 0\nsif <2> ctime @%s\nsif <2> ctime_extra 0\n", epoch
    }
    /^Inode [0-9]+ is marked in use$/ {
        ino = $2
        printf "sif <%s> uid 0\nsif <%s> gid 0\nsif <%s> ctime @%s\nsif <%s> ctime_extra 0\n",
            ino, ino, ino, epoch, ino
    }
' "$DBG_DIR/testi.out" > "$DBG_DIR/sif.cmd"

if [[ ! -s "$DBG_DIR/sif.cmd" ]]; then
    echo "mkrootfs: debugfs produced empty inode normalization script" >&2
    exit 1
fi

E2FSPROGS_FAKE_TIME="$FIXED_EPOCH" "$DEBUGFS" -w -f "$DBG_DIR/sif.cmd" "$TMP_IMG" \
    >/dev/null 2> "$DBG_DIR/sif.err"
if grep -qvE '^(debugfs [0-9]|$)' "$DBG_DIR/sif.err"; then
    cat "$DBG_DIR/sif.err" >&2
    exit 1
fi

"$E2FSCK" -fn "$TMP_IMG" >/dev/null
mv -f "$TMP_IMG" "$OUT"
