#!/usr/bin/env bash
# #454: pinned git trees, checked with fsck (as fetch-libc-test.sh does).
# Codeload archives can change bytes without changing a tree; commit IDs name
# the tree itself. Tags are fetched but must resolve to the commits below.
# Worktree-local checkouts avoid writing through a shared build/deps symlink.
# --check verifies cached objects and tracked files without any network access.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
DIR="$ROOT/build/gvisor-sources"
[[ $# == 0 || ( $# == 1 && $1 == --check ) ]] || { echo "usage: $0 [--check]" >&2; exit 2; }
GVISOR_COMMIT=cc5b7474bf134432a4f41a47d37022fc4f05ede6
GTEST_COMMIT=063de7e9578f82b369302001269680b4b1553359 # v1.18.0
ABSEIL_COMMIT=2065f4ded0558c6f89fee67c8e5228feb4eb960e # 20260817.0
# Upstream licenses stay in the archived trees: Apache-2.0 (gVisor/Abseil), BSD (gtest).
checked() {
    local tree=$1 commit=$2
    [[ -d "$tree/.git" ]] &&
        [[ $(git -C "$tree" rev-parse --verify HEAD) == "$commit" ]] &&
        git -C "$tree" diff --quiet HEAD -- &&
        git -C "$tree" fsck --full --no-dangling >/dev/null 2>&1
}
fetch() {
    local name=$1 commit=$2 url=$3 ref=$4 tree="$DIR/$1" tmp
    if checked "$tree" "$commit"; then return; fi
    if [[ ${CHECK_ONLY:-} == 1 ]]; then
        echo "fetch-gvisor-tests: $name missing, changed, or corrupt (expected $commit)" >&2
        return 1
    fi
    tmp=$(mktemp -d "$DIR/$name.tmp.XXXXXX")
    if ! { git init -q "$tmp" &&
        git -C "$tmp" -c transfer.fsckObjects=true -c fetch.fsckObjects=true \
            fetch -q --depth=1 "$url" "$ref" &&
        [[ $(git -C "$tmp" rev-parse 'FETCH_HEAD^{commit}') == "$commit" ]] &&
        git -C "$tmp" -c advice.detachedHead=false checkout -q --detach "$commit" &&
        checked "$tmp" "$commit"; }; then
        echo "fetch-gvisor-tests: $name failed verification at $commit" >&2
        rm -rf "$tmp"
        return 1
    fi
    # Only replace after checking the new tree; failed fetches keep the old one.
    rm -rf "$tree"
    mv "$tmp" "$tree"
}
CHECK_ONLY=
if [[ ${1:-} == --check ]]; then CHECK_ONLY=1; else mkdir -p "$DIR"; fi
fetch gvisor "$GVISOR_COMMIT" https://github.com/google/gvisor.git "$GVISOR_COMMIT"
fetch googletest "$GTEST_COMMIT" https://github.com/google/googletest.git refs/tags/v1.18.0
fetch abseil "$ABSEIL_COMMIT" https://github.com/abseil/abseil-cpp.git refs/tags/20260817.0
