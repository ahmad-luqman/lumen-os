#!/usr/bin/env bash
# Fetch every .apk in tools/alpine-packages.lock into build/deps/alpine-packages/,
# sha256-checked (#344), for tools/mkrootfs-dev.sh to build build/alpine-dev.ext4.
#
#   tools/fetch-alpine-packages.sh        (`make deps-dev`)
#
# The lock is read by tools/alpine-lock.py (`files`), its one parser. A file
# already there with the locked sha256 is kept, one with another is fetched
# again, and .apk files the lock doesn't name are removed, so the directory's
# .apk files are exactly the lock's (what CI caches, besides the Makefile's
# stamp).
#
# Sources, tried in this order for each file until one gives the locked
# sha256 (every file is checked, whichever source served it):
#   1. LUMEN_APK_MIRROR: a local or faster mirror to prefer (default: none);
#   2. LUMEN_APK_CDN: default dl-cdn, https://dl-cdn.alpinelinux.org/alpine
#      (the harness's tests point it at a local directory);
#   3. LUMEN_APK_FALLBACK: default the durable mirror, DURABLE_MIRROR below:
#      the repository's `alpine-apk-mirror` release, each locked file an
#      asset named by its sha256 (tools/mirror-alpine-packages.sh fills it,
#      and CI's check fails a lock it doesn't hold all of).
# Each variable is a list split on spaces, tabs and newlines (a YAML block's
# lines too); unset means its default, set empty means none. An entry is
# either a base URL laid out as Alpine's mirrors are
# (<base>/v3.21/<repo>/x86_64/<file>), or a template for a flat directory
# holding `{file}` (the .apk's name) or `{sha256}` (its locked sha256, for a
# store that renames names with `+`, as GitHub's release assets may), such
# as `file:///srv/apks/{sha256}.apk`. Any other `{` or `}` is refused.
#
# LUMEN_PYTHON, if set, runs tools/alpine-lock.py instead of the project's uv
# environment (the harness's tests, on a stub tree with no uv project).
#
# Dropped -rN packages: dl-cdn keeps only the newest build of each package in
# a branch, so a locked `name-1.2-r3.apk` disappears once `-r4` lands
# (tools/fetch-deps.sh's busybox has the same problem). The strategy:
#   1. CI restores build/deps/alpine-packages/ from actions/cache, keyed by
#      the lockfile's hash; a warm cache never downloads.
#   2. A cold cache tries dl-cdn, then the durable mirror (source 3 above).
#   3. A file every source answers isn't there fails the fetch, naming it.
#      The durable mirror should hold it: run tools/mirror-alpine-packages.sh
#      `check`, then `upload` from a machine that has the file. Or re-run
#      tools/alpine-lock.py, which re-resolves against the current index and
#      rewrites the lock (its own PR, like any bump). A source that can't be
#      reached, or serves other bytes under the name, fails saying so
#      instead: neither is a reason to bump the lock.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
LOCK="$ROOT/tools/alpine-packages.lock"
DIR="$ROOT/build/deps/alpine-packages"
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

# The sources in order: LUMEN_APK_MIRROR's and LUMEN_APK_CDN's (FIRST of
# them), then LUMEN_APK_FALLBACK's. Unset means the default, empty means
# none, for all three alike.
MIRRORS=()
read -r -d '' -a MIRRORS <<< "${LUMEN_APK_MIRROR-} ${LUMEN_APK_CDN-https://dl-cdn.alpinelinux.org/alpine}" || true
FIRST=${#MIRRORS[@]}
FALLBACK=()
read -r -d '' -a FALLBACK <<< "${LUMEN_APK_FALLBACK-$DURABLE_MIRROR}" || true
MIRRORS+=(${FALLBACK[@]+"${FALLBACK[@]}"})
fallback_disabled=false
if [[ -n "${LUMEN_APK_FALLBACK+set}" && ${#FALLBACK[@]} -eq 0 ]]; then
    fallback_disabled=true
fi
if [[ ${#MIRRORS[@]} -eq 0 ]]; then
    echo "fetch-alpine-packages: no source to try: LUMEN_APK_MIRROR, LUMEN_APK_CDN and LUMEN_APK_FALLBACK are all empty" >&2
    exit 1
fi
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
from_fallback=0
kept=0
# The sources (by their place in MIRRORS) that couldn't be reached: skipped for
# the rest of the run, so a stalled or unreachable dl-cdn costs one file's
# timeouts, not every file's, before the durable mirror serves the rest.
DEAD=" "
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
    # What each source answered, for the advice below.
    unreachable=false
    mismatch=false
    durable_missing=false
    tried=()
    i=0
    for mirror in "${MIRRORS[@]}"; do
        i=$((i + 1))
        u=$(url "$mirror" "$path" "$file" "$want")
        if [[ "$DEAD" == *" $i "* ]]; then
            tried+=("$u: skipped (unreachable earlier in this run)")
            unreachable=true
            continue
        fi
        rc=0
        note=""
        # The timeouts: a source that accepts the connection but sends
        # nothing (or a trickle) fails each try after 30 s, and its retries
        # stop starting after 60 s, so the next source still gets its turn;
        # an unreachable source is then skipped for the rest of the run (DEAD).
        code=$(curl -fsSL --retry 3 --retry-connrefused --retry-max-time 60 --connect-timeout 15 \
            --speed-limit 1024 --speed-time 30 -w '%{http_code}' -o "$PART" "$u") || rc=$?
        if [[ $rc -eq 0 ]]; then
            got=$(sha256 "$PART")
            if [[ "$got" == "$want" ]]; then
                ok=true
                break
            fi
            outcome="other bytes (sha256 $got)"
            mismatch=true
        elif [[ $rc -eq 37 || ($rc -eq 22 && "$code" =~ ^(403|404|410)$) ]]; then
            # Not found: an HTTP 403, 404 or 410 (stores answer a missing
            # object with any of them), or a file:// path that isn't there.
            if [[ $rc -eq 37 ]]; then outcome="not found (no such file)"; else outcome="not found (HTTP $code)"; fi
            if [[ "$mirror" == "$DURABLE_MIRROR" ]]; then durable_missing=true; fi
        else
            # Anything else (no connection, a 5xx) is an unreachable source.
            if [[ $rc -eq 22 ]]; then outcome="unreachable (HTTP $code)"; else outcome="unreachable (curl exit $rc)"; fi
            unreachable=true
            DEAD+="$i "
            note=" (skipped for the rest of this run)"
        fi
        tried+=("$u: $outcome")
        if [[ $i -lt ${#MIRRORS[@]} ]]; then
            echo "fetch-alpine-packages: $u: $outcome$note, trying the next source" >&2
        fi
    done
    if ! $ok; then
        echo "fetch-alpine-packages: no mirror gave $file with sha256 $want; tried:" >&2
        printf '  %s\n' "${tried[@]}" >&2
        # Every line that applies, from what the sources answered.
        if $unreachable; then
            echo "  A mirror couldn't be reached (curl's error above): check the network and retry." >&2
        fi
        if $mismatch; then
            # Alpine never changes the bytes behind a name (a rebuild is a new -rN).
            echo "  A mirror served other bytes under that name (a proxy, a captive portal or a corrupt" >&2
            echo "  mirror): fix or drop that mirror; that is no reason to re-lock." >&2
        fi
        if $durable_missing; then
            echo "  The durable mirror should hold every locked file: run tools/mirror-alpine-packages.sh check," >&2
            echo "  then \`tools/mirror-alpine-packages.sh upload\` from a machine that has it." >&2
        fi
        if ! $unreachable && ! $mismatch; then
            echo "  Alpine drops a package's older -rN builds. Re-run tools/alpine-lock.py to lock the current" >&2
            echo "  ones (its own PR), or point LUMEN_APK_MIRROR at a mirror that kept these." >&2
        fi
        if $fallback_disabled; then
            echo "  LUMEN_APK_FALLBACK is set empty, so the durable mirror wasn't tried: unset it to try" >&2
            echo "  $DURABLE_MIRROR." >&2
        fi
        exit 1
    fi
    mv -f "$PART" "$DIR/$file"
    fetched=$((fetched + 1))
    if [[ $i -gt $FIRST ]]; then
        from_fallback=$((from_fallback + 1))
    fi
done <<< "$FILES"

removed=0
for f in "$DIR"/*.apk; do
    [[ -e "$f" ]] || continue
    if [[ "$WANTED" != *" $(basename "$f") "* ]]; then
        rm -f "$f"
        removed=$((removed + 1))
    fi
done
echo "fetch-alpine-packages: $((fetched + kept)) packages ($fetched fetched ($from_fallback from the fallback), $kept already there, $removed stale removed)"
