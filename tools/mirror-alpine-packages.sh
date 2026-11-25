#!/usr/bin/env bash
# The durable mirror of the Alpine developer image's packages: every .apk
# tools/alpine-packages.lock has pinned, as an asset of this repository's
# `alpine-apk-mirror` GitHub release (a prerelease), named <sha256>.apk and
# labelled with the package's file name. dl-cdn keeps only a package's newest
# -rN build, so a locked file can vanish from it; the fetch
# (tools/fetch-alpine-packages.sh) falls back to this release after dl-cdn.
#
#   tools/mirror-alpine-packages.sh check    every locked file is an asset (CI runs this)
#   tools/mirror-alpine-packages.sh upload   upload the missing ones (the lead, after a lock change)
#
# An asset counts only when GitHub says its upload finished (`state`
# "uploaded") and its `digest` is the sha256 its name gives. A locked file's
# name there that fails this (a partial upload, other bytes) is an error in
# both modes, which names the `gh release delete-asset` command to run by
# hand. A draft release is an error too (the fetch can't download from one).
#
# `check` reads only the release's asset list (every page), never the files.
# `upload` takes the files from build/deps/alpine-packages/ (`make deps-dev`
# first): it finds the locked files the release lacks, checks every one of
# them against the lock's sha256 before uploading any, creates the release if
# there is none, uploads each from a staged copy whose sha256 it checked, and
# ends with `check`'s pass. The tool's policy is never to replace or delete an
# asset: a name is its content's hash, and an older lock's files stay
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

# Sets RELEASE_ID to the release's id, or to "" when there is none. A draft,
# or any gh failure but "release not found", stops the script.
probe_release() {
    local out rc=0
    out=$("$GH" release view "$TAG" --repo "$REPO" --json databaseId,isDraft \
        --jq '"\(.databaseId) \(.isDraft)"' </dev/null 2>&1) || rc=$?
    RELEASE_ID=""
    if [[ $rc -ne 0 ]]; then
        if [[ "$out" == "release not found" ]]; then
            return 0
        fi
        echo "mirror-alpine-packages: can't read release $TAG of $REPO: $out" >&2
        exit 1
    fi
    if [[ "$out" =~ ^([0-9]+)\ false$ ]]; then
        RELEASE_ID=${BASH_REMATCH[1]}
    elif [[ "$out" =~ ^[0-9]+\ true$ ]]; then
        echo "mirror-alpine-packages: release $TAG of $REPO is a draft, which the fetch can't download from: publish it as a prerelease" >&2
        exit 1
    else
        echo "mirror-alpine-packages: release $TAG of $REPO: gh answered '$out'" >&2
        exit 1
    fi
}

# Sets MISSING to the `path file sha256` lines of the locked files the
# release lacks, and BAD to how many it holds but not as a finished upload of
# the named bytes, naming each of those on stderr. Every asset is read, page
# by page, as a `name<TAB>state<TAB>digest` line.
read_release() {
    local listing="" path file want rest
    MISSING=()
    BAD=0
    if [[ -n "$RELEASE_ID" ]]; then
        if ! listing=$("$GH" api --paginate "repos/$REPO/releases/$RELEASE_ID/assets?per_page=100" \
            --jq '.[] | [.name, .state, (.digest // "")] | @tsv' </dev/null 2>&1); then
            echo "mirror-alpine-packages: can't list the assets of release $TAG of $REPO: $listing" >&2
            exit 1
        fi
    fi
    listing=$'\n'"$listing"$'\n'
    while read -r path file want; do
        if [[ -z "$path" || -z "$file" || -z "$want" ]]; then
            echo "mirror-alpine-packages: alpine-lock.py files gave a short line: '$path $file $want'" >&2
            exit 1
        fi
        if [[ "$listing" == *$'\n'"$want.apk"$'\t'"uploaded"$'\t'"sha256:$want"$'\n'* ]]; then
            continue
        fi
        if [[ "$listing" == *$'\n'"$want.apk"$'\t'* ]]; then
            rest=${listing#*$'\n'"$want.apk"$'\t'}
            rest=${rest%%$'\n'*}
            echo "mirror-alpine-packages: release $TAG holds $want.apk ($file), but its state is '${rest%%$'\t'*}' and its digest '${rest#*$'\t'}', not a finished upload of those bytes: delete it by hand (\`gh release delete-asset $TAG $want.apk --repo $REPO\`), then run \`tools/mirror-alpine-packages.sh upload\`" >&2
            BAD=$((BAD + 1))
            continue
        fi
        MISSING+=("$path $file $want")
    done <<< "$FILES"
}

# `check`'s verdict on what read_release found: 0 when the release holds
# every locked file.
report() {
    local line path file want
    for line in ${MISSING[@]+"${MISSING[@]}"}; do
        read -r path file want <<< "$line"
        echo "mirror-alpine-packages: $file (sha256 $want) isn't in release $TAG" >&2
    done
    if [[ ${#MISSING[@]} -ne 0 || $BAD -ne 0 ]]; then
        echo "mirror-alpine-packages: ${#MISSING[@]} locked package(s) not mirrored, $BAD broken: run \`make deps-dev\`, then \`tools/mirror-alpine-packages.sh upload\`" >&2
        return 1
    fi
    echo "mirror-alpine-packages: release $TAG holds all $(grep -c . <<< "$FILES") locked packages"
}

[[ $# -eq 1 && ("$1" == check || "$1" == upload) ]] || usage
mode=$1

FILES=$(alpine_lock files --lock "$LOCK")

probe_release
if [[ "$mode" == check ]]; then
    if [[ -z "$RELEASE_ID" ]]; then
        echo "mirror-alpine-packages: can't list release $TAG of $REPO: release not found (\`tools/mirror-alpine-packages.sh upload\` creates it)" >&2
        exit 1
    fi
    read_release
    report || exit 1
    exit 0
fi

# upload: what the release lacks, every one of those files checked before
# anything is written to GitHub.
read_release
if [[ $BAD -ne 0 ]]; then
    echo "mirror-alpine-packages: nothing uploaded: delete the broken asset(s) above first" >&2
    exit 1
fi
wrong=0
for line in ${MISSING[@]+"${MISSING[@]}"}; do
    read -r path file want <<< "$line"
    if [[ ! -f "$DIR/$file" ]]; then
        echo "mirror-alpine-packages: $DIR/$file is missing" >&2
        wrong=$((wrong + 1))
        continue
    fi
    got=$(sha256 "$DIR/$file")
    if [[ "$got" != "$want" ]]; then
        echo "mirror-alpine-packages: $DIR/$file has sha256 $got, the lock says $want" >&2
        wrong=$((wrong + 1))
    fi
done
if [[ $wrong -ne 0 ]]; then
    echo "mirror-alpine-packages: nothing uploaded: $wrong file(s) aren't the locked ones: run \`make deps-dev\` first" >&2
    exit 1
fi

uploaded=0
if [[ ${#MISSING[@]} -ne 0 ]]; then
    if [[ -z "$RELEASE_ID" ]]; then
        "$GH" release create "$TAG" --repo "$REPO" --prerelease --title "Alpine packages for the developer image" --notes \
"The Alpine Linux x86_64 binary packages that tools/alpine-packages.lock has pinned, unmodified, each named by its sha256 (\`<sha256>.apk\`) and labelled with its file name, for tools/fetch-alpine-packages.sh to fall back to once dl-cdn drops an older build. Kept by tools/mirror-alpine-packages.sh; never edited by hand.

Each package's licence is in its .PKGINFO. The corresponding sources are Alpine's: the lock that pinned a package names its Alpine branch (vX.Y) in its header; the package's APKBUILD is in https://gitlab.alpinelinux.org/alpine/aports on branch X.Y-stable, and its source archives are on https://distfiles.alpinelinux.org/distfiles/vX.Y/." </dev/null
    fi
    STAGE=$(mktemp -d "${TMPDIR:-/tmp}/lumen-apk-mirror.XXXXXX")
    trap 'rm -rf "$STAGE"' EXIT
    for line in "${MISSING[@]}"; do
        read -r path file want <<< "$line"
        # Upload exactly the bytes checked: the staged copy, hashed again.
        cp "$DIR/$file" "$STAGE/$want.apk"
        got=$(sha256 "$STAGE/$want.apk")
        if [[ "$got" != "$want" ]]; then
            echo "mirror-alpine-packages: $DIR/$file changed while uploading (sha256 $got, the lock says $want): run \`make deps-dev\`, then upload again" >&2
            exit 1
        fi
        if ! "$GH" release upload "$TAG" --repo "$REPO" "$STAGE/$want.apk#$file" </dev/null; then
            echo "mirror-alpine-packages: uploading $file as $want.apk failed; the release may now hold a partial asset of that name: run \`tools/mirror-alpine-packages.sh check\`, which names it if so" >&2
            exit 1
        fi
        rm -f "$STAGE/$want.apk"
        uploaded=$((uploaded + 1))
    done
fi
echo "mirror-alpine-packages: $uploaded uploaded to release $TAG"

# The same pass as `check`, over what GitHub lists now.
probe_release
read_release
report
