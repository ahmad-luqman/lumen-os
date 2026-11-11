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
make test-scenarios  # headless QEMU scenarios incl. in-kernel ktests (~20 s)
make audit           # proves no SSE/AVX, no red zone, kernel code model
make lint fmt        # clippy -D warnings + ruff; rustfmt + ruff format
make status-check    # feature gate: every feature in tests/features/ passes (CI runs this)
make status          # regenerate docs/STATUS.md from the last test results (on main)
make golden          # re-record golden screenshots (look at them before committing!)
```

One scenario: `uv run pytest tests/scenarios/test_crash.py -k pf -v`.
Failing scenarios keep `build/test-artifacts/<test>/` with `serial.log`,
`qemu.log` (`-d int,cpu_reset,guest_errors`) and screenshots. The interrupt
trace (`-d int`) is only recorded under TCG: locally on macOS always, and in CI
by re-running failed scenarios with `LUMEN_ACCEL=tcg` (CI runs KVM first).

Prerequisites: rustup (the toolchain installs itself from
`rust-toolchain.toml`), `qemu-system-x86_64`, `xorriso`, `uv`, `make`, `curl`.

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
   guard page. (Temporary M0 exception: the boot stack and IST stacks, until M1.)
5. **A real heap with working `free`**, plus in-kernel fragmentation and reuse
   tests.
6. **Exception handling comes first.** GDT/IDT setup is the first thing
   `kmain` does; keep it that way.
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
  block has a `// SAFETY:` comment. Kernel code is `no_std` with no `alloc`
  until the M1 heap lands.
- **Kernel tests:** `crate::ktest!(fn name() { ...; Ok(()) });` next to the
  code, using `kassert!` and `kassert_eq!`. No central list; tests register
  through the `.ktests` linker section.
- **Logs** that tests match on (e.g. `INFO  boot: boot complete`) are an
  interface: change the test in the same PR.
- **QEMU exit codes:** 33 pass, 35 test failure, 37 crash report; anything else
  (including 0 from a triple fault with `-no-reboot`) is a failure.
- **Golden images** live in `tests/golden/`. Re-record with `make golden` only
  when the change to the screen is intended, and open the PNG to check it.

## Map

- `kernel/src/main.rs`: boot sequence. `arch/x86_64/`: GDT/TSS, IDT and entry
  stubs, serial, IST stacks. `crash.rs`: crash reporter. `ktest.rs`: test
  runner. `kmon.rs`: serial debug monitor (until the M2 shell). `fb.rs`:
  framebuffer and console. `klog.rs`: logger. `symbols.rs`: symbol lookup.
- `crates/lumen-elf`: ELF symbol-table reader (host-tested).
- `tools/harness/lumen_harness/`: `qemu.py` (the only place QEMU command lines
  are built), `qmp.py`, `tap.py`, `image.py`, `audit.py`, `status.py`.
- `tests/scenarios/`: QEMU tests. `tests/harness/`: harness self-tests.
  `tests/features/*.toml`: feature → test map (the CI feature gate and STATUS.md).
