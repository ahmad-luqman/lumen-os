#!/usr/bin/env bash
# The durable mirror of the Alpine developer image's packages: every .apk
# tools/alpine-packages.lock has ever pinned, as an asset of this repository's
# `alpine-apk-mirror` GitHub release, named <sha256>.apk. dl-cdn keeps only a
# package's newest -rN build, so a locked file can vanish from it; the fetch
# (tools/fetch-alpine-packages.sh) falls back to this release after dl-cdn.
#
#   tools/mirror-alpine-packages.sh check    every locked sha256 is an asset (CI runs this)
#   tools/mirror-alpine-packages.sh upload   upload the missing ones (the lead, after a lock change)
#
# `check` reads only the release's asset names, never the files. `upload`
# takes the files from build/deps/alpine-packages/ (`make deps-dev` first),
# checks each against the lock's sha256, then uploads each one the release
# lacks; it creates the release if there is none. Assets are never replaced
# or deleted: a name is its content's hash, and an older lock's files stay
# fetchable.
#
# LUMEN_GH runs instead of `gh` (the harness's tests: a stub); LUMEN_PYTHON
# runs tools/alpine-lock.py instead of the project's uv environment, as in
# tools/fetch-alpine-packages.sh.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
LOCK="$ROOT/tools/alpine-packages.lock"
DIR="$ROOT/build/deps/alpine-packages"
# tools/fetch-alpine-packages.sh's DURABLE_MIRROR names this release; the
# harness checks the two agree.
REPO=ahmad-luqman/lumen-os
TAG=alpine-apk-mirror
GH=${LUMEN_GH:-gh}

usage() {
    echo "usage: tools/mirror-alpine-packages.sh check|upload" >&2
    exit 2
}

sha256() {
    if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

alpine_lock() {
    if [[ -n "${LUMEN_PYTHON:-}" ]]; then
        "$LUMEN_PYTHON" "$ROOT/tools/alpine-lock.py" "$@"
    else
        uv run --frozen --project "$ROOT" python "$ROOT/tools/alpine-lock.py" "$@"
    fi
}

# The release's asset names, one per line, or a failure saying why.
assets() {
    local out
    if ! out=$("$GH" release view "$TAG" --repo "$REPO" --json assets --jq '.assets[].name' 2>&1); then
        echo "mirror-alpine-packages: can't list release $TAG of $REPO: $out" >&2
        return 1
    fi
    printf '%s\n' "$out"
}

[[ $# -eq 1 ]] || usage
mode=$1
[[ "$mode" == check || "$mode" == upload ]] || usage

FILES=$(alpine_lock files --lock "$LOCK")

if [[ "$mode" == upload ]] && ! "$GH" release view "$TAG" --repo "$REPO" --json name >/dev/null 2>&1; then
    "$GH" release create "$TAG" --repo "$REPO" --latest=false --title "Alpine packages for the developer image" --notes \
"The Alpine Linux v3.21 x86_64 binary packages that tools/alpine-packages.lock pins, unmodified, each named by its sha256 (\`<sha256>.apk\`), for tools/fetch-alpine-packages.sh to fall back to once dl-cdn drops an older build. Kept by tools/mirror-alpine-packages.sh; never edited by hand.

Each package's licence is in its .PKGINFO. The corresponding sources are Alpine's: the APKBUILDs in https://gitlab.alpinelinux.org/alpine/aports (branch 3.21-stable) and their source archives on https://distfiles.alpinelinux.org/distfiles/v3.21/."
fi

HAVE=" $(assets | tr '\n' ' ') "

missing=0
uploaded=0
STAGE=""
trap 'if [[ -n "$STAGE" ]]; then rm -rf "$STAGE"; fi' EXIT
while read -r _path file want; do
    if [[ -z "$file" || -z "$want" ]]; then
        echo "mirror-alpine-packages: alpine-lock.py files gave a short line: '$_path $file $want'" >&2
        exit 1
    fi
    [[ "$HAVE" == *" $want.apk "* ]] && continue
    if [[ "$mode" == check ]]; then
        echo "mirror-alpine-packages: $file (sha256 $want) isn't in release $TAG" >&2
        missing=$((missing + 1))
        continue
    fi
    if [[ ! -f "$DIR/$file" ]]; then
        echo "mirror-alpine-packages: $DIR/$file is missing: run \`make deps-dev\` first" >&2
        exit 1
    fi
    got=$(sha256 "$DIR/$file")
    if [[ "$got" != "$want" ]]; then
        echo "mirror-alpine-packages: $DIR/$file has sha256 $got, the lock says $want: run \`make deps-dev\` first" >&2
        exit 1
    fi
    [[ -n "$STAGE" ]] || STAGE=$(mktemp -d)
    cp "$DIR/$file" "$STAGE/$want.apk"
    "$GH" release upload "$TAG" --repo "$REPO" "$STAGE/$want.apk#$file"
    rm -f "$STAGE/$want.apk"
    uploaded=$((uploaded + 1))
done <<< "$FILES"

if [[ "$mode" == check ]]; then
    if [[ $missing -ne 0 ]]; then
        echo "mirror-alpine-packages: $missing locked package(s) not mirrored: run \`make deps-dev\`, then \`tools/mirror-alpine-packages.sh upload\`" >&2
        exit 1
    fi
    echo "mirror-alpine-packages: release $TAG holds all $(grep -c . <<< "$FILES") locked packages"
else
    echo "mirror-alpine-packages: $uploaded uploaded to release $TAG"
fi
