#!/usr/bin/env bash
# #454: standalone, static x86_64-linux-musl gVisor spike. No Bazel, source
# patches, downloads, or execution of target programs. Requires the existing
# pinned zig, cmake, ninja, and Python 3. Build reports survive partial failure.
# GVISOR_TEST_JOBS bounds C++ compilation memory (default 2).
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"
OUT="$ROOT/build/gvisor-tests"
for tool in cmake ninja python3; do
    command -v "$tool" >/dev/null || { echo "build-gvisor-tests: missing $tool" >&2; exit 1; }
done
[[ -x build/deps/zig/zig && $(build/deps/zig/zig version) == 0.15.2 ]] || {
    echo "build-gvisor-tests: missing pinned zig 0.15.2 in build/deps/zig" >&2; exit 1;
}
tools/fetch-gvisor-tests.sh --check
JOBS=${GVISOR_TEST_JOBS:-2}
[[ "$JOBS" =~ ^[1-9][0-9]*$ ]] || { echo "GVISOR_TEST_JOBS must be positive" >&2; exit 1; }
mkdir -p "$OUT"
mkdir "$OUT/.lock" 2>/dev/null || { echo "build-gvisor-tests: build already locked ($OUT/.lock)" >&2; exit 1; }
trap 'rmdir "$OUT/.lock"' EXIT
START=$(date +%s)
# A configure failure must not leave an earlier success report behind.
rm -f "$OUT/tests.txt" "$OUT/not-built.txt" "$OUT/build-report.json" "$OUT/native-report.json"
rm -rf "$OUT/native"
# Extraction always uses verified git objects; no trust in cached source trees.
rm -rf "$OUT/src"
mkdir -p "$OUT/src" "$OUT/logs"
for name in gvisor googletest abseil; do
    mkdir "$OUT/src/$name"
    git -C "build/gvisor-sources/$name" archive HEAD | tar -xf - -C "$OUT/src/$name"
done
export ZIG_GLOBAL_CACHE_DIR="$OUT/.zig-cache/global"
export ZIG_LOCAL_CACHE_DIR="$OUT/.zig-cache/local"
# These wrappers also keep Apple's ar/ranlib out of ELF archives on macOS.
for pair in cc:cc cxx:c++ ar:ar ranlib:ranlib; do
    name=${pair%%:*} command=${pair#*:}
    {
        echo '#!/usr/bin/env bash'
        printf 'exec %q %q ' "$ROOT/build/deps/zig/zig" "$command"
        case "$name" in cc|cxx) printf '%s ' '-target x86_64-linux-musl';; esac
        printf '"$@"\n'
    } > "$OUT/$name"
    chmod +x "$OUT/$name"
done
# test_util_impl.cc's initialization without Google Benchmark, which none
# of the selected targets uses. Same gtest/Abseil flags and SIGPIPE masking.
# No RunfilePath stub: any unexpected dependency on Bazel runfiles must fail
# to link and appear in not-built.txt, rather than silently change a test.
cat > "$OUT/adapter.cc" <<'CPP'
#include <signal.h>
#include "gtest/gtest.h"
#include "absl/flags/parse.h"
#include "test/util/logging.h"
namespace gvisor { namespace testing {
void SetupGvisorDeathTest() {}
void TestInit(int* argc, char*** argv) {
  ::testing::InitGoogleTest(argc, *argv);
  ::absl::ParseCommandLine(*argc, *argv);
  struct sigaction sa = {};
  sa.sa_handler = SIG_IGN;
  TEST_CHECK(sigaction(SIGPIPE, &sa, nullptr) == 0);
}
int RunAllTests() { return RUN_ALL_TESTS(); }
}}
CPP
PYTHONPATH=tools/harness python3 - "$OUT" <<'PY'
from pathlib import Path
import json
import sys
from lumen_harness.gvisor import build_rules

out = Path(sys.argv[1])
src = out / 'src/gvisor'
rules = {}
for package in ('test/util', 'test/syscalls/linux'):
    rules.update(build_rules((src / package / 'BUILD').read_text(), package))

def family(path):
    name = Path(path).name
    for prefix in ('socket_unix_', 'socket_ip_tcp_', 'socket_ip_udp_', 'socket_generic'):
        if name.startswith(prefix):
            return prefix.rstrip('_')
    # These two instantiate socket_unix.cc/socket_unix_cmsg.cc too.
    if name in ('socket_abstract.cc', 'socket_filesystem.cc'):
        return 'socket_unix'
    if name in ('epoll.cc', 'eventfd.cc', 'timerfd.cc'):
        return Path(name).stem
    return None

selected = {k: family(v.sources[0]) for k, v in rules.items()
            if v.kind == 'binary' and len(v.sources) == 1 and family(v.sources[0])}
assert len(selected) == 34, f'pinned BUILD changed: expected 34 binaries, got {len(selected)}'

def closure(key, seen):
    if key in seen:
        return set(), set()
    seen.add(key)
    if key.startswith('@com_google_absl//'):
        name = key.rsplit(':', 1)[-1] if ':' in key else key.rsplit('/', 1)[-1]
        name = {'flag': 'flags', 'parse': 'flags_parse', 'container': 'algorithm_container'}.get(name, name)
        return set(), {'absl::' + name}
    if key == '@bazel_tools//tools/cpp/runfiles':
        return set(), set()
    rule = rules[key]
    if rule.unsupported:
        raise ValueError(f'{key}: selected rule needs unsupported attributes {rule.unsupported}')
    sources = {p for p in rule.sources if Path(p).name not in ('test_util_impl.cc', 'test_util_runfiles.cc')}
    libs = set()
    for dep in rule.deps:
        s, lib = closure(dep, seen)
        sources |= s
        libs |= lib
    return sources, libs

sets = {k: closure(k, set()) for k in selected}
# Bazel's malloc override preserves errno across new/delete, including test
# assertion allocations. Keep it instead of relying on libc++'s allocator.
allocator = 'test/util/errno_safe_allocator.cc'
all_sources = set().union(*(v[0] for v in sets.values())) | {allocator}
lines = [
    'cmake_minimum_required(VERSION 3.20)',
    'project(gvisor_spike LANGUAGES C CXX)',
    'set(CMAKE_CXX_STANDARD 20)', 'set(CMAKE_CXX_STANDARD_REQUIRED ON)',
    'set(BUILD_SHARED_LIBS OFF)', 'set(ABSL_BUILD_TESTING OFF)',
    'set(ABSL_PROPAGATE_CXX_STD ON)',
    f'add_subdirectory("{out}/src/abseil" abseil EXCLUDE_FROM_ALL)',
    f'add_subdirectory("{out}/src/googletest" googletest EXCLUDE_FROM_ALL)',
    f'include_directories("{src}")',
]
for p in sorted(all_sources):
    obj = 'obj_' + Path(p).stem
    lines += [f'add_library({obj} OBJECT "{src}/{p}")',
              f'target_link_libraries({obj} PRIVATE gmock absl::flags_parse absl::str_format absl::synchronization)']
lines += ['add_library(adapter OBJECT adapter.cc)',
          'target_link_libraries(adapter PRIVATE gmock absl::flags_parse)']
for key, (sources, libs) in sets.items():
    name = key.split(':')[1]
    # OBJECT libraries preserve Bazel alwayslink test registrations; a normal
    # archive would discard unreferenced TEST_P bodies and falsely shrink coverage.
    extra = {allocator} if rules[key].allocator else set()
    objects = ' '.join(f'$<TARGET_OBJECTS:obj_{Path(p).stem}>' for p in sorted(sources | extra))
    lines += [f'add_executable({name} $<TARGET_OBJECTS:adapter> {objects})',
              f'target_link_libraries({name} PRIVATE gmock absl::flags_parse ' + ' '.join(sorted(libs)) + ')']
(out / 'CMakeLists.txt').write_text('\n'.join(lines) + '\n')
(out / 'targets.json').write_text(json.dumps({k.split(':')[1]: v for k, v in selected.items()}, indent=2) + '\n')
(out / 'targets.txt').write_text('\n'.join(k.split(':')[1] for k in selected) + '\n')
PY
# A parent make's jobserver and dry-run flags do not belong to this build.
unset MAKEFLAGS MFLAGS MAKELEVEL MAKEOVERRIDES
# Zig 0.15.2 rejects CMake's newer --dependency-file linker option; Ninja
# still tracks the objects and libraries through its generated build graph.
# Release supplies -O3 -DNDEBUG. A zero PT_GNU_STACK matches GNU ld and
# preserves musl's default pthread stack size instead of Zig's larger hint.
cmake -S "$OUT" -B "$OUT/cmake" -G Ninja \
    -DCMAKE_SYSTEM_NAME=Linux -DCMAKE_SYSTEM_PROCESSOR=x86_64 \
    -DCMAKE_C_COMPILER="$OUT/cc" -DCMAKE_CXX_COMPILER="$OUT/cxx" \
    -DCMAKE_AR="$OUT/ar" -DCMAKE_RANLIB="$OUT/ranlib" \
    -DCMAKE_LINK_DEPENDS_USE_LINKER=FALSE \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_C_FLAGS="-fno-sanitize=all -ffile-prefix-map=$OUT/src=. -ffile-prefix-map=$OUT=." \
    -DCMAKE_CXX_FLAGS="-fno-sanitize=all -ffile-prefix-map=$OUT/src=. -ffile-prefix-map=$OUT=." \
    -DCMAKE_EXE_LINKER_FLAGS='-static -Wl,-z,stack-size=0 -Wl,--strip-debug' \
    > "$OUT/configure.log" 2>&1 || { tail -30 "$OUT/configure.log" >&2; exit 1; }
: > "$OUT/tests.txt"
: > "$OUT/not-built.txt"
while IFS= read -r name; do
    # A failed rebuild cannot retain an earlier executable as a success.
    rm -f "$OUT/cmake/$name"
    if cmake --build "$OUT/cmake" --parallel "$JOBS" --target "$name" > "$OUT/logs/$name.log" 2>&1; then
        echo "$name" >> "$OUT/tests.txt"
        echo "build-gvisor-tests: built $name"
    else
        why=$(grep -m1 -E 'fatal error:|error:|undefined symbol:' "$OUT/logs/$name.log" || true)
        echo "$name # ${why:-build failed}; see logs/$name.log" >> "$OUT/not-built.txt"
        echo "build-gvisor-tests: not built $name"
    fi
done < "$OUT/targets.txt"
PYTHONPATH=tools/harness python3 - "$OUT" "$START" <<'PY'
from pathlib import Path
import hashlib
import json
import subprocess
from lumen_harness.libctest import read_elf
from lumen_harness.gvisor import host_identity
import sys
import time
out = Path(sys.argv[1])
targets = json.loads((out / 'targets.json').read_text())
built = (out / 'tests.txt').read_text().splitlines()
report = {**host_identity(),
          'zig_version': subprocess.check_output(['build/deps/zig/zig', 'version'], text=True).strip(),
          'cmake_version': subprocess.check_output(['cmake', '--version'], text=True).splitlines()[0],
          'ninja_version': subprocess.check_output(['ninja', '--version'], text=True).strip(),
          'elapsed_seconds': int(time.time()) - int(sys.argv[2]), 'families': {}, 'binaries': {}}
for name, family in targets.items():
    counts = report['families'].setdefault(family, {'built': 0, 'not_built': 0, 'bytes': 0})
    if name not in built:
        counts['not_built'] += 1
        continue
    data = (out / 'cmake' / name).read_bytes()
    elf = read_elf(out / 'cmake' / name)
    assert elf.interp is None and not elf.dynamic, f'{name}: PT_DYNAMIC or PT_INTERP'
    counts['built'] += 1
    counts['bytes'] += len(data)
    report['binaries'][name] = {'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()}
(out / 'build-report.json').write_text(json.dumps(report, indent=2) + '\n')
print(json.dumps(report['families'], indent=2))
print(f"build-gvisor-tests: {len(built)}/{len(targets)} built in {report['elapsed_seconds']} s")
if not built:
    sys.exit('build-gvisor-tests: no executables built; see not-built.txt')
PY
