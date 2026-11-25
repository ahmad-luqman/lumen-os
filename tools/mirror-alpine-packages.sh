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
# `check` reads the release's asset list (every page), then downloads the
# headers of the first locked file anonymously, at the URL the fetch uses
# (DURABLE_MIRROR, read from tools/fetch-alpine-packages.sh), so a release
# listed but not downloadable fails too. `upload` takes the files from
# build/deps/alpine-packages/ (`make deps-dev` first): it finds the locked
# files the release lacks, checks every one of them against the lock's sha256
# before uploading any, creates the release if there is none, uploads each
# from a staged copy whose sha256 it checked, and ends with `check`'s pass
# over the listing (read once more after a pause if GitHub's listing lags).
# The tool's policy is never to replace or delete an asset: a name is its
# content's hash, and an older lock's files stay fetchable.
#
# gh's reads are tried up to 3 times, LUMEN_GH_RETRY_SLEEP seconds apart
# (default 2), then twice that; "release not found" is an answer, never
# retried. LUMEN_GH runs instead of `gh` (the harness's tests: a stub);
# LUMEN_PYTHON runs tools/alpine-lock.py instead of the project's uv
# environment, as in tools/fetch-alpine-packages.sh.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
LOCK="$ROOT/tools/alpine-packages.lock"
DIR="$ROOT/build/deps/alpine-packages"
# tools/fetch-alpine-packages.sh's DURABLE_MIRROR names this release; the
# harness checks the two agree.
REPO=ahmad-luqman/lumen-os
TAG=alpine-apk-mirror
GH=${LUMEN_GH:-gh}
RETRY_SLEEP=${LUMEN_GH_RETRY_SLEEP:-2}
if [[ ! "$RETRY_SLEEP" =~ ^[0-9]+$ ]]; then
    echo "mirror-alpine-packages: LUMEN_GH_RETRY_SLEEP is '$RETRY_SLEEP', not a whole number of seconds" >&2
    exit 2
fi

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

# gh_read <gh arguments>: a read-only gh call, its stdout in $WORK/out and its
# stderr in $WORK/err, tried up to 3 times; returns gh's last exit code.
gh_read() {
    local try=1 rc delay=$RETRY_SLEEP
    while :; do
        rc=0
        "$GH" "$@" </dev/null >"$WORK/out" 2>"$WORK/err" || rc=$?
        if [[ $rc -eq 0 || "$(<"$WORK/err")" == "release not found" || $try -ge 3 ]]; then
            return $rc
        fi
        echo "mirror-alpine-packages: gh $1 $2 failed ($(<"$WORK/err")), trying again in ${delay}s" >&2
        sleep "$delay"
        delay=$((delay * 2))
        try=$((try + 1))
    done
}

# Sets RELEASE_ID to the release's id, or to "" when there is none. A draft,
# or any gh failure but "release not found", stops the script. (A draft is
# visible only to a token that may write to the repository: under CI's
# read-only token, a draft reads as "release not found".)
probe_release() {
    local out
    RELEASE_ID=""
    if ! gh_read release view "$TAG" --repo "$REPO" --json databaseId,isDraft \
        --jq '"\(.databaseId) \(.isDraft)"'; then
        if [[ "$(<"$WORK/err")" == "release not found" ]]; then
            return 0
        fi
        echo "mirror-alpine-packages: can't read release $TAG of $REPO: $(<"$WORK/err")" >&2
        exit 1
    fi
    out=$(<"$WORK/out")
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

# read_release [quiet]: sets MISSING to the `path file sha256` lines of the
# locked files the release lacks, and BAD to how many it holds but not as a
# finished upload of the named bytes, naming each of those on stderr unless
# quiet. Every asset is read, page by page, as a `name<TAB>state<TAB>digest`
# line.
read_release() {
    local listing="" path file want row running
    MISSING=()
    BAD=0
    if [[ -n "$RELEASE_ID" ]]; then
        if ! gh_read api --paginate "repos/$REPO/releases/$RELEASE_ID/assets?per_page=100" \
            --jq '.[] | [.name, .state, (.digest // "")] | @tsv'; then
            echo "mirror-alpine-packages: can't list the assets of release $TAG of $REPO: $(<"$WORK/err")" >&2
            exit 1
        fi
        listing=$(<"$WORK/out")
    fi
    listing=$'\n'"$listing"$'\n'
    while read -r path file want; do
        if [[ -z "$path" || -z "$file" || -z "$want" ]]; then
            echo "mirror-alpine-packages: alpine-lock.py files gave a short line: '$path $file $want'" >&2
            exit 1
        fi
        # Its asset's `state<TAB>digest` (asset names are unique in a release).
        row=${listing#*$'\n'"$want.apk"$'\t'}
        if [[ "$row" == "$listing" ]]; then
            MISSING+=("$path $file $want")
            continue
        fi
        row=${row%%$'\n'*}
        if [[ "$row" == "uploaded"$'\t'"sha256:$want" ]]; then
            continue
        fi
        BAD=$((BAD + 1))
        if [[ "${1:-}" == quiet ]]; then
            continue
        fi
        if [[ "${row%%$'\t'*}" == uploaded ]]; then
            running=""
        else
            running="if no upload is running, "
        fi
        echo "mirror-alpine-packages: release $TAG holds $want.apk ($file), but its state is '${row%%$'\t'*}' and its digest '${row#*$'\t'}', not a finished upload of those bytes: ${running}delete it by hand (\`gh release delete-asset $TAG $want.apk --repo $REPO\`), then run \`tools/mirror-alpine-packages.sh upload\`" >&2
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
    if [[ $BAD -ne 0 ]]; then
        echo "mirror-alpine-packages: ${#MISSING[@]} locked package(s) not mirrored, $BAD broken: delete each broken asset named above, run \`make deps-dev\`, then \`tools/mirror-alpine-packages.sh upload\`" >&2
        return 1
    fi
    if [[ ${#MISSING[@]} -ne 0 ]]; then
        echo "mirror-alpine-packages: ${#MISSING[@]} locked package(s) not mirrored, 0 broken: run \`make deps-dev\`, then \`tools/mirror-alpine-packages.sh upload\`" >&2
        return 1
    fi
    echo "mirror-alpine-packages: release $TAG holds all $(grep -c . <<< "$FILES") locked packages"
}

# The release's assets listed is not the fetch able to download them: one
# anonymous request for the first locked file's headers, at the fetch's URL.
download_probe() {
    local durable path file want u
    durable=$(sed -n "s/^DURABLE_MIRROR='\(.*\)'\$/\1/p" "$ROOT/tools/fetch-alpine-packages.sh")
    if [[ -z "$durable" ]]; then
        echo "mirror-alpine-packages: tools/fetch-alpine-packages.sh sets no DURABLE_MIRROR" >&2
        exit 1
    fi
    read -r path file want <<< "$FILES"
    u=${durable//\{file\}/$file}
    u=${u//\{sha256\}/$want}
    # Retried as the gh reads are, so one blip on GitHub's asset host
    # doesn't fail CI.
    if ! curl -q -fsIL --retry 3 --retry-connrefused --max-time 20 -o /dev/null "$u" </dev/null; then
        echo "mirror-alpine-packages: release $TAG is listed but not downloadable at $u" >&2
        exit 1
    fi
}

[[ $# -eq 1 && ("$1" == check || "$1" == upload) ]] || usage
mode=$1

FILES=$(alpine_lock files --lock "$LOCK")
WORK=$(mktemp -d "${TMPDIR:-/tmp}/lumen-apk-mirror.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

probe_release
if [[ "$mode" == check ]]; then
    if [[ -z "$RELEASE_ID" ]]; then
        echo "mirror-alpine-packages: can't list release $TAG of $REPO: release not found, or a draft this token can't see (\`tools/mirror-alpine-packages.sh upload\` creates it)" >&2
        exit 1
    fi
    read_release
    report
    download_probe
    exit
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

if [[ ${#MISSING[@]} -ne 0 ]]; then
    if [[ -z "$RELEASE_ID" ]]; then
        "$GH" release create "$TAG" --repo "$REPO" --prerelease --title "Alpine packages for the developer image" --notes \
"The Alpine Linux x86_64 binary packages that tools/alpine-packages.lock has pinned, unmodified, each named by its sha256 (\`<sha256>.apk\`) and labelled with its file name, for tools/fetch-alpine-packages.sh to fall back to when dl-cdn doesn't serve a locked file (an older build dropped, or dl-cdn down). Kept by tools/mirror-alpine-packages.sh, which never replaces or deletes an asset; by hand, delete only an asset it names as broken.

Each package's licence is in its .PKGINFO. The corresponding sources are Alpine's: the lock that pinned a package names its Alpine branch (vX.Y) in its header; the package's APKBUILD is in https://gitlab.alpinelinux.org/alpine/aports on branch X.Y-stable, and its source archives are on https://distfiles.alpinelinux.org/distfiles/vX.Y/." </dev/null
    fi
    STAGE=$WORK
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
    done
fi
echo "mirror-alpine-packages: ${#MISSING[@]} uploaded to release $TAG"

# The same pass as `check`, over what GitHub lists now: read once more after
# a pause if anything is missing or broken, in case the listing lags.
probe_release
read_release quiet
if [[ ${#MISSING[@]} -ne 0 || $BAD -ne 0 ]]; then
    echo "mirror-alpine-packages: the release's listing isn't complete yet; reading it again in $((RETRY_SLEEP * 2))s" >&2
    sleep $((RETRY_SLEEP * 2))
    # A release created just now may not be found at once either.
    probe_release
    read_release
fi
report
