#!/usr/bin/env bash
# Assemble the deterministic Alpine developer image into build/alpine-dev.ext4 (#344).
#
#   tools/mkrootfs-dev.sh [output.ext4]
#
# The minirootfs (build/deps/alpine-minirootfs/, `make deps`) plus the data of
# every package in tools/alpine-packages.lock (build/deps/alpine-packages/,
# `make deps-dev`: gcc, g++, make, musl-dev, binutils, git, sqlite, vim and
# their closure), plus build/linux/* as tools/mkrootfs.sh copies them. M8's
# gate scenarios boot on it.
#
# Duplication: the e2fsprogs discovery, the build/linux/* copy and its ELF
# checks, the mtime normalization, the mke2fs flags, the signed-hash step, the
# inode normalization and the final e2fsck below are tools/mkrootfs.sh's,
# copied rather than shared because M7 still used that script when #344 was
# written. Sharing them is a follow-up; a change to one belongs in both.
# What differs: the packages' data, the size (worked out from the content
# rather than a fixed 64M), the label and the UUID.
#
# Install scripts: none is ever run. tools/alpine-lock.py's `stage` checks
# every .apk's sha256, then extracts only each one's data segment, and refuses
# a package whose control segment carries a script its HANDLED_SCRIPTS table
# doesn't list with how this script reproduces it. At the lock of #344 none of
# the 34 carries one: no .pre-install, .post-install or trigger. The one script
# the packages set off is the minirootfs' busybox trigger (on /bin, /usr/bin,
# /sbin, /usr/sbin), which runs `busybox --install -s`: links for applets
# with nothing at their path. The minirootfs already has every link, so it
# would add none; a package's file replaces an applet link no package owns,
# as apk does (strings from binutils, xxd), and the trigger leaves a replaced
# link replaced. The staged tree was checked against `apk add` of the same
# files in docker.io/library/alpine:3.21 (by hand, not in CI; see #344's PR).
#
# Determinism: as tools/mkrootfs.sh: a fixed epoch for every time, a fixed
# UUID and hash seed, the #248-pinned mke2fs -O mask, owners 0:0, and the
# size and inode count computed from the staged content the same way on every
# host (tools/alpine-lock.py `size`), then e2fsck -fn.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
OUT=${1:-$ROOT/build/alpine-dev.ext4}
MINIROOTFS="$ROOT/build/deps/alpine-minirootfs"
APK_DIR="$ROOT/build/deps/alpine-packages"
LOCK="$ROOT/tools/alpine-packages.lock"
LINUX_DIR="$ROOT/build/linux"

FIXED_EPOCH=1700000000
FIXED_TOUCH_TS=202311142213.20
# mkrootfs.sh's UUID with "M8" (4d38) for "M7" (4d37).
FIXED_UUID=4c756d65-6e2d-4d38-8000-000000000001

if [[ ! -f "$MINIROOTFS/.version" || ! -e "$MINIROOTFS/lib/ld-musl-x86_64.so.1" || ! -x "$MINIROOTFS/bin/busybox" ]]; then
    echo "mkrootfs-dev: Alpine minirootfs not found in $MINIROOTFS; run \`make deps\` (or tools/fetch-deps.sh)" >&2
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
        echo "mkrootfs-dev: LUMEN_E2FSPROGS_SBIN=$dir must contain executable mke2fs, e2fsck, and debugfs" >&2
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
    echo "mkrootfs-dev: could not find matched (mke2fs, e2fsck, debugfs) in the same directory." >&2
    echo "  macOS: brew install e2fsprogs (keg-only under /opt/homebrew/opt/e2fsprogs/sbin)" >&2
    echo "  Ubuntu/Debian: sudo apt-get install e2fsprogs" >&2
    echo "  Or set LUMEN_E2FSPROGS_SBIN=/path/to/sbin" >&2
    return 1
}

SBIN=$(find_e2fsprogs_sbin)
MKE2FS="$SBIN/mke2fs"
E2FSCK="$SBIN/e2fsck"
DEBUGFS="$SBIN/debugfs"

# LUMEN_PYTHON, if set, runs tools/alpine-lock.py instead of the project's
# uv environment (the harness's tests, on a stub tree with no uv project).
alpine_lock() {
    if [[ -n "${LUMEN_PYTHON:-}" ]]; then
        "$LUMEN_PYTHON" "$ROOT/tools/alpine-lock.py" "$@"
    else
        uv run --frozen --project "$ROOT" python "$ROOT/tools/alpine-lock.py" "$@"
    fi
}

STAGE=$(mktemp -d "${TMPDIR:-/tmp}/lumen-rootfs-dev.XXXXXX")
DBG_DIR=$(mktemp -d "${TMPDIR:-/tmp}/lumen-rootfs-dev-dbg.XXXXXX")
mkdir -p "$(dirname "$OUT")"
TMP_IMG="${OUT}.tmp.$$"
# The stage may hold read-only directories (from a package's modes): make
# them writable before removing it.
trap 'chmod -R u+w "$STAGE" 2>/dev/null; rm -rf "$STAGE" "$DBG_DIR" "$TMP_IMG"' EXIT

COPYFILE_DISABLE=1 cp -a "$MINIROOTFS/." "$STAGE/"
rm -f "$STAGE/.version"
rm -rf "${STAGE:?}/dev" "$STAGE/proc" "$STAGE/tmp"
mkdir -p "$STAGE/dev" "$STAGE/proc" "$STAGE/tmp"

alpine_lock stage --lock "$LOCK" --apk-dir "$APK_DIR" --minirootfs "$MINIROOTFS" "$STAGE" \
    | sed 's/^/mkrootfs-dev: /'
# After the packages, so none can change them.
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
    echo "mkrootfs-dev: warning: $LINUX_DIR not found; building the image without build/linux/* binaries" >&2
fi

if [[ -f "$STAGE/usr/local/lib/libm7.so" ]]; then
    PYTHONPATH="$ROOT/tools/harness" uv run --frozen --project "$ROOT" \
        python -m lumen_harness.rootfs "$STAGE/usr/local/lib/libm7.so" --rootfs-dir "$STAGE" --no-interp
fi
for bin in m7_test m7_vfs m8_proc m8_io m8_vm ptyrun m8_pty; do
    if [[ -f "$STAGE/usr/local/bin/$bin" ]]; then
        PYTHONPATH="$ROOT/tools/harness" uv run --frozen --project "$ROOT" \
            python -m lumen_harness.rootfs "$STAGE/usr/local/bin/$bin" --rootfs-dir "$STAGE"
    fi
done

# The size: the content, 64 MiB free and a quarter more inodes (at least
# 4096) for what the gates write (tools/alpine-lock.py's image_size).
SIZE_MIB='' INODES='' CONTENT='' INODES_USED=''
SIZES=$(alpine_lock size "$STAGE")
while IFS='=' read -r key value; do
    if [[ ! "$value" =~ ^[0-9]+$ ]]; then
        echo "mkrootfs-dev: alpine-lock.py size gave $key=$value" >&2
        exit 1
    fi
    case $key in
    size_mib) SIZE_MIB=$value ;;
    inodes) INODES=$value ;;
    content) CONTENT=$value ;;
    inodes_used) INODES_USED=$value ;;
    esac
done <<< "$SIZES"
if [[ -z "$SIZE_MIB" || -z "$INODES" || -z "$CONTENT" || -z "$INODES_USED" ]]; then
    echo "mkrootfs-dev: alpine-lock.py size left out a value: $SIZES" >&2
    exit 1
fi
echo "mkrootfs-dev: content $((CONTENT / 1048576)) MiB in $INODES_USED inodes; image ${SIZE_MIB} MiB with $INODES inodes"

# Normalize staged entry mtimes bottom-up (E2FSPROGS_FAKE_TIME does not rewrite `-d` mtimes).
find "$STAGE" -depth -print0 | xargs -0 env TZ=UTC0 touch -h -t "$FIXED_TOUCH_TS"

COPYFILE_DISABLE=1 E2FSPROGS_FAKE_TIME="$FIXED_EPOCH" "$MKE2FS" -q -F \
    -t ext4 -b 4096 -I 256 -N "$INODES" \
    -O '^orphan_file,^metadata_csum_seed,^metadata_csum,^has_journal' \
    -m 0 -L lumen-dev -U "$FIXED_UUID" \
    -E "hash_seed=${FIXED_UUID},root_owner=0:0,lazy_itable_init=0,no_copy_xattrs" \
    -d "$STAGE" "$TMP_IMG" "${SIZE_MIB}M"

# As tools/mkrootfs.sh: s_flags said signed on every host (#308).
E2FSPROGS_FAKE_TIME="$FIXED_EPOCH" "$ROOT/tools/ext4-signed-hash.sh" "$DEBUGFS" "$TMP_IMG"

# As tools/mkrootfs.sh: uid/gid 0:0, ctime and atime the fixed epoch, on every
# allocated inode (`mke2fs -d` copies the host's owners and ctimes).
LC_ALL=C "$DEBUGFS" -R "stats" "$TMP_IMG" > "$DBG_DIR/stats.out" 2> "$DBG_DIR/stats.err"
if grep -qvE '^(debugfs [0-9]|$)' "$DBG_DIR/stats.err"; then
    cat "$DBG_DIR/stats.err" >&2
    exit 1
fi
INODE_COUNT=$(LC_ALL=C awk '/^Inode count:/ { print $3; exit }' "$DBG_DIR/stats.out")
FIRST_INODE=$(LC_ALL=C awk '/^First inode:/ { print $3; exit }' "$DBG_DIR/stats.out")
FIRST_INODE=${FIRST_INODE:-11}
if [[ -z "$INODE_COUNT" || ! "$INODE_COUNT" =~ ^[0-9]+$ || ! "$FIRST_INODE" =~ ^[0-9]+$ ]]; then
    echo "mkrootfs-dev: failed to parse inode bounds from debugfs stats" >&2
    exit 1
fi

LC_ALL=C awk -v first="$FIRST_INODE" -v n="$INODE_COUNT" \
    'BEGIN { for (i = first; i <= n; i++) printf "testi <%d>\n", i }' > "$DBG_DIR/testi.cmd"
LC_ALL=C "$DEBUGFS" -f "$DBG_DIR/testi.cmd" "$TMP_IMG" > "$DBG_DIR/testi.out" 2> "$DBG_DIR/testi.err"
if grep -qvE '^(debugfs [0-9]|$)' "$DBG_DIR/testi.err"; then
    cat "$DBG_DIR/testi.err" >&2
    exit 1
fi

LC_ALL=C awk -v epoch="$FIXED_EPOCH" '
    BEGIN {
        printf "sif <2> uid 0\nsif <2> gid 0\nsif <2> ctime @%s\nsif <2> ctime_extra 0\nsif <2> atime @%s\nsif <2> atime_extra 0\n",
            epoch, epoch
    }
    /^Inode [0-9]+ is marked in use$/ {
        ino = $2
        printf "sif <%s> uid 0\nsif <%s> gid 0\nsif <%s> ctime @%s\nsif <%s> ctime_extra 0\nsif <%s> atime @%s\nsif <%s> atime_extra 0\n",
            ino, ino, ino, epoch, ino, ino, epoch, ino
    }
' "$DBG_DIR/testi.out" > "$DBG_DIR/sif.cmd"

if [[ ! -s "$DBG_DIR/sif.cmd" ]]; then
    echo "mkrootfs-dev: debugfs produced empty inode normalization script" >&2
    exit 1
fi

LC_ALL=C E2FSPROGS_FAKE_TIME="$FIXED_EPOCH" "$DEBUGFS" -w -f "$DBG_DIR/sif.cmd" "$TMP_IMG" \
    >/dev/null 2> "$DBG_DIR/sif.err"
if grep -qvE '^(debugfs [0-9]|$)' "$DBG_DIR/sif.err"; then
    cat "$DBG_DIR/sif.err" >&2
    exit 1
fi

if ! "$E2FSCK" -fn "$TMP_IMG" >/dev/null; then
    echo "mkrootfs-dev: e2fsck -fn failed on the image (one cause: an indexed directory, left in unsigned-hash order by ext4-signed-hash.sh's switch, holding a name with a byte of 0x80 or above: #308)" >&2
    exit 1
fi
mv -f "$TMP_IMG" "$OUT"
echo "mkrootfs-dev: wrote $OUT"
