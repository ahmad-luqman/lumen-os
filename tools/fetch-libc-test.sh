#!/usr/bin/env bash
# Fetch musl's libc-test at a pinned commit into build/deps/libc-test/.
#
#   build/deps/libc-test/         a git checkout, detached at LIBC_TEST_COMMIT
#   build/deps/libc-test/.version the commit, written once the checkout is checked
#
# Pinned by commit id, not by a tarball's sha256: cgit's snapshot tarballs are
# generated on request and aren't byte-stable, while a commit id names its
# whole tree (git checks every object's hash as it fetches, and fsck is on).
# Every run checks that HEAD is the pinned commit and that no tracked file
# differs from it, even when the stamp says it was fetched before (CI restores
# this directory from a cache).
#
# Upstream: libc-test is part of the musl project (README: "libc-test is
# developed as part of the musl project"), hosted at repo.or.cz;
# git.musl-libc.org serves musl and the older libc-testsuite but has no
# libc-test repository. License: MIT (its COPYRIGHT file); it is never
# committed to this repository, only fetched.
#
# To bump: change LIBC_TEST_COMMIT (and LIBC_TEST_DATE, for the reader) and
# run `make libc-test`; re-record what changes in tests/harness/test_libctest.py
# (NOT_BUILT, the tests that don't build, and NATIVE_PASS, the native
# reference run's pass set).
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
DIR="$ROOT/build/deps/libc-test"

LIBC_TEST_URL=https://repo.or.cz/libc-test.git
LIBC_TEST_COMMIT=63e36e934e8f2bedf645303a90b64098e2af6c8f
LIBC_TEST_DATE=2026-11-01 # "regex backref regression test with multibyte i..."

# checked: the checkout is at the pinned commit with every tracked file as it
# is there. Untracked files (.version) don't matter: the build copies the
# commit's tree out with `git archive`, never the working tree.
checked() {
    [[ -d "$DIR/.git" ]] || return 1
    [[ "$(git -C "$DIR" rev-parse --verify --quiet HEAD)" == "$LIBC_TEST_COMMIT" ]] || return 1
    git -C "$DIR" diff --quiet HEAD -- || return 1
}

if [[ -f "$DIR/.version" && "$(cat "$DIR/.version")" == "$LIBC_TEST_COMMIT" ]] && checked; then
    exit 0
fi

echo "fetch-libc-test: cloning libc-test at $LIBC_TEST_COMMIT ($LIBC_TEST_DATE)"
rm -rf "$DIR"
mkdir -p "$(dirname "$DIR")"
git -c transfer.fsckObjects=true clone --quiet --no-checkout "$LIBC_TEST_URL" "$DIR"
if ! git -C "$DIR" -c advice.detachedHead=false checkout --quiet --detach "$LIBC_TEST_COMMIT"; then
    echo "fetch-libc-test: $LIBC_TEST_URL has no commit $LIBC_TEST_COMMIT" >&2
    rm -rf "$DIR"
    exit 1
fi
if ! checked; then
    echo "fetch-libc-test: checkout is not at $LIBC_TEST_COMMIT after cloning" >&2
    rm -rf "$DIR"
    exit 1
fi
echo "$LIBC_TEST_COMMIT" > "$DIR/.version"
