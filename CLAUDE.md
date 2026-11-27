# Lumen: notes for agents and contributors

Lumen is a hobby/teaching OS in Rust for x86-64 UEFI (Limine, QEMU q35 first).
Read `docs/ARCHITECTURE.md` before changing interfaces; `docs/ROADMAP.md` lists
milestones with test-based exit criteria; `docs/STATUS.md` is generated;
`docs/MAP.md` says where everything lives (read an area's entry first).

## Commands

```bash
make deps            # pinned Limine + OVMF + Alpine busybox + Alpine minirootfs into build/deps (sha256-checked)
make build           # the apps (build/apps/<name>.wasm, test apps build/apps/tests/), Linux binaries, then both kernels
make rootfs          # deterministic Alpine minirootfs ext4 image (build/alpine-rootfs.ext4) + M7/M8 Linux binaries
make deps-dev        # pinned Alpine developer packages (tools/alpine-packages.lock, sha256-checked)
make rootfs-dev      # deterministic Alpine developer image (build/alpine-dev.ext4: gcc, g++, make, git, sqlite3, vim)
make deps-zig        # pinned zig C cross-compiler for host-built Linux test binaries (sha256-checked)
make libc-test       # fetch musl libc-test (pinned commit) and build it with zig into build/libc-test/
make app NAME=foo    # a new app: apps/foo copied from apps/template (tools/new-app.sh); `make build` bundles it
make run             # QEMU with a window, serial on your terminal (Ctrl-A x quits; HEADLESS=1; SOUND=1 for beep/play; DESKTOP=1 starts the desktop; ROOTFS=1 attaches build/alpine-rootfs.ext4 via virtio-blk)
make debug           # QEMU paused + gdb attached, stopped at kmain
make test            # everything CI runs: host tests, flag audit, QEMU scenarios
make test-host       # cargo test + harness self-tests (fast, no QEMU)
make test-scenarios  # headless QEMU scenarios incl. in-kernel ktests, in parallel (SCENARIO_JOBS=0: serially)
make audit           # proves no SSE/AVX, no red zone, stack probes, kernel code model, image layout
make lint fmt        # clippy -D warnings + ruff; rustfmt + ruff format
make status-check    # feature gate: every feature in tests/features/ passes (CI runs this)
make status          # regenerate docs/STATUS.md from the last test results (on main)
make golden          # re-record golden screenshots (look at them before committing!)
make census          # CI's red first attempts over the last week and the tests behind them (DAYS=7; needs gh)
```

One scenario: `uv run pytest tests/scenarios/test_crash.py -k pf -v`.
Failing scenarios keep `build/test-artifacts/<module>/<test>/` (e.g.
`test_crash/test_fault_produces_symbolized_report_pf_/`) with `serial.log`,
`qemu-stderr.log` (QEMU's own warnings), `qemu.log` (`-d int,cpu_reset,guest_errors`), `cmdline.txt`, `environment.txt` (QEMU version,
accelerator, OVMF version) and screenshots. The interrupt
trace (`-d int`) is only recorded under TCG: locally on macOS always, and in CI
by re-running failed scenarios with `LUMEN_ACCEL=tcg` (CI runs KVM first,
and keeps the re-run's artifacts and `ktest.tap` apart in
`build/test-artifacts-tcg/` via `LUMEN_ARTIFACTS_DIR`). Don't set it on a
first run: the ktests' TAP then goes there too, not to
`build/test-results/ktest.tap`, so `make status` shows them as not run (and
says why).

Prerequisites: rustup (the toolchain installs itself from
`rust-toolchain.toml`), `qemu-system-x86_64`, `xorriso`, `e2fsprogs` (`mke2fs`
and `e2fsck` in the same directory; on macOS `brew install e2fsprogs` is
keg-only and discovered via `/opt/homebrew/opt/e2fsprogs/sbin`,
`/usr/local/opt/e2fsprogs/sbin`, or `LUMEN_E2FSPROGS_SBIN` so a partial
`mke2fs`-only `$PATH` entry like Android SDK never hijacks it; on Ubuntu/Debian
`apt-get install e2fsprogs`), `uv`, `make`, `curl`, and `gdb` for `make debug`
(its scenario is skipped without it; a `LUMEN_GDB` that can't be found fails it).

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
   `kmain` does (only a bare `rdtsc`, the boot timeline's origin, and
   COM1's setup in `arch::init_early`, which takes no lock and can't fault,
   come before it), and the first thing `smp::ap_entry` does on each other CPU;
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
   parallel PRs don't collide), and update the area's `docs/MAP.md` entry. Run `make lint test status-check` locally.
   **Don't commit `docs/STATUS.md` in feature PRs**: CI's feature gate
   (`make status-check`) fails the PR if any listed feature isn't passing, and
   the committed STATUS.md is refreshed on main (`make test status`) by the
   lead. Because every merged commit passed the gate, that file can lag behind
   (missing new rows) but can never claim something broken works.
5. Open a PR that says `Fixes #<n>`, lists what is tested (by test name), and
   notes anything not yet tested. CI must be green; a reviewer agent or a human
   reviews before merge.

Interfaces shared across issues (syscall numbers, VFS interfaces, driver
traits, the automation tree) are defined in `docs/ARCHITECTURE.md`. Changing
one is its own PR that updates that doc first.

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
  (including 0 from a triple fault with `-no-reboot`) is a failure. The one
  exception: 0 in the power-off scenarios, with a QMP `SHUTDOWN` event whose
  reason is `guest-shutdown` (a triple fault's is `guest-reset`).
- **Tests on a busy host.** A guest's clocks run on while the host stalls
  its vCPU, so a loaded laptop or CI runner adds time to anything timed
  (measured in #139, #165, #170 and #171). A new or changed scenario or
  ktest asserts only what the host can't change (older checks come in
  line when they are next changed):
  - a floor (the printed bound is at least the injected stall);
  - a decision that follows from the numbers the kernel printed
    (`lumen_harness.calibration`);
  - an effect confirmed before the next step (`gui.py`'s steps, `mouse hold`).

  Not a ceiling or a % range on something timed, nor a "no WARN at boot"
  that a slow boot trips. Exact numbers and texts for given inputs belong in
  host tests or ktests over made-up inputs. A claim that only means
  something within a tolerance (test_ktest's stalled-within-tolerance runs)
  keeps its precondition, and its PR says it is at risk on a busy host.
  Reviewers: don't ask a scenario for timing ranges. A mutant that only a
  slower result would show goes in the PR's not-tested notes, or gets a
  deterministic seam.
- **Golden images** live in `tests/golden/`. Re-record with `make golden` only
  when the change to the screen is intended, and open the PNG to check it.

## Map

The full map is [`docs/MAP.md`](docs/MAP.md): read the entry for the area
you are changing first, and update it in the same PR. In short:

- `kernel/src/`: the kernel. `main.rs` boot sequence; `arch/x86_64/` CPU,
  interrupts, syscall entry; `mm/` frames, heap, stacks, paging, user
  address spaces, TLB; `sched.rs`, `smp.rs`, `sync.rs`; `shell/`, `term.rs`,
  `vt.rs`, `desktop.rs`, `ui.rs`, `taskbar.rs` (consoles and the desktop);
  `pci/`, `drivers.rs`, `virtio.rs` the PCI/virtio driver core, `blk/`,
  and per-device files (`keyboard.rs`, `mouse.rs`, `sound.rs`, `rng.rs`,
  `serial_in.rs`); `app/` WASM
  apps (M5); `vfs/` the VFS, ext4, tmpfs, devfs, devpts, procfs (M7);
  `user.rs` ring-3 entry and exit, user copies, syscall dispatch; `linux/`
  the Linux ABI: processes, signals, futexes, pipes, timers and syscall
  handlers (M6 to M8); `tty.rs` line disciplines; `ktest.rs`,
  `crash.rs`, `trace.rs`, `klog.rs`.
- `crates/lumen-*`: host-tested `no_std` logic the kernel and apps use
  (memory, VFS, block cache, process tree, signals, futexes, pipes, tty,
  window manager, toolkit, graphics, input, time, trace, ELF, PCI, ACPI,
  PNG, fonts, the app API and the apps' own logic).
- `vendor/ext4plus`: the ext4 crate with Lumen's fixes, each marked
  `Lumen (#n)`.
- `apps/`: the WASM apps and their SDK, a separate cargo workspace.
- `tools/`: the test harness (`tools/harness/lumen_harness/`), rootfs and
  dependency scripts, the Linux test programs (`tools/linux-m7/`,
  `tools/linux-m8/`).
- `tests/`: QEMU scenarios (`scenarios/`), harness self-tests (`harness/`),
  the feature map (`features/*.toml`), libc-test's expected failures
  (`conformance/`), golden screenshots (`golden/`), the syscall census
  (`linux/`).
