#!/usr/bin/env bash
# TEMPORARY (#61, reverted before merge): boot timeline and a 32 KiB serial
# write bench, origin/main against this branch, on the same runner.
set -euo pipefail
BASE=/tmp/lumen-base
git fetch -q --depth=1 origin main
git worktree add -q --detach "$BASE" FETCH_HEAD
mkdir -p "$BASE/build" && cp -R build/deps "$BASE/build/"
bench='
crate::ktest!(
    fn zz_serial_throughput_bench() {
        let mut buf = [b'"'"'x'"'"'; 64];
        buf[63] = b'"'"'\n'"'"';
        let t0 = crate::time::now_ns();
        crate::klog::with_line_lock(|| for _ in 0..512 { write_bytes(&buf) });
        let us = (crate::time::now_ns() - t0) / 1000;
        crate::ktest::diag(format_args!("BENCH 32768 bytes in {us} us"));
        Ok(())
    }
);'
for tree in . "$BASE"; do
  printf '%s\n' "$bench" >> "$tree/kernel/src/arch/x86_64/serial.rs"
  (cd "$tree" && make iso >/dev/null && tools/mkiso.sh build/lumen-kernel-ktest build/bench.iso lumen.qemu_exit >/dev/null)
done
export PYTHONPATH=tools/harness
for tree in "$BASE" .; do
  echo "== $tree"
  uv run --frozen python tools/measure_timeline.py timeline "$tree/build/lumen-qemutest.iso" 7
  uv run --frozen python tools/measure_timeline.py bench "$tree/build/bench.iso" 5
done
git checkout -- kernel/src/arch/x86_64/serial.rs
