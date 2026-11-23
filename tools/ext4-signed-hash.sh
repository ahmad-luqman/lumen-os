#!/usr/bin/env bash
# Make an ext4 image say its directories hash names signed (#308):
#
#   tools/ext4-signed-hash.sh DEBUGFS IMAGE
#
# mke2fs sets s_flags' EXT2_FLAGS_UNSIGNED_HASH (0x2) where `char` is unsigned
# (e.g. Linux on aarch64), and Lumen mounts such a volume read-only, because
# ext4plus hashes directory names signed (#308, until #313). If IMAGE has that
# bit, this clears it and sets EXT2_FLAGS_SIGNED_HASH (0x1), keeping s_flags'
# other bits, with DEBUGFS's `ssv flags` (E2FSPROGS_FAKE_TIME, if set, passes
# through to it); an image without it is left as it is. Either way it then
# reads s_flags back from the image's bytes and fails unless the bit is clear.
#
# Only the flag changes. An htree directory already in IMAGE stays ordered by
# the unsigned hash, which `e2fsck -fn` then reports, so this is for images
# whose directories aren't indexed: `mke2fs -d` indexes none (mkrootfs.sh's
# final `e2fsck -fn` fails the build if one ever is).
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

# Sets S_FLAGS from IMAGE's bytes (one at a time: od's words are host-endian).
read_s_flags() {
    local bytes b0 b1 b2 b3
    bytes=$(od -An -tu1 -j "$S_FLAGS_AT" -N4 "$IMAGE")
    read -r b0 b1 b2 b3 <<< "$bytes" || true
    if [[ ! "${b3:-}" =~ ^[0-9]+$ ]]; then
        echo "ext4-signed-hash: can't read s_flags from $IMAGE" >&2
        exit 1
    fi
    S_FLAGS=$(( b0 | b1 << 8 | b2 << 16 | b3 << 24 ))
}

read_s_flags
if (( S_FLAGS & UNSIGNED )); then
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
    echo "ext4-signed-hash: $IMAGE had unsigned directory hashes (s_flags $S_FLAGS); set s_flags to $new (#308)" >&2
fi

read_s_flags
if (( S_FLAGS & UNSIGNED )); then
    echo "ext4-signed-hash: $IMAGE still has unsigned directory hashes (s_flags $S_FLAGS)" >&2
    exit 1
fi
