#!/usr/bin/env bash
# Make an ext4 image say its directories hash names signed (#308):
#
#   tools/ext4-signed-hash.sh DEBUGFS IMAGE
#
# mke2fs sets s_flags' EXT2_FLAGS_UNSIGNED_HASH (0x2) where `char` is unsigned
# (e.g. Linux on aarch64), EXT2_FLAGS_SIGNED_HASH (0x1) elsewhere. Lumen
# mounted an unsigned volume read-only until #313 (#308); it still mounts one
# with neither flag (and dir_index) read-only. Set to signed on every host,
# mkrootfs.sh's image comes out the same wherever it is made. If IMAGE has the
# unsigned bit, or neither, this clears the one and sets the signed one,
# keeping s_flags' other bits, with DEBUGFS's `ssv flags` (E2FSPROGS_FAKE_TIME,
# if set, passes through to it); an image already signed is left as it is.
# Either way it then reads s_flags back from the image's bytes and fails
# unless it says signed and not unsigned.
#
# Only the flag changes, and only in the primary superblock: backups keep the
# old flag (mkrootfs.sh's default 64M image with 4 KiB blocks has one block
# group, so none), and the kernel and e2fsck read the primary. An htree
# directory already in IMAGE stays ordered by the unsigned hash, so this is
# for images whose directories aren't indexed: `mke2fs -d` indexes none. If
# one ever is, mkrootfs.sh's final `e2fsck -fn` fails the build once such a
# directory holds a name with a byte of 0x80 or above filed in a leaf its
# signed hash doesn't lead to (an ASCII name hashes alike either way).
set -euo pipefail

if (( $# != 2 )); then
    echo "usage: $0 DEBUGFS IMAGE" >&2
    exit 2
fi
DEBUGFS=$1
IMAGE=$2
# s_flags: a little-endian u32 at 0x160 in the superblock, which starts at 1024.
S_FLAGS_AT=$((1024 + 0x160))
UNSIGNED=0x2
SIGNED=0x1

# The superblock's magic, 0xEF53, little-endian at 0x38 in it.
MAGIC_AT=$((1024 + 0x38))

# Sets S_FLAGS from IMAGE's bytes (one at a time: od's words are host-endian).
read_s_flags() {
    local bytes b0 b1 b2 b3
    bytes=$(od -An -tu1 -j "$S_FLAGS_AT" -N4 "$IMAGE")
    read -r b0 b1 b2 b3 <<< "$bytes"
    if [[ ! "${b3:-}" =~ ^[0-9]+$ ]]; then
        echo "ext4-signed-hash: can't read s_flags from $IMAGE" >&2
        exit 1
    fi
    S_FLAGS=$(( b0 | b1 << 8 | b2 << 16 | b3 << 24 ))
}

if [[ ! -f "$IMAGE" || ! -r "$IMAGE" ]]; then
    echo "ext4-signed-hash: can't read $IMAGE" >&2
    exit 1
fi
magic=$(od -An -tu1 -j "$MAGIC_AT" -N2 "$IMAGE")
if [[ "$(echo $magic)" != "83 239" ]]; then
    echo "ext4-signed-hash: $IMAGE has no ext4 superblock (no magic 0xEF53)" >&2
    exit 1
fi
# Whether S_FLAGS fails to say signed (unsigned, or neither).
not_signed() {
    (( (S_FLAGS & UNSIGNED) || !(S_FLAGS & SIGNED) ))
}

read_s_flags
if not_signed; then
    if (( S_FLAGS & UNSIGNED )); then
        had="unsigned directory hashes"
    else
        had="no directory hash flag"
    fi
    new=$(( (S_FLAGS & ~UNSIGNED) | SIGNED ))
    # debugfs exits 0 when a request fails: any stderr line but its version
    # banner is an error.
    rc=0
    errors=$(LC_ALL=C "$DEBUGFS" -w -R "ssv flags $new" "$IMAGE" 2>&1 >/dev/null) || rc=$?
    if (( rc != 0 )) || grep -qvE '^(debugfs [0-9]|$)' <<< "$errors"; then
        printf '%s\n' "$errors" >&2
        echo "ext4-signed-hash: debugfs ssv flags $new failed (exit $rc) on $IMAGE" >&2
        exit 1
    fi
    echo "ext4-signed-hash: $IMAGE had $had (s_flags $S_FLAGS); set s_flags to $new (#308)" >&2
fi

read_s_flags
if not_signed; then
    echo "ext4-signed-hash: $IMAGE still doesn't say its directories hash signed (s_flags $S_FLAGS)" >&2
    exit 1
fi
