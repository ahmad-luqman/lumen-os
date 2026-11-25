#!/usr/bin/env bash
# Fetch every .apk in tools/alpine-packages.lock into build/deps/alpine-packages/,
# sha256-checked (#344). tools/mkrootfs-dev.sh builds build/alpine-dev.ext4 from them.
#
#   tools/fetch-alpine-packages.sh        (`make deps-dev`)
#
# The lock is read by tools/alpine-lock.py (`files`), its one parser. A file
# already there with the locked sha256 is kept; one with another sha256 is
# fetched again; .apk files the lock doesn't name are removed, so the
# directory's .apk files are exactly the lock's (what CI caches, besides the
# Makefile's stamp).
#
# Sources, tried in this order for each file until one gives the locked sha256:
#   1. LUMEN_APK_MIRROR: a list split on spaces or newlines, tried in order
#      (empty unless set: a local or faster mirror to prefer);
#   2. LUMEN_APK_CDN (dl-cdn's https://dl-cdn.alpinelinux.org/alpine unless
#      set; the harness's tests point it at a local directory);
#   3. LUMEN_APK_FALLBACK: a list like LUMEN_APK_MIRROR's, unset meaning the
#      durable mirror, DURABLE_MIRROR below (set it empty for none): every
#      locked file as an asset of the repository's `alpine-apk-mirror`
#      release, named by its sha256 (tools/mirror-alpine-packages.sh keeps it
#      complete, and CI's check fails a lock it doesn't hold).
# Every file is sha256-checked against the lock whichever source served it.
# An entry is either a
# base URL laid out as Alpine's mirrors are (<base>/v3.21/<repo>/x86_64/<file>),
# or a template for a flat directory holding `{file}` (the .apk's name) or
# `{sha256}` (its locked sha256, for a store that renames names with `+`, as
# GitHub's release assets may: name each asset <sha256>.apk):
#   LUMEN_APK_MIRROR='https://github.com/<owner>/<repo>/releases/download/<tag>/{sha256}.apk'
# Any other `{` or `}` in an entry is refused.
#
# LUMEN_PYTHON, if set, runs tools/alpine-lock.py instead of the project's uv
# environment (the harness's tests, on a stub tree with no uv project).
#
# Dropped -rN packages: dl-cdn keeps only the newest build of each package in
# a branch, so a locked `name-1.2-r3.apk` disappears once `-r4` lands
# (tools/fetch-deps.sh's busybox has the same problem). The strategy:
#   1. CI restores build/deps/alpine-packages/ from actions/cache, keyed by
#      the lockfile's hash; a warm cache never downloads.
#   2. The durable mirror, tried after dl-cdn (LUMEN_APK_FALLBACK above), so a
#      dropped file still comes back on a cold cache.
#   3. When every mirror answers that a file isn't there, this script fails,
#      naming it and saying to re-run tools/alpine-lock.py, which re-resolves
#      against the current index and rewrites the lock (its own PR, like any
#      bump). When a mirror can't be reached, or serves other bytes under the
#      name, it fails saying so instead: neither is a reason to bump the lock.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
LOCK="$ROOT/tools/alpine-packages.lock"
DIR="$ROOT/build/deps/alpine-packages"
CDN=${LUMEN_APK_CDN:-https://dl-cdn.alpinelinux.org/alpine}
# tools/mirror-alpine-packages.sh uploads to this release (its REPO and TAG);
# tests/harness/test_rootfs_dev.py checks the two agree.
DURABLE_MIRROR='https://github.com/ahmad-luqman/lumen-os/releases/download/alpine-apk-mirror/{sha256}.apk'

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

# url <mirror> <path under an Alpine-layout base> <file> <sha256>
url() {
    local mirror=$1 path=$2 file=$3 sha=$4
    if [[ "$mirror" == *"{file}"* || "$mirror" == *"{sha256}"* ]]; then
        mirror=${mirror//\{file\}/$file}
        printf '%s\n' "${mirror//\{sha256\}/$sha}"
    else
        printf '%s/%s\n' "${mirror%/}" "$path"
    fi
}

# Mirrors split on spaces, tabs and newlines (a YAML block's lines too).
MIRRORS=()
read -r -d '' -a MIRRORS <<< "${LUMEN_APK_MIRROR:-}" || true
MIRRORS+=("$CDN")
FALLBACK=()
read -r -d '' -a FALLBACK <<< "${LUMEN_APK_FALLBACK-$DURABLE_MIRROR}" || true
MIRRORS+=(${FALLBACK[@]+"${FALLBACK[@]}"})
for mirror in "${MIRRORS[@]}"; do
    rest=${mirror//\{file\}/}
    rest=${rest//\{sha256\}/}
    if [[ "$rest" == *"{"* || "$rest" == *"}"* ]]; then
        echo "fetch-alpine-packages: mirror $mirror: a template takes only {file} and {sha256}" >&2
        exit 1
    fi
done

# The lock, as alpine-lock.py checks it: `mirror-path file sha256` lines,
# the path laid out as Alpine's mirrors are (alpine-lock.py's mirror_path).
FILES=$(alpine_lock files --lock "$LOCK")

mkdir -p "$DIR"
PART="$DIR/.part.$$"
trap 'rm -f "$PART"' EXIT

# The locked files, space-separated (bash 3.2, macOS's, has no associative arrays).
WANTED=" "
fetched=0
kept=0
while read -r path file want; do
    if [[ -z "$path" || -z "$file" || -z "$want" ]]; then
        echo "fetch-alpine-packages: alpine-lock.py files gave a short line: '$path $file $want'" >&2
        exit 1
    fi
    WANTED+="$file "
    if [[ -f "$DIR/$file" && "$(sha256 "$DIR/$file")" == "$want" ]]; then
        kept=$((kept + 1))
        continue
    fi
    ok=false
    unreachable=false
    mismatch=false
    tried=()
    for mirror in "${MIRRORS[@]}"; do
        u=$(url "$mirror" "$path" "$file" "$want")
        tried+=("$u")
        rc=0
        code=$(curl -fsSL --retry 3 --retry-connrefused -w '%{http_code}' -o "$PART" "$u") || rc=$?
        if [[ $rc -ne 0 ]]; then
            # Not found: an HTTP 403, 404 or 410 (stores answer a missing
            # object with any of them), or a file:// path that isn't there.
            # Anything else (no connection, a 5xx) is an unreachable mirror.
            [[ $rc -eq 37 || ($rc -eq 22 && "$code" =~ ^(403|404|410)$) ]] || unreachable=true
            continue
        fi
        got=$(sha256 "$PART")
        if [[ "$got" == "$want" ]]; then
            ok=true
            break
        fi
        mismatch=true
        echo "fetch-alpine-packages: $u: sha256 $got, the lock says $want" >&2
    done
    if ! $ok; then
        echo "fetch-alpine-packages: no mirror gave $file with sha256 $want; tried:" >&2
        printf '  %s\n' "${tried[@]}" >&2
        if $unreachable; then
            echo "  A mirror couldn't be reached (curl's error above): check the network and retry." >&2
        elif $mismatch; then
            # Alpine never changes the bytes behind a name (a rebuild is a new -rN).
            echo "  A mirror served other bytes under that name (a proxy, a captive portal or a corrupt" >&2
            echo "  mirror): fix or drop that mirror; that is no reason to re-lock." >&2
        else
            echo "  Alpine drops a package's older -rN builds. Re-run tools/alpine-lock.py to lock the current" >&2
            echo "  ones (its own PR), or point LUMEN_APK_MIRROR at a mirror that kept these." >&2
        fi
        exit 1
    fi
    mv -f "$PART" "$DIR/$file"
    fetched=$((fetched + 1))
done <<< "$FILES"

removed=0
for f in "$DIR"/*.apk; do
    [[ -e "$f" ]] || continue
    if [[ "$WANTED" != *" $(basename "$f") "* ]]; then
        rm -f "$f"
        removed=$((removed + 1))
    fi
done
echo "fetch-alpine-packages: $((fetched + kept)) packages ($fetched fetched, $kept already there, $removed stale removed)"
