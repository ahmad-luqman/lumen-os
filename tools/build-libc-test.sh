#!/usr/bin/env bash
# Build musl's libc-test for x86_64-linux-musl with the pinned zig, into build/libc-test/.
#
#   build/libc-test/src/{functional,regression}/<name>-static.exe   static tests
#   build/libc-test/src/{functional,regression}/<name>.exe          dynamic tests
#                                    (interpreter /lib/ld-musl-x86_64.so.1, as Alpine's)
#   build/libc-test/src/{functional,regression}/*.so                the dynamic tests' libraries
#   build/libc-test/src/common/runtest-static.exe                   the runner, static
#   build/libc-test/tests.txt       every test that built, one path per line (the ids
#                                   lumen_harness.libctest and the expected-failure list use)
#   build/libc-test/not-built.txt   every test or library that didn't build, and why
#   build/libc-test/build.log       the whole build's output
#   build/libc-test/...             the rest of the pinned commit's tree, the objects, and
#                                   libc-test's own .err files (compiler and linker output)
#
# Inputs: build/deps/zig (tools/fetch-zig.sh) and build/deps/libc-test
# (tools/fetch-libc-test.sh); `make libc-test` fetches both, then runs this.
#
# The build is libc-test's own Makefile (its per-test flags and the .mk files
# that say which tests need a library), run in build/libc-test on a copy of
# the pinned commit's tree (`git archive`), with a config.mak that sets
# CC="zig cc -target x86_64-linux-musl", and with its default build directory
# B=src: a dynamic test linked against a test library names it by that
# relative path (src/functional/tls_align_dso.so), so the tests run from the
# directory holding src/, here and in Lumen.
# Every test is built twice, static (`-static`) and dynamic: zig 0.15.2's musl
# is 1.2.5, the same as Alpine 3.21's loader (see tools/fetch-zig.sh), so a
# dynamic test runs against the libc Lumen's Alpine programs use. The
# dlopen and TLS-in-a-library tests (dlopen, tls_align_dlopen,
# tls_init_dlopen, tls_get_new-dtv) exist only dynamic: libc-test's .mk files
# build them so, as a static musl program can't load libraries.
#
# Built: src/functional (libc behaviour over the kernel's syscalls: threads,
# signals, files, sockets, IPC, time, ...) and src/regression (musl bug
# regressions, many of them kernel-facing). Left out:
#   src/math    libm's input-output vectors: pure computation, no kernel surface
#               (and zig's compiler-rt provides some of the math functions);
#   src/api     compile-only header checks: nothing to run;
#   src/musl    musl's internal interfaces (__pleval), not ABI.
#
# libc-test's make never fails on a test that doesn't build (`|| echo
# BUILDERROR`); this script lists each one, with its first error line, rather
# than skipping it silently, and fails only if libc-test's Makefile names no
# tests, the runner itself doesn't build, a built binary's linkage isn't what
# its name says (or a test, the runner or a library carries zig's UBSan
# runtime or a .debug_ section, or a test or the runner asks for a stack
# size), or make itself fails.
#
# zig's caches live in build/libc-test/.zig-cache, never in the home
# directory. A second run with the same zig, libc-test commit and script does
# nothing.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
ZIG_DIR="$ROOT/build/deps/zig"
SRC="$ROOT/build/deps/libc-test"
OUT="$ROOT/build/libc-test"
ZIG="$ZIG_DIR/zig"

die() {
    echo "build-libc-test: $*" >&2
    exit 1
}

[[ -x "$ZIG" && -f "$ZIG_DIR/.version" ]] || die "no pinned zig in build/deps/zig (run tools/fetch-zig.sh or make deps-zig)"
[[ -f "$SRC/.version" && -d "$SRC/.git" ]] || die "no libc-test in build/deps/libc-test (run tools/fetch-libc-test.sh)"
COMMIT=$(git -C "$SRC" rev-parse --verify HEAD)
[[ "$COMMIT" == "$(cat "$SRC/.version")" ]] || die "build/deps/libc-test is at $COMMIT, not its stamp's commit (rerun tools/fetch-libc-test.sh)"

sha256() {
    if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}
STAMP="zig $(cat "$ZIG_DIR/.version"); libc-test $COMMIT; script $(sha256 "${BASH_SOURCE[0]}")"
if [[ -f "$OUT/.stamp" && "$(cat "$OUT/.stamp")" == "$STAMP" && -f "$OUT/tests.txt" ]]; then
    echo "build-libc-test: up to date ($(wc -l <"$OUT/tests.txt" | tr -d ' ') tests, $(wc -l <"$OUT/not-built.txt" | tr -d ' ') not built: see build/libc-test/not-built.txt)"
    exit 0
fi

echo "build-libc-test: building libc-test $COMMIT with zig $(cat "$ZIG_DIR/.version")"
rm -rf "$OUT"
mkdir -p "$OUT"
git -C "$SRC" archive --format=tar "$COMMIT" | tar -xf - -C "$OUT"

# config.mak.def's flags as they are (its glibc-only libraries are part of
# musl's libc, and zig accepts them); then the cross-compiler. `-dynamic`:
# zig links musl programs statically unless told otherwise; libc-test's static
# rule adds `-static` after CC, which wins (checked below, binary by binary).
# `-fno-sanitize=all`: without an -O flag zig cc builds in its Debug mode,
# which compiles in UBSan and links zig's UBSan runtime (2 MB a binary, its
# own syscalls when it reports); libc-test means plain C at -O0, as with gcc
# (also checked below). `-z stack-size=0`: zig's linker otherwise writes a
# 16 MiB PT_GNU_STACK, which musl takes as the default thread stack size
# (capped at 8 MiB); GNU ld writes 0, so Alpine's programs get musl's 128 KiB.
# `--strip-debug`: the DWARF names the host's build directory and zig's
# randomly named cache directories, so two builds would differ byte for byte
# (zig's linker then strips the symbol table too; no binary keeps a .debug_
# section, checked below). With it, two builds were byte-identical when
# checked by hand (two builds, 289 .exe and .so files); no test compares
# builds.
{
    echo "# Written by tools/build-libc-test.sh"
    echo "include config.mak.def"
    echo "CFLAGS += -fno-sanitize=all"
    echo "LDFLAGS += -fno-sanitize=all -Wl,-z,stack-size=0 -Wl,--strip-debug"
    echo "CC := $ZIG cc -target x86_64-linux-musl -dynamic"
    echo "AR := $ZIG ar"
    echo "RANLIB := $ZIG ranlib"
} >"$OUT/config.mak"

export ZIG_GLOBAL_CACHE_DIR="$OUT/.zig-cache/global"
export ZIG_LOCAL_CACHE_DIR="$OUT/.zig-cache/local"
# An outer make's flags (`make -n`, its jobserver) are not this build's.
unset MAKEFLAGS MFLAGS MAKELEVEL MAKEOVERRIDES
MK=(make --no-print-directory -C "$OUT")

# The binaries libc-test's Makefile would build (its BINS and LIBS, after the
# .mk files), kept to the two directories built here.
"${MK[@]}" debug >"$OUT/make-debug.txt"
# (Paths without spaces, as make's own lists are; no mapfile: macOS's bash is 3.2.)
words() {
    local w
    for w in $(sed -n "s/^$1 //p" "$OUT/make-debug.txt"); do
        case "$w" in src/functional/* | src/regression/*) echo "$w" ;; esac
    done | sort
}
TESTS=($(words BINS))
LIBS=($(words LIBS))
((${#TESTS[@]} > 0)) || die "libc-test's Makefile names no tests (see build/libc-test/make-debug.txt)"

# LIBC_TEST_JOBS caps the parallel compiles (each zig cc can take a few
# hundred MB); the default is one per CPU.
JOBS=${LIBC_TEST_JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)}
[[ "$JOBS" =~ ^[1-9][0-9]*$ ]] || die "LIBC_TEST_JOBS must be a positive number, not '$JOBS'"
RUNNER=src/common/runtest-static.exe
# libtest.a first: the runner's static link names it but doesn't depend on it.
if ! { "${MK[@]}" src/common/libtest.a && "${MK[@]}" -k -j"$JOBS" "$RUNNER" ${LIBS[@]+"${LIBS[@]}"} "${TESTS[@]}"; } >"$OUT/build.log" 2>&1; then
    tail -n 30 "$OUT/build.log" >&2
    die "make failed (above: the end of build/libc-test/build.log)"
fi
[[ -x "$OUT/$RUNNER" ]] || die "the runner (src/common/runtest-static.exe) didn't build: see build/libc-test/build.log"

# why <binary>: the first error its compile or link printed (libc-test's
# rules keep each one's output in <object>.o.err or <object>.lo.err, and
# <binary>.ld.err for a program or <name>.so.err for a library: its rule
# writes $@.err).
why() {
    local bin=$OUT/$1 obj
    obj="${bin%.so}"
    obj="${obj%.exe}"
    obj="${obj%-static}"
    local f line
    for f in "$obj.o.err" "$obj.lo.err" "$bin.ld.err" "$bin.err"; do
        [[ -s "$f" ]] || continue
        line=$(grep -m1 -i 'error' "$f" || grep -m1 . "$f" || true)
        if [[ -n "$line" ]]; then
            echo "$line"
            return
        fi
    done
    echo "no diagnostics (see build/libc-test/build.log)"
}

: >"$OUT/tests.txt"
: >"$OUT/not-built.txt"
for bin in "${TESTS[@]}"; do
    if [[ -x "$OUT/$bin" ]]; then
        echo "$bin" >>"$OUT/tests.txt"
    else
        echo "$bin # $(why "$bin")" >>"$OUT/not-built.txt"
    fi
done
for lib in ${LIBS[@]+"${LIBS[@]}"}; do
    [[ -f "$OUT/$lib" ]] || echo "$lib # library: $(why "$lib")" >>"$OUT/not-built.txt"
done

(cd "$ROOT" && PYTHONPATH=tools/harness uv run --frozen python -m lumen_harness.libctest linkage "$OUT") \
    || die "a binary's linkage isn't what its name says (above)"

BUILT=$(wc -l <"$OUT/tests.txt" | tr -d ' ')
STATIC=$(grep -c -- '-static\.exe$' "$OUT/tests.txt" || true)
NOT_BUILT=$(wc -l <"$OUT/not-built.txt" | tr -d ' ')
echo "build-libc-test: $BUILT tests built ($STATIC static, $((BUILT - STATIC)) dynamic), $NOT_BUILT not built"
if ((NOT_BUILT > 0)); then
    echo "build-libc-test: not built (also in build/libc-test/not-built.txt):"
    sed 's/^/  /' "$OUT/not-built.txt"
fi
echo "$STAMP" >"$OUT/.stamp"
