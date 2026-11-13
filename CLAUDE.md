# Lumen: notes for agents and contributors

Lumen is a hobby/teaching OS in Rust for x86-64 UEFI (Limine, QEMU q35 first).
Read `docs/ARCHITECTURE.md` before changing interfaces; `docs/ROADMAP.md` lists
milestones with test-based exit criteria; `docs/STATUS.md` is generated.

## Commands

```bash
make deps            # pinned Limine + OVMF into build/deps (sha256-checked)
make build           # both kernels: build/lumen-kernel, build/lumen-kernel-ktest
make run             # QEMU with a window, serial on your terminal (Ctrl-A x quits; HEADLESS=1)
make debug           # QEMU paused + gdb attached, stopped at kmain
make test            # everything CI runs: host tests, flag audit, QEMU scenarios
make test-host       # cargo test + harness self-tests (fast, no QEMU)
make test-scenarios  # headless QEMU scenarios incl. in-kernel ktests, in parallel (SCENARIO_JOBS=0: serially)
make audit           # proves no SSE/AVX, no red zone, stack probes, kernel code model, image layout
make lint fmt        # clippy -D warnings + ruff; rustfmt + ruff format
make status-check    # feature gate: every feature in tests/features/ passes (CI runs this)
make status          # regenerate docs/STATUS.md from the last test results (on main)
make golden          # re-record golden screenshots (look at them before committing!)
```

One scenario: `uv run pytest tests/scenarios/test_crash.py -k pf -v`.
Failing scenarios keep `build/test-artifacts/<module>/<test>/` (e.g.
`test_crash/test_fault_produces_symbolized_report_pf_/`) with `serial.log`,
`qemu.log` (`-d int,cpu_reset,guest_errors`), `cmdline.txt`, `environment.txt` (QEMU version,
accelerator, OVMF version) and screenshots. The interrupt
trace (`-d int`) is only recorded under TCG: locally on macOS always, and in CI
by re-running failed scenarios with `LUMEN_ACCEL=tcg` (CI runs KVM first,
and keeps the re-run's artifacts and `ktest.tap` apart in
`build/test-artifacts-tcg/` via `LUMEN_ARTIFACTS_DIR`). Don't set it on a
first run: the ktests' TAP then goes there too, not to
`build/test-results/ktest.tap`, so `make status` shows them as not run (and
says why).

Prerequisites: rustup (the toolchain installs itself from
`rust-toolchain.toml`), `qemu-system-x86_64`, `xorriso`, `uv`, `make`, `curl`, and `gdb` for `make debug`
(its scenario is skipped without it).

## Hard rules

Each rule exists because breaking it caused a real bug in v1.

1. **"Verified" means an automated test passes in CI.** Never call a feature
   working in docs, a PR or an issue unless a test checks it. Add the test and
   its `tests/features/<area>.toml` entry in the same PR. (v1's docs called pipes and
   cross-directory `mv` "verified" while both were broken.)
2. **Never hand out memory you don't own.** The frame allocator is built from
   the bootloader map and explicitly reserves the kernel image, boot
   structures, framebuffer, ACPI tables, page tables and the kernel ELF file.
   A test asserts it never returns a reserved page. (v1 handed out its own
   `.text` as a RAM disk.)
3. **Compiler flags are tested, not assumed.** `make audit` checks the built
   ELF and the LLVM IR. Never weaken it to make a build pass.
4. **No fixed-address stacks.** Every stack comes from the allocator and has a
   guard page (`mm::stack`). The only exception is early boot, which runs on
   Limine's stack (and #DF, NMI and #MC with it, no IST yet) until `kmain`
   switches right after `mm::init`, and likewise each other CPU from
   `smp::ap_entry` until `enter_kernel_space`. Nothing in that stretch may
   fault: its IST stacks are mapped only in the kernel's tables (an NMI, #DF
   or #MC there is a triple fault), and any other exception loses its report
   (one `!!!` line). CPUs past `MAX_CPUS` halt for good on their Limine
   stack.
5. **A real heap with working `free`**, plus in-kernel fragmentation and reuse
   tests.
6. **Exception handling comes first.** GDT/IDT setup is the first thing
   `kmain` does (only a bare `rdtsc`, the boot timeline's origin, comes
   before it), and the first thing `smp::ap_entry` does on each other CPU;
   keep it that way.
7. **Reproducible toolchain.** Rust is pinned (stable only, no `#![feature]`),
   Limine and OVMF are pinned by sha256, and Python deps by `uv.lock`. Bump
   them deliberately, in their own PR.
8. **No debug-print litter.** Use `log::{error,warn,info,debug,trace}!` with
   `target: "<subsystem>"` (or the default module tag). Remove temporary prints
   before committing. Anchor `.gitignore` patterns (`/build/`, not `build/`).
9. **Fix root causes, not symptoms.** If a fix involves cache flushes,
   disabling optimizations or "timing", stop and gather evidence: the crash
   report, `qemu.log` (`-d int,cpu_reset`), and the fault address mapped to a
   symbol.
10. **One status doc, generated from tests** (`docs/STATUS.md`). No
    handoff/summary markdown files. Session handoffs go in GitHub issues and PRs.

## How to pick up work

1. Take a GitHub issue labelled with the current milestone (e.g. `m1`) and a
   component (`area:*`). Assign it to yourself and comment that you're on it.
2. Work on a branch in your own worktree:
   `git worktree add ../lumen-<issue> -b <type>/<issue>-<slug>` (e.g. `feat/12-frame-allocator`).
3. Implement with tests: an in-kernel `ktest!`, a scenario in `tests/scenarios/`,
   and/or host tests in a `no_std` crate under `crates/`. Prefer putting pure
   logic in a host-testable crate.
4. Add your feature to `tests/features/<area>.toml` (one file per area, so
   parallel PRs don't collide). Run `make lint test status-check` locally.
   **Don't commit `docs/STATUS.md` in feature PRs**: CI's feature gate
   (`make status-check`) fails the PR if any listed feature isn't passing, and
   the committed STATUS.md is refreshed on main (`make test status`) by the
   lead. Because every merged commit passed the gate, that file can lag behind
   (missing new rows) but can never claim something broken works.
5. Open a PR that says `Fixes #<n>`, lists what is tested (by test name), and
   notes anything not yet tested. CI must be green; a reviewer agent or a human
   reviews before merge.

Interfaces shared across issues (syscall numbers, `FileSystem`, driver traits,
the automation tree) are defined in `docs/ARCHITECTURE.md`. Changing one is its
own PR that updates that doc first.

## Conventions

- **Commits:** Conventional Commits (`feat(mm): ...`, `fix(crash): ...`,
  `test: ...`, `docs: ...`, `ci: ...`, `chore: ...`). Reference issues with
  `Fixes #n`. **No AI attribution lines** (no `Co-Authored-By` for AI tools, no
  "Generated with" footers) in commits.
- **Rust:** edition 2024, `unsafe_op_in_unsafe_fn` denied, and every `unsafe`
  block has a `// SAFETY:` comment. Kernel code is `no_std`; `alloc` (`Box`,
  `Vec`) works once `mm::init` has set up the heap. Code holding the
  page-table or frame-allocator lock must not allocate (the heap takes them).
- **Kernel tests:** `crate::ktest!(fn name() { ...; Ok(()) });` next to the
  code, using `kassert!` and `kassert_eq!`. No central list; tests register
  through the `.ktests` linker section.
- **Logs** that tests match on (e.g. `INFO  boot: boot complete`) are an
  interface: change the test in the same PR. On serial each line starts
  with a timestamp (`[    0.139288]`, or `[    ?.??????]` before the TSC is
  calibrated): match the rest of the line, not its start.
- **QEMU exit codes:** 33 pass, 35 test failure, 37 crash report; anything else
  (including 0 from a triple fault with `-no-reboot`) is a failure.
- **Golden images** live in `tests/golden/`. Re-record with `make golden` only
  when the change to the screen is intended, and open the PNG to check it.

## Map

- `kernel/src/main.rs`: boot sequence. `boot.rs`: the Limine requests.
  `cmdline.rs`: kernel command line (flags and `key=value`, e.g.
  `lumen.log=debug`). `sync.rs`: `SpinLock` (interrupts off while held).
  `smp.rs`: starting the other CPUs, `run_on`.
  `arch/x86_64/`: GDT/TSS (per CPU), IDT, entry
  stubs and the vector map, `percpu.rs` (GS base, CPU index), `irq.rs` (nested interrupt disable), local and
  I/O APIC, 8259 and PIT registers, serial, IST stacks. `crash.rs`: crash
  reporter. `ktest.rs`: test runner. `kmon.rs`: serial debug monitor (until
  the M2 shell). `fb.rs`:
  framebuffer and console. `klog.rs`: logger. `symbols.rs`: symbol lookup.
  `mm/frame.rs`: physical frame allocator, its reservations, `OwnedFrame`.
  `mm/window.rs`: all-or-nothing mapping of fresh frames into a window
  (heap and stacks), through `paging::map` with a `Window` token.
  `mm/heap.rs`: the `#[global_allocator]`, growing in the heap window.
  `mm/stack.rs`: kernel stacks with guard pages; `arch/x86_64/stacks.rs`
  holds their registry (backtraces, overflow reports).
  `mm/paging.rs`: the kernel's page tables (HHDM, W^X image, map/unmap,
  MMIO window, crash-report walk). `arch/x86_64/paging.rs`: CR3, TLB, NX/WP,
  PAT. `trace.rs`: trace rings (one per CPU), event kinds, `ktrace!`, the boot timeline
  (`Timeline`). `acpi.rs`: ACPI
  discovery (CPUs, LAPIC, I/O APICs, HPET) and the firmware-table reader.
  `interrupts.rs`: 8259s masked, local APIC on, I/O APIC inputs masked.
  `time.rs`: TSC and LAPIC timer calibration, the boot origin
  (`mark_boot`, `since_boot_ns`), `now_ns`, `sleep`, `ticks`.
- `crates/lumen-elf`: ELF symbol-table reader (host-tested).
- `crates/lumen-mm`: frame-allocator logic (bitmap), typed addresses, the
  page-table format and mapper (host-tested, incl. a model comparison), the
  window tokens that confine `paging::map`, and the kernel-stack layout and
  lock-free stack registry.
- `crates/lumen-heap`: heap allocator logic (address-ordered free list,
  coalescing; host-tested, incl. a randomized stress test against a shadow map).
- `crates/lumen-trace`: lock-free trace ring (host-tested, incl. threads).
- `crates/lumen-time`: timekeeping arithmetic: calibration, tick/ns
  conversion, timer counts, log time formats (`Stamp`, `Millis`)
  (host-tested).
- `crates/lumen-acpi`: ACPI table parser: RSDP, XSDT/RSDT, MADT, HPET
  (host-tested on handcrafted tables).
- `tools/harness/lumen_harness/`: `qemu.py` (the only place QEMU command lines
  are built), `qmp.py`, `tap.py`, `image.py`, `audit.py`, `status.py`,
  `crash.py` (crash-report parsing).
- `tests/scenarios/`: QEMU tests. `tests/harness/`: harness self-tests.
  `tests/features/*.toml`: feature → test map (the CI feature gate and STATUS.md).
