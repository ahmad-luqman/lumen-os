#!/usr/bin/env bash
# Fetch every .apk in tools/alpine-packages.lock into build/deps/alpine-packages/,
# sha256-checked (#344). tools/mkrootfs-dev.sh builds build/alpine-dev.ext4 from them.
#
#   tools/fetch-alpine-packages.sh        (`make deps-dev`)
#
# A file already there with the locked sha256 is kept; one with another sha256
# is fetched again; .apk files the lock doesn't name are removed, so the
# directory holds exactly the lock (what CI caches).
#
# Mirrors: LUMEN_APK_MIRROR, a space-separated list tried in order, then
# LUMEN_APK_CDN (dl-cdn's https://dl-cdn.alpinelinux.org/alpine unless set;
# the harness's tests point it at a local directory). An entry is a base URL laid out as Alpine's mirrors are
# (<base>/v3.21/<repo>/x86_64/<file>), or a template holding `{file}` (and
# maybe `{repo}`), for a flat directory such as a release's assets:
#   LUMEN_APK_MIRROR='https://github.com/<owner>/<repo>/releases/download/<tag>/{file}'
#
# Dropped -rN packages: dl-cdn keeps only the newest build of each package in
# a branch, so a locked `name-1.2-r3.apk` disappears once `-r4` lands
# (tools/fetch-deps.sh's busybox has the same problem). The strategy:
#   1. CI restores build/deps/alpine-packages/ from actions/cache, keyed by
#      the lockfile's hash (wired by the gate issues); a warm cache never
#      downloads, so a dropped file breaks only a cold cache.
#   2. A durable mirror in LUMEN_APK_MIRROR (proposed in #344's PR: the
#      locked files as a GitHub release's assets; not created yet).
#   3. When every mirror has lost a file, this script fails, naming it and
#      saying to re-run tools/alpine-lock.py, which re-resolves against the
#      current index and rewrites the lock (its own PR, like any bump).
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
LOCK="$ROOT/tools/alpine-packages.lock"
DIR="$ROOT/build/deps/alpine-packages"
BRANCH=v3.21
ARCH=x86_64
CDN=${LUMEN_APK_CDN:-https://dl-cdn.alpinelinux.org/alpine}

sha256() {
    if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# url <mirror> <repo> <file>
url() {
    local mirror=$1 repo=$2 file=$3
    if [[ "$mirror" == *"{file}"* ]]; then
        mirror=${mirror//\{repo\}/$repo}
        printf '%s\n' "${mirror//\{file\}/$file}"
    else
        printf '%s/%s/%s/%s/%s\n' "${mirror%/}" "$BRANCH" "$repo" "$ARCH" "$file"
    fi
}

if [[ ! -f "$LOCK" ]]; then
    echo "fetch-alpine-packages: $LOCK not found" >&2
    exit 1
fi

read -ra MIRRORS <<< "${LUMEN_APK_MIRROR:-}"
MIRRORS+=("$CDN")

mkdir -p "$DIR"
PART="$DIR/.part.$$"
trap 'rm -f "$PART"' EXIT

# The locked files, space-separated (bash 3.2, macOS's, has no associative arrays).
WANTED=" "
count=0
fetched=0
kept=0
while read -r name version repo want rest; do
    [[ -z "$name" || "$name" == \#* ]] && continue
    if [[ -n "$rest" || ! "$want" =~ ^[0-9a-f]{64}$ || ! "$name" =~ ^[A-Za-z0-9][A-Za-z0-9+._-]*$ \
        || ! "$version" =~ ^[0-9][A-Za-z0-9._~]*-r[0-9]+$ || ! "$repo" =~ ^(main|community)$ ]]; then
        echo "fetch-alpine-packages: malformed lock line: $name $version $repo $want $rest" >&2
        exit 1
    fi
    file="$name-$version.apk"
    WANTED+="$file "
    count=$((count + 1))
    if [[ -f "$DIR/$file" && "$(sha256 "$DIR/$file")" == "$want" ]]; then
        kept=$((kept + 1))
        continue
    fi
    got=""
    tried=()
    for mirror in "${MIRRORS[@]}"; do
        u=$(url "$mirror" "$repo" "$file")
        tried+=("$u")
        if curl -fsSL --retry 3 -o "$PART" "$u"; then
            got=$(sha256 "$PART")
            if [[ "$got" == "$want" ]]; then
                break
            fi
            echo "fetch-alpine-packages: $u: sha256 $got, the lock says $want" >&2
        fi
        got=""
    done
    if [[ -z "$got" ]]; then
        echo "fetch-alpine-packages: no mirror has $file with sha256 $want; tried:" >&2
        printf '  %s\n' "${tried[@]}" >&2
        echo "  Alpine drops a package's older -rN builds. Re-run tools/alpine-lock.py to lock the current" >&2
        echo "  ones (its own PR), or point LUMEN_APK_MIRROR at a mirror that kept these." >&2
        exit 1
    fi
    mv -f "$PART" "$DIR/$file"
    fetched=$((fetched + 1))
done < "$LOCK"

if [[ $count -eq 0 ]]; then
    echo "fetch-alpine-packages: $LOCK names no package" >&2
    exit 1
fi

removed=0
for f in "$DIR"/*.apk; do
    [[ -e "$f" ]] || continue
    if [[ "$WANTED" != *" $(basename "$f") "* ]]; then
        rm -f "$f"
        removed=$((removed + 1))
    fi
done
echo "fetch-alpine-packages: $count packages ($fetched fetched, $kept already there, $removed stale removed)"
