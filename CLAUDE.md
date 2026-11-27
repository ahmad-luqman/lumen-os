# Lumen: notes for agents and contributors

Lumen is a hobby/teaching OS in Rust for x86-64 UEFI (Limine, QEMU q35 first).
Read `docs/ARCHITECTURE.md` before changing interfaces; `docs/ROADMAP.md` lists
milestones with test-based exit criteria; `docs/STATUS.md` is generated.

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
   parallel PRs don't collide). Run `make lint test status-check` locally.
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

- `kernel/src/main.rs`: boot sequence. `boot.rs`: the Limine requests
  (and, by hand, the TSC-frequency and bootloader-performance ones;
  `before_kmain`).
  `cmdline.rs`: kernel command line (flags and `key=value`, e.g.
  `lumen.log=debug`). `sync.rs`: `SpinLock` (interrupts off while held; a wait recorded as an X-ray `lock` event)
  and `SleepMutex` (interrupts left on, its waiters asleep: for code that may block, e.g. the VFS's).
  `smp.rs`: starting the other CPUs, `run_on`. `sched.rs`: kernel threads
  (each named by a `lumen_trace::ThreadName`, 16 bytes inline, #276; per-CPU run queues, preemption on the timer, `sleep`, `WaitQueue`
  with or without a deadline, `wait_interruptible` and `wait_killable` returning `Wake` on pending signals (`Thread::sigpending`, `fatal_pending`, `kick`, #373),
  `spawn`/`join`, `scope`: threads borrowing from the caller, joined before it returns;
  SMP migration and work-stealing (`spawn_migratable`, `spawn_fork_child`, `affinity`/`set_affinity`, `online_mask`, `nice`/`set_nice`, `cpu_time`, `comm`/`set_current_comm`, #377, #380, #391);
  per-thread ring-3 state (`UserSpaceGuard`, `current_address_space`/`replace_user_space`/`detach_user_space`, `current_user_thread`, `snapshot_user_fpu`/`restore_signal_fpu`, `set_restart_block`/`take_restart_block`);
  each run-queue change an X-ray `runq` event, `run_queue`: a queue's length both ways); `arch/x86_64/context.rs` switches threads.
  `arch/x86_64/`: GDT/TSS (per CPU), IDT (`TLB_SHOOTDOWN` on vector `0x3e`, #376; M6's deferred `#DB` handling, `DEFERRED_LSTAR_DB`/`DEFERRED_STUB_DB`), entry
  stubs and the vector map, `percpu.rs` (GS base, CPU index, `loaded_space`, #376, `DebugTrap` pending debug traps, #384), `irq.rs` (nested interrupt disable),
  `syscall.rs` (`SyscallFrame`, `SyscallReturn`, `unwind_to_kernel`), `fpu.rs` (M6's `FpuMode`, `UserFpuState`; `install_signal`, `signal_xfeature_layout`, #384),
  local and I/O APIC, 8259 and PIT registers, serial, IST stacks. `user.rs`: ring-3 entry/exit (`run_elf_process`, `UserExit`, `UserFault`), fault-safe copies (`copy_from_user`, `copy_to_user`, `copy_to_user_partial`, `copy_cstr_from_user`, `.lumen_fixup`), non-faulting futex atomics (`read_u32_nofault`, `cmpxchg_u32`, `fault_in_writable`, `.lumen_fixup_nofault`, #373, #383), `handle_user_exception` and `dispatch_syscall`. `crash.rs`: crash
  reporter. `ktest.rs`: test runner (`lumen.ktest=` runs only the tests with given name prefixes). `script.rs`: rhai scripts (the shell's
  `rhai`; built-in scripts in `script/`). `shell/`: the shells, one on serial
  (plain text unless `lumen.ansi`), one on each virtual terminal (keyboard) and one in each terminal
  window (`run_in_window`, #155): `mod.rs` the consoles (`Console::place`: anywhere, a virtual terminal
  or a window; `closed`) and `Ctx` (keys through `poll_key`/`wait_key`, the screen
  through `canvas`), their commands (`commands.rs`: each row carries its
  help line; the system ones in `sysinfo.rs`, `ps` among them: the Linux
  processes and their threads, #372, and `mem` showing the frame low-water mark `min_free`, #394), pipelines (`pipe.rs`: `cpus |
  where busy > 50`), deliberate faults (`crash.rs`) and `trace runq`/`trace xray`
  (`xray.rs`: run queues by X-ray and the scheduler, `park` holding the shell's CPU still, `spin` keeping
  threads spinning on the last CPU until stopped, #208). `fb.rs`:
  framebuffer pixels, the header (`draw_header`, again when the desktop is left) and the crash screen. `term.rs`: the
  terminals on the framebuffer (four virtual ones, one shown: log lines on the first, the screen shells, scrollback, `view_text`, and
  `Canvas`: the pixels below the header for one command, the terminals paused meanwhile;
  and the mouse pointer, a save-under arrow that drawing over it takes off and puts back;
  `take_screen`: the desktop's `ScreenCanvas`, the whole screen, a row per hold of the screen's lock;
  `leave_desktop`; `Cells` over `Pixels`: a cell's pixels, the console's and the terminal windows', drawn by
  `lumen_term::render::Renderer`; `view_text_from`). `vt.rs`: the keyboard's keys to the shown virtual terminal's shell, Alt+F1 to F4 switching;
  the decoding pass (`decode_pending`), `desktop_shown()` (keys the desktop's alone while it is), `cut()` (a
  copy of where each pause and resume falls in the input rings, with the `pause_owns_*`/`after_resume_*`
  predicates) and `enter_desktop`.
  `desktop.rs`: the desktop (#153): the `compositor` thread (the back buffer, damage-only presents, the
  pointer drawn on the screen only, both input rings through `lumen_wm::Wm`, the cut), windows
  (`open_window`, `open_window_at`, `WindowHandle`: an owner's surface and a committed one, `commit`,
  `next_event`/`wait_event`/`wait_event_until`, a `Waker` for another thread, `Desktop::without_screen` for the
  ktests; `publish`: a toolkit's tree), each window's root node, the two demo windows, panels
  (#157: `open_panel`, the taskbar: frameless, in front, never focused, left out of `windows`), minimized
  windows (not composed; `minimize_window`/`restore_window`), `desktop` (`status`,
  `check`, `spin`, `counter`, `tick`, `hold`/`release`) and `windows`, and the tree's accessors for `ui.rs`
  (`tree_rows`, `find_node`, `push_request`: a refused request handed back; #207: a window taking one
  request at a time from its opening, `Requests::OneAtATime`, refusing another busy, and
  `published_node`, a node of its published tree);
  `lumen.desktop_cut_delay_ms=` widens the race at a pause for the scenarios;
  the look (#154): titles and the demos' text in the UI font (`font::Text`), the embedded
  wallpaper (`assets/wallpaper/lumen.png`, made by `tools/wallpaper.py`; decoded once a boot at
  the first start, a flat colour and one warning if it doesn't), and the themes (`ThemeId`;
  `theme light|dark`, `lumen.theme=`: a switch damages the whole screen and tells each owner; a
  window's first event is `WindowEvent::Theme`; owners redraw on it with `WindowHandle::theme()`,
  and every wait goes through `Window::wait`, which wakes for `has_event()`). `ui.rs`: the toolkit in a window
  (#156: `Toolkit<A: lumen_ui::App>`, one step per event that publishes before it answers, its palette from the
  `Theme` it is handed (`with_palette`: `taskbar_palette`, #157), its text `font::Text`, `refresh` committing
  only the damaged widgets), `ui invoke`'s `Request` (its reply slot; `start` gives the
  `Started` that answers; either answers for itself if dropped; a `RequestSlot`, a window's one
  request slot, freed before its asker is woken, #207; `Answer::OwnerEnded`), Counter's owner, and `ui tree`/`ui invoke`. `termwin.rs`: terminal windows (#155): `terminal` opens one, its
  owner thread (`terminal`: draws the grid, hands keys over, blinks the focused one's cursor) and its shell
  (`term-shell`, `shell::run_in_window` on a closable console), joined on close; `term text <window>`.
  `taskbar.rs`: the desktop shell's taskbar (#157): a panel along the bottom, its `Launcher` menu
  (Terminal, Mandelbrot, Life, Counter, then the bundled apps granting `desktop`, #206;
  Life starting the Life app, which is left out of those, `LIFE_APP`, #202; refused while
  paused, a failure answered `Then::Failed`), a
  button per window, the `Clock` (the RTC polled without animating, `rtc::now_quiet`, its last reading kept
  on errors; `desktop tick` moves it a minute ahead), its `taskbar` owner thread
  (`lumen.taskbar_wake_delay_ms=` holds back a poke's handling, for the scenarios). `klog.rs`: logger (and the ring `dmesg` reads; ktest kernels: `hold_repeats`, a sweep's repeated lines kept off serial, summarized, #347). `rtc.rs`: the CMOS RTC (`date`, `now_quiet` for pollers; `arch/x86_64/cmos.rs` its ports). `symbols.rs`: symbol lookup.
  `mm/frame.rs`: physical frame allocator, its reservations, `OwnedFrame`,
  `Counts::min_free` (free-frame low-water mark; `LowWaterGuard` in ktests, #394);
  `REFS`, one count per frame of the bitmap's span (reserved by name as
  `frame refcounts`; lumen-mm's `RefTable`), and `UserFrame` (#375: a
  counted user frame, move-only; `alloc_user`, `from_owned`, `share_leaf`,
  `from_leaf`, `into_raw`, `crate::mm` alone moving a reference; dropping
  the last frees it; `user_count`).
  `mm/window.rs`: all-or-nothing mapping of fresh frames into a window
  (heap and stacks), through `paging::map` with a `Window` token.
  `mm/heap.rs`: the `#[global_allocator]`, growing in the heap window.
  `mm/stack.rs`: kernel stacks with guard pages, a dropped one's slot
  reused (#300; a stack that couldn't wait for its range's TLB flush
  counted by `Fallback` reason, shown in `mem`); `arch/x86_64/stacks.rs`
  holds their registry (backtraces, overflow reports). `mm/tlb.rs`: retiring dropped stacks' pages from every
  CPU's TLB before reuse (`unsafe fn retire` giving a `Generation`,
  `flushed_through` the one maker of a `Flushed`, `catch_up` at the timer
  (`on_tick`; `TickFlushesOff` for the ktests) and `WAKEUP` interrupts,
  `join` at the top of `smp::ap_main`, `sync`: IPIs and a bounded wait,
  only outside critical sections, `still_stuck` after a timeout), and
  user address-space TLB shootdowns (#376: synchronous `shoot_down` over
  vector `0x3e`, `arch::idt::TLB_SHOOTDOWN`, handled by `on_shootdown_ipi`,
  `FULL_FLUSH_PAGES`, `STALL_WARN_NS`).
  `mm/paging.rs`: the kernel's page tables (HHDM, W^X image, map/unmap,
  MMIO window, crash-report walk; `protect_user_range_with`, one walk
  deciding each user leaf's rights, #375). `arch/x86_64/paging.rs`: CR3, TLB, NX/WP,
  PAT.
  `mm/user_vm.rs`: per-process address spaces (`AddressSpace`: VMAs,
  ELF loading, streaming `from_images_with_stack` from `ImageSource` with
  `rlimit`-sized stack (#381), anonymous and file-backed `mmap` (`mmap_file`, `Sharing`,
  #255), `munmap`/`mprotect`/`brk`, the page-fault paths (zero-fill, the
  page cache's pages, copy-on-write, dirty promotion, `FaultResult::BusError`),
  `invalidate_file_from`, `Mappers` and `invalidate_file_mappings`
  (cross-space truncation invalidation + shootdown, #378), `futex_key`
  (`linux::futex::KeyResolution`, #383), teardown (every counted frame released);
  #375's `fork_copy` (private leaves read-only in both spaces with their
  counts raised, shared anonymous pages populated and shared, page-cache
  leaves refaulted), the copy-on-write split (`split_private_leaf`: reuse at
  count 1, else copy, shoot down, then release), `users` with
  `add_user`/`drop_user` (teardown at 0), and `active_mask`/`switch_loaded`
  driving `mm::tlb::shoot_down` across active CPUs (#376); `pinned_pages`, `writable_maps` and
  `mapped_files` (`KeptFile`: a mapping's open reference and path; the
  `MappedFile` that `mmap_file` takes also carries its description's
  shared-write right, `O_RDWR` or not, which only `mmap_file` reads, into
  the VMA's `may_write`);
  `map_place` and `map_anon_at`; `msync_files`, what `msync` writes back,
  found under the lock; the images' maps names, `ImageFile`s recorded by
  `commit_loaded_elf` and trimmed by unmaps; `format_maps`;
  `release_order`, the ktests' unmap-order recorder).
  `trace.rs`: trace rings (one per CPU), event kinds, `ktrace!`, the boot timeline
  (`Timeline`), and X-ray's rings (#198: one per CPU; typed recorders for run-queue
  changes, handled device interrupts and `SpinLock`s that waited; `read`: either ring
  of an online CPU, events since a sequence number with the gap counted; `cpus`). `acpi.rs`: ACPI
  discovery (CPUs, LAPIC, I/O APICs, HPET, the MCFG's ECAM regions, the S5
  power-off registers) and the firmware-table reader. `pci/mod.rs`: PCI
  functions found over ECAM at boot (`pci::devices`, which `lspci` lists,
  and `pci::status`), the registry (one slot per function: its BARs'
  mappings and who claimed it), `pci::claim`, `DeviceResources`,
  `FunctionConfig`, and `pci::failed_after_bind` (a driver giving its
  device up after binding); `pci/msix.rs`: a claimed function's MSI-X
  vector (`DeviceResources::msix`, `MsiVector`). `drivers.rs`: the driver table (`DRIVERS`) and
  `probe_all` (declines, the MSI-X vector and readiness checks). `virtio.rs`: the adapter for
  `virtio-drivers` (the `Hal`: DMA runs from the frame allocator, bounce
  buffers, probe attempts; `transport`, `VirtioTransport` and its MSI-X
  vectors, the device status handle, the `Doorbell` a driver's interrupt
  rings, or the driver itself to wake its thread: `mouse::hold`). `blk/` (M7): `virtio_blk.rs` the
  virtio-blk driver (`DRIVER`, `VirtioBlkDevice`: a `lumen_bcache::BlockDevice` over one queue and its
  MSI-X vector) and each device's `BlockCache` (`devices`: the bound entries; `device`: one's driver;
  a `BlkEntry`'s cache only through `stats` and `into_cache`, which `vfs::init` mounts behind ext4's
  `LatchedCache`, #349); `mod.rs` the `blk` command (the devices; `read`/`write` by sector and
  `flush`, straight to the device, past its cache). `rng.rs`: randomness: `rng::fill`, `u64`, `below`,
  `sources`, `stats` (the shell's `random`, rhai's `random()`), RDRAND and
  the virtio-rng driver (its own queue, one deadline per request, dropped
  on a failed request) feeding a ChaCha20 pool. `power.rs`: `reboot` and `shutdown` (ACPI S5;
  the shell's commands sync the filesystems first, `shell/commands.rs`'s `sync_then_announce`, #352);
  `arch/x86_64/reset.rs` their registers.
  `interrupts.rs`: 8259s masked, local APIC on, I/O APIC inputs masked,
  `route_isa_irq` (MADT overrides, handler registered before unmasking).
  `mouse.rs`: the virtio tablet (#98; bound by class, read by its own
  `mouse` thread, which moves the pointer; `mouse::status` for `mouse`;
  `mouse::hold`/`release` stop and resume its reads, for overruns on purpose;
  each frame also into `input::POINTER`).
  `make run` attaches a tablet.
  `input.rs`: the desktop's input rings (#151): `POINTER` (every tablet
  frame) and `KEYS` (every key event), `lumen_input::Ring`s behind leaf
  locks (`LockedRing`, an `InputDevice`); `mouse events`, `kbd events`.
  `serial_in.rs`: COM1's input by interrupt (IRQ 4) into the queue the
  serial shell blocks on (a 100 ms deadline as a safety net, counted and
  warned about; `serial` shows it); it polls, yielding, without that
  interrupt or before the boot CPU's scheduler runs, and blocks for good
  without a UART; the UART's one reader; readiness subscriptions
  (`subscribe_poll`, `note_poll_woken`, #374) and `wait_or`/`wake_reader`
  for the console tty input thread (#389).
  `keyboard.rs`: the PS/2 keyboard (8042 init, IRQ 1, `pc-keyboard`
  decoding through `lumen_input::KeyDecoder`, each key event into
  `input::KEYS`, `lumen.keymap=`); `arch/x86_64/i8042.rs` its ports.
  `tty.rs` (M8, #388, #389): terminal line disciplines (`Tty` over
  `lumen_tty::LineDiscipline`, Lock 9a `SleepMutex`, readiness through
  `KernelPollHead`, `read`/`write`/`ioctl`/`poll`, job control and
  `leader_exit`), console tty instances (`tty::init`, `tty::console`) and
  `ConsoleSession` (exclusive `tty-serial`/`tty-vt1..4` input threads for
  running Linux root processes, #389).
  `time.rs`: TSC and LAPIC timer calibration (judged by each clock's
  tolerance; `lumen.stall_tsc_windows=`/`lumen.stall_timer_windows=` stall
  them for scenarios), the boot origin
  (`mark_boot`, `since_boot_ns`), `now_ns`, `sleep`, `ticks`.
  `demo.rs`: the `mandelbrot` (bands on every CPU) and `life` commands, and
  the launcher's Mandelbrot window (`draw_bands`: a window's surface lent
  as bands to a thread per CPU through `sched::scope`; #157's Life window
  is the Life app since #202).
  `perf.rs`: `perf top`, the sampling profiler (#205): the timer tick's
  samples (`on_tick`, a call and one load unless a profile runs) into a
  per-CPU `lumen_perf::Buffer`, `profile` (a count per CPU, not a duration;
  a `Profiling` guard disarms; silent CPUs a hint), ranked by function (an
  `FnIndex` over `symbols::functions`, built per ranking; `<unknown 0x…>`
  rows) or `--threads`; refused in a pipeline; `perf burn` spins
  `burn_loop` on a CPU (`StartError`, `StopError`; a stopping burn keeps
  its slot and handle until a stop sees it finished, every wait stopping
  on `q` or Ctrl-C).
  `app.rs`: WASM apps (M5): the embedded modules (`App`, `BUNDLED`, `TEST_APPS`, written by
  `kernel/build.rs` from `build/apps`); `app/runtime.rs` the runtime (#199: the table of apps,
  `precheck` then a slot under its lock, loading on the app's own thread behind a start gate
  (`live`), one `Engine` per app with section 8's wasmi configuration, the import/export/fixed-memory
  checks, `out_of_memory` (instantiation's and the translator's), fuel slices through one slice-end
  path (`end_slice`/`begin_slice` over the store or a `Caller`: the kill flag, the close grace, fuel
  debt), one reader of wasmi's answers (`step`: a host function's recorded `Stop`), each app's `Life`
  (one phase, moving only forward, and one `Outcome`, the first recorded; `finish`; a load failure
  printed by `run` or logged once, `start_line`, `stopped_by`), `kill` waiting at most `KILL_WAIT_NS`
  (`kill_outcome`, `Phase::place`), the `app-reaper` joining ended apps; `apps` (records), `run`,
  `kill`; `desktop_apps` and `start`, the launcher's); `app/window.rs` the window and text calls (#206:
  `AppWindow`, an app's one window on the live desktop with the event in hand (`Held`) and `text_draw`'s
  scratch surface; the canvas copied in by rows; `window_wait`
  through `blocking`, a kill waking it through the window's `Waker`; fuel by the pixel and by the glyph
  rasterized) and its automation tree (#207: `window_publish`, the app's ids mapped to global ones;
  requests taken by `request::take` and handed on as the app's ids, one running (answered `OwnerEnded`
  by `AppWindow`'s `Drop`, after the window); `IdMap` for the ids; `window_answer`, answered from the published tree; the request
  ktests over a screenless desktop); `app/host.rs` the `lumen` host
  functions without a window (`entry`: the kill flag, a slice
  end with too little fuel left, `CALL_FUEL`, for every call; `ending` for `exit` and `panic`;
  `blocking` for `time_sleep` and #206's `window_wait`; `read_trace`); `app/tests.rs` the runtime's
  ktests, over hand-assembled modules (a `Spec` builder) and the test apps, with seams for a kill at
  a resume, a failed reaper spawn and the heap refusing. And
  #197's checks: `wasmi` (`portable-dispatch`: a constant native stack depth
  however long a module runs; functions compiled eagerly; see ARCHITECTURE sections 2 and 8) running a
  module in fuel slices (`run_in_slices`, `run_bounded` with a resume cap: out of fuel, yield, refuel at
  least what the instruction that ran out needs, resume; `Failed`: load, export, run (after how many
  resumes) or the resume cap; a host trap is an error, never a panic), `wasm` (the checks in the booted
  kernel, shared with the ktests: `LoopChecks`, its resume band from the loop's fuel per iteration) and
  its ktests, over hand-assembled modules (`app/modules.rs`: a long loop and call depth, one float
  operation per export, a bulk `memory.fill`, a generated large body). `app/wire.rs`: ktest only,
  postcard's encoding and decoding rules over serde-derived types (the wire types themselves are
  `lumen-app-api`'s `event`, `tree`, `answer` and `wire`, host-tested). `mm/heap.rs`'s `peak`/`reset_peak`: the heap's high-water mark (and, in the ktest
  kernel, `refuse_from`: the heap refusing big allocations, for `not enough memory`).
  `font.rs`: the desktop's anti-aliased text (`lumen-font`): `font::face`,
  the UI font parsed on first use and shared; `font::Text`, one drawing
  thread's glyph cache over it (`draw` glyph by glyph inside a clip,
  rasterizing only the glyphs reaching it and saying how many it did,
  `measure`, `line_height`; `TITLE_PX`, `TEXT_PX`, `HEADING_PX`); `font`
  renders the golden sample twice (timings, cache, golden check), as its
  ktest does.
  `screenshot.rs`: `screenshot`: the screen captured (`capture`: the
  desktop's back buffer while it is shown, `desktop::capture`; else the
  framebuffer read once under the screen's lock, `term::read_pixels`,
  which says if a canvas held the screen), encoded on a 256 KiB stack, sent as a base64 frame
  on serial (the format: ARCHITECTURE section 11).
  `sound.rs`: the virtio-sound driver (its `sound` thread owns the device,
  playing `lumen-synth` sources paced by the device), `beep`, `play`,
  `volume` and `sound`.
  `vfs/` (M7, #253; ARCHITECTURE section 9): `vfs/mod.rs` the kernel's VFS
  tables (`KernelVfsTables`: lumen-vfs's `VfsTables` on `SleepMutex`;
  `KernelLocks`/`KernelLeafLock` and `KernelPollHead`, #374), `init` (the
  first ext4 virtio-blk volume at `/`, else a tmpfs root; `/tmp`, `/dev`,
  `/dev/pts` (#388), `/dev/shm` (a 1777 tmpfs on `DevId::DEV_SHM`, #433),
  `/proc` over it), `sync_all` (each filesystem flushing its own inodes),
  and the `mount`, `sync` and `vfstest` commands (`mount`'s `used`/`limit`:
  a tmpfs mount's quota, through `mounts_with_fs` and `tmpfs::quota_use`,
  #324; an errored ext4 volume's mode `ro,errors`, #349; `vfstest`: file
  operations from the kernel shell, for the scenarios to set up and inspect
  files around ring-3 programs);
  `vfs/tests.rs` the ktests of `mod.rs`'s code (`lumen_kernel::vfs::tests::`;
  `vfs/ext4.rs` and `vfs/page_cache.rs` keep their own; `MemBlkDev`, the
  in-memory block device those files' ktests use too (`fail_device_writes`:
  the block cache's writes back failing, #331), stays in `mod.rs`);
  `vfs/page_cache.rs` the frame-backed `PageCache` shared by ext4 and tmpfs
  (dirty and writable-map tracking, the soft 32 MiB ext4 reclaim and its
  64 MiB hard cap, one check, `admit_unreclaimable`, for a dirty or mapped
  page, `QuotaCounter` (bytes; a leak logged when dropped), the detached-inode
  quota; `read_into`/`write_from`/`resize`/`map_page`, the page I/O ext4 and
  tmpfs share, #264; a failure past the first page a short count (`page::short_count`), #321; `mmap_page`,
  re-exported as `vfs::mmap_page`: an ext4 or tmpfs file's own cached page
  for a mapping, through `VfsInode::as_any`, and `MappedPage::copy_to_frame` (under the cache lock)
  for copy-on-write, #255; `WritableMap`, the guard counting a writable
  shared PTE, made by `MappedPage::map_writable`; the last writable unmap
  leaving a page dirty, #266; `ktest_files`, scratch tmpfs and ext4 volumes
  for `mm::user_vm`'s file-backed ktests and `linux::syscall::io`'s
  positional I/O, `fsync` and `sync` ktests); `vfs/ext4.rs`
  the writable adapter over `ext4plus` and the `BlockCache` (writeback,
  Gate 6 open-unlinked detach, `rmdir`/`rename`/inline-symlink workarounds,
  undoing a namespace operation's disk changes after a write error, then
  `errors=remount-ro`: the first failed device write, told by the block
  cache's `first_failure` (`LatchableCache`, the controls only the caches
  below the latch have) and latched at `LatchedCache`, the one seam the
  volume holds, makes the volume read-only (syncs stop there, older shared
  mappings get no new writable PTE, a miss with no clean slot read
  uncached, an operation running then `EIO`, `WriteOp::end`) and
  freezes its block cache when the last write-type operation (`WriteOp`,
  naming the one that met it) ends, or at once for a read's eviction with
  none running, #349; the
  link counts kept in memory when an undo fails (`NlinkBounds` through
  `write_links_or_pin`/`raise_links_or_floor`); `rename` split into
  `Exchange` (`RENAME_EXCHANGE`: `run`, `roll_back`, `finish`), `MoveEntry`
  (link, unlink, `roll_back`, `finish`) and a held target (`hold_target`,
  then `put_back_target` or `retire_target`); a removed child's end
  through `ChildFate`/`settle_child`; `snapshot_live`, reclaim's and
  `sync`'s inodes; a device's number read
  raw from `i_block`, `read_rdev`, #292; the times through `times`'
  `InodeTimes`, clamped on load and by every setter and stored by `store`/
  `store_ctime`, and `new_inode`, the one way to make an inode, #294;
  after an unlink or rmdir failed with its directory unreadable,
  `child_lost_entry`: whether the child's entry went, from its inode
  (`child_after_failed_unlink`'s `ChildReadBack`: zeroed, its first freed
  block free by lumen-vfs's `first_block_freed`, allocated, or unreadable
  (`Undecided`, why): kept live and logged), #330; `Ext4DiskLayout` over lumen-vfs's `Geometry`
  (`block_byte`, `bit_is_clear`, `read_i_block`); `DirHash`, the htree hash
  by `s_flags` (neither flag with `dir_index` mounts read-only,
  `forced_read_only_reason`, a lookup in an htree directory missing a name
  with a byte of 0x80 or above walking the directory, `scan_dir_for`, #308; its ktests' image
  `testdata/unsigned-hash.ext4`, made by `tools/ext4-unsigned-hash-image.py`);
  `ktest_util`, the ext4 ktest helpers `vfs/tests.rs` and `page_cache.rs`
  share: `FaultCache` (writes failed by number, pair or from one on,
  reads by range or after a write fault), `Mnt`, `mount`, `remount_ro`,
  `set_clock`, the ktests' clock, `Ext4Shared::now`; `mknod` FIFOs, #386,
  and `statfs`, #391; #355: a writable mount runs lumen-vfs's
  `check_allocation` through the block cache first, an inconsistency or a
  volume it can't check mounting read-only with one warning, leaks counted
  at `debug`; the fast-symlink/device/blocks rule from `i_block_kind_raw`;
  #350: a count/bitmap mismatch `ext4plus` reports makes the volume
  read-only);
  `vfs/tmpfs.rs` (its quota charging inodes, directory entries and symlink
  targets besides pages, #267: `charge`, the cost model, `InodeCharge`,
  `EntryCharge` and `Children`, the only code reserving or giving one back;
  `quota_use`, a mount's `QuotaUse` for `mount`, #324; `mknod` FIFOs with
  a boxed `FifoSlot` (`Option<Box<FifoSlot>>`), #386, and `statfs`, #391),
  `vfs/devfs.rs` (`null`, `zero`, `random`, `urandom`, `tty`, `console`,
  `ptmx` with `TerminalOpen::{Ptmx, Alias, Console}` and `terminal_open`, #388, #389;
  `DevKind::MountDir`, the fixed `pts` and `shm` directories other
  filesystems mount over, #433),
  `vfs/devpts.rs` (M8, #388, #389: `/dev/pts` filesystem `Devpts` on dev
  0:5, `PTY_MAX` = 256, `TtyEnd` open-description lifetime, `open`
  allocating PTY pairs on `/dev/ptmx` and resolving `/dev/tty` and
  `/dev/console`, `console_file`)
  and `vfs/procfs.rs` (`self`, `<pid>/{exe,fd,maps,mounts}`, `cpuinfo`,
  `meminfo`, `mounts`, `version`; `mounts`: an errored ext4 volume
  `ro,errors=remount-ro`, `format_mounts_of`, #349; `/proc/<pid>/maps` from
  `AddressSpace::format_maps`, its lines `lumen_vm::maps`'s; `/proc/<pid>/exe`
  updated on `execve`, #381; the node
  kinds, inode numbers, fds and numeric names `lumen_vfs::procfs`'s).
  `linux/` (M6, M7, M8; ARCHITECTURE section 8): `linux/mod.rs` the
  `linux-raw-sys` re-exports, `M6_SYSCALLS`, `M7_SYSCALLS` and
  `M8_SYSCALLS` (the numbers, `syscall_name`; pinned by the
  `abi_constants_match_spec`, `m7_abi_constants_match_spec` and
  `m8_abi_constants_match_spec` ktests, the latter two with their flags and
  struct layouts, #371), `uname`'s payload, `pub use process::timer`, and the `linux` command (`linux /path`
  from the VFS, `run_exec`: load, run, count frames, the path as `exe`, the thread named after the path's last component, #276);
  `linux/init.rs` `linux::init`, called at boot after the VFS to start pid 1
  (`process::start_init`: a kernel-owned process with no threads, every
  exit's reaper, #372), the console ttys (`tty::init`, #389), and the timer
  worker (`linux::timer::init`, #392);
  `linux/exec.rs` `Exec` (#255, #381: `ExecFile`, `open_path`/`open_inode`,
  streaming a binary and its `PT_INTERP` interpreter through `ImageSource`
  with its `ImageFile` (none for a boot module), naming its maps lines and
  giving its path, `load`/`load_with_stack` through
  `AddressSpace::from_images_with_stack`; `ExecError` and `to_errno`, each
  read naming its file);
  `linux/process.rs` processes and threads (#372, #379, #380, #381, #382,
  #384, #385, #387, #388, #389, #391), with child modules enforcing owner
  privacy: `Process` (a thread group: pid, `mm`, `files`, `fs`, `signal`,
  `exe_path`, console state, `exit`, each written only through its owner
  methods such as `record_exit`), `UserThread` (a tid, its process,
  `clear_child_tid`, robust list; held in the scheduler thread's
  `UserThreadState::linux`, so a syscall finds it lock-free through
  `current_thread`/`current_process`), `KernelFile` (combined `end_refs`,
  in-flight `IoFile` refcount guard, optional `PipeEnd` and `TtyEnd`, #379,
  #386, #388), `process/clone.rs` (`ProcessClone`, `clone_flags`: fork,
  vfork and thread clone validation, #380, #382), `process/table.rs`
  (`ProcTable` behind `PROCS`: lumen-proc's tree, processes by pid, threads
  by tid, `WaitResult`, `link_init`/`link_root`/`link_thread`/
  `unlink_thread`/`exit_in_tree`/`wait`/`setsid`/`setpgid`/`getpgid`/
  `getsid`/`priority`/`process_time`, `check_nproc` (fork and thread
  clone against `RLIMIT_NPROC` through lumen-proc's `allows_new_task`,
  after `check_clone`; root not exempt; `nproc_task_count` the ktests'
  count, #435), `check` cross-checking them in the ktests), `process/files.rs` (`Files`: the 1024-slot FD table of
  `KernelFile`s, shared/exclusive `FilesAccess` leases, `ReservedFd`,
  `fork_copy`, `prepare_exec`/`copy_exec_into`; `FsState`/`FsInner` for
  `Cwd` and umask, #379, #380, #381), `process/signal.rs` (`SignalState`,
  `ThreadSignals`, `Delivery`, `HandlerDelivery`, group exit/stop/cont,
  `wait_signal`, `sigaltstack`, `ctty`, #384, #385, #388; `Rlimits` (stack,
  `NOFILE`, `NPROC`) read and replaced through `Process::rlimit`
  (`Rlimit`, `RlimitKind`); `Process::nofile_soft`, the live soft limit
  bounding fd allocation and `poll`'s `nfds`, #435),
  `lookup`/`lookup_thread`, `fork_current`, `de_thread` (#381, #382),
  `register_current` and `take_current` (a run's takedown in the design's
  exit order: detach, unlink, release `mm`/`files`/`fs`, then the tree, its
  `NoMem` retried), `install_current_fd`/`insert_current_fd`/
  `dup_current_fd`(`_from`)/`close_current_fd` (close and exit releasing
  POSIX and `flock` locks outside `PROCS`, `release_fd`), `ps_rows` for the shell's `ps`,
  the advisory locks' wait queue and release count (`lock_releases`,
  `locks_released`, `wait_for_lock_release`: `F_SETLKW` and blocking
  `flock`, #254), `Stdio` (fds 0/1/2 on `/dev/console` bound to the console
  tty when interactive, `/` as cwd; opened before a run or the run is
  refused), `open_kernel_file`, `ConsoleIo`
  (`Capture`, `Serial`, `Vt(VtId)`), `in_test_process` (`add_ktest_thread`,
  `with_process_user`), and `RegisteredProcess` (#264);
  `linux/signal.rs` (#373, #384: `RestartBlock`, `ERESTARTSYS`,
  `interrupted_with`, `exit_to_user` syscall/fork return hook,
  `exit_to_user_from_interrupt` non-IST return hook, `force_fault`,
  `force_sigsegv`, `force_debug_trap`, `exception` translating CPU faults
  to `SigInfo`) and `linux/signal/frame.rs` (`deliver`/`restore`:
  `rt_sigframe` copy-out/copy-in and FPU/XSTATE snapshot/restore outside
  siglock, `FrameFailure`/`FailureReason`, #384);
  `linux/futex.rs` (#383: `FutexKey` (`Private`, `Anon`, `File`,
  `SharedFrame`), `KeyResolution`, 256-bucket `FUTEX_BUCKETS` over
  `lumen_futex::FutexTable`, `wait`/`wake`/`requeue`, `release_user_sync`
  clearing `clear_child_tid` and walking the robust list on thread exit);
  `linux/pipe.rs` (#386: `PipeObject` over `SleepMutex<lumen_pipe::Pipe>`,
  `readers`/`writers` `WaitQueue`s and `KernelPollHead`, `read`/`write`/
  `set_capacity`/`fionread`/`poll_open`/`deliver_sigpipe`, `PipeEnd`,
  `PipeInode` for anonymous `pipefs` on dev 0:6, `FifoSlot` and
  `open_fifo` for ext4/tmpfs FIFOs);
  `linux/timer.rs` (`#[path = "timer.rs"]` under `process`, re-exported as
  `linux::timer`, #392: `Timers`, `TimerId` (`Real`, `Posix`),
  `PosixTimer`, `TimerNotify`, global `TIMERS` deadline map with boot-CPU
  `on_tick` wakeup and timer worker thread from `timer::init`,
  `create`/`set`/`get`/`delete`/`overrun`/`delivered`/`clear`);
  `linux/modules.rs` the Limine boot modules; `linux/syscall/`: `mod.rs`
  `dispatch` (each number in one arm, its constant as the pattern; M7's and
  M8's `not_implemented` stubs are gone; of the M8 table's rows only
  `sendfile` and `membarrier` stay `-ENOSYS`; with `clone3`, `execveat`,
  `futex_waitv`, the scheduler-policy calls, `signalfd*` and `times` they
  make up `ENOSYS_BY_DECISION` in `tools/harness/lumen_harness/syscalls.py`
  (ARCHITECTURE section 8, "Stays `-ENOSYS` in M8")), `unmap_past`
  (#378; `meta.rs`'s `unmap_past_size_now`, a failed `O_TRUNC`'s unmap,
  #414) and the shared copy helpers, `time.rs` (`RawTimespec`,
  `read_timespec`, `write_timespec`, `ERESTARTNOHAND`, #383, shared by `futex`,
  `poll` and `signal` (`rt_sigtimedwait`)), and a module per cluster holding its handlers (`fd`,
  `stat`, `ns`, `dir`, `meta`, `io`, `lock`; M6's `mem`, with #255's
  `mmap` of a descriptor and `msync` (ktests in `mem/tests.rs`), `proc`;
  M8's clusters, #371: `task` (`exit`, `exit_group`, the ids,
  `set_tid_address`, `clone`, `fork`, `vfork`, `execve` with shebangs and
  `ArgBudget`, `wait4`, `waitid`, `setpgid`, `setsid`, `getpgid`, `getsid`,
  `getpgrp`, #380, #381, #382, #387), `signal` (`rt_sigaction`,
  `rt_sigprocmask`, `rt_sigreturn`, `rt_sigpending`, `rt_sigtimedwait`,
  `rt_sigqueueinfo`, `rt_tgsigqueueinfo`, `rt_sigsuspend`, `sigaltstack`,
  `kill`, `tkill`, `tgkill`, `pause`, `restart_syscall`, #384, #385),
  `futex` (`futex`, `set_robust_list`, `get_robust_list`, `nanosleep`,
  `clock_nanosleep`, #383), `poll` (`poll`, `ppoll`, `select`, `pselect6`
  over `wait_ready` and `PollWait`, #374, #385), `pipe` (`pipe`, `pipe2`,
  `dup`, `dup2`, `dup3`, `mknod`, `mknodat`, #386), `timer` (`alarm`,
  `getitimer`, `setitimer`, `timer_create`, `timer_settime`,
  `timer_gettime`, `timer_getoverrun`, `timer_delete`, #392) and `sys`
  (`sched_yield`, `sched_setaffinity`, `sched_getaffinity`, `getpriority`,
  `setpriority`, `getrusage`, `sysinfo`, `prctl`, `statfs`, `fstatfs`,
  `clock_getres`, `getcpu`, `setfsuid`, `setfsgid`, #391);
  `fd` holds `open`/`openat`/`lseek` (with `open_fifo`, #386, and
  `devpts::open`, #388, #389) and descriptor I/O/`ioctl` (`FIONREAD`, tty
  requests) through the table, routing terminals by `VfsInode::is_tty`
  to their tty or the process's `ConsoleIo`, and the syscall ktests'
  shared helpers (`ktest_user_path`, `ktest_fd`, `ktest_fd_of`: a failure
  naming the path, `ktest_open`, `ktest_install` and `ktest_install_file`:
  a descriptor on a volume no mount holds, `ktest_failed_at`,
  `KtestTmpFile`, `ktest_unlink`), `dir` `getcwd`,
  `getdents64` (the filesystem's `readdir` cursor in `dir_cursor`), `chdir`
  and `fchdir`; `lock` holds `fcntl` (incl. `F_DUPFD*`, `F_GETPIPE_SZ`/
  `F_SETPIPE_SZ`, #379, #386) and `flock`: `lock_or_wait`, the retry loop
  over the wait queue, and `POSIX_WAITERS`, `F_SETLKW`'s `EDEADLK`
  records), and `at.rs` the file syscalls' shared parts: `reg`/`FromReg`,
  decoding a register into a handler's parameter type (`Fd`, with `Fd::file`
  the descriptor lookup, `Dirfd`, `Mode`, the VFS flags, `UserPtr`,
  `u32`/`i32`: the low 32 bits), `UserPath` (a path argument copied in),
  `start_dir` and its `Start` (the one dirfd resolution, and the only way
  to lumen-vfs's path operations: `open`, `resolve`, `mkdir`, `mknod`, `unlink`,
  `readlink`, `symlink`, `Start::link`, `Start::rename`) and
  `stat_from_metadata`/`statx_from_metadata`; the handlers' rules are in
  ARCHITECTURE section 8, "Handlers".
- `crates/lumen-demo`: fixed-point Mandelbrot, dealing image rows to CPUs
  in bands, and Game of Life with its patterns (host-tested; the `life`
  command's and the Life app's, a path dependency of `apps/`, #202).
- `crates/lumen-elf`: ELF symbol-table reader and `loader` (`ElfHeader`,
  bounded header-only parsing `parse_headers`/`parse_headers_with_pie_base`/
  `parse_interp_headers`, `validate_interp`, `tests/allocations.rs`, #381)
  (host-tested); `shebang`
  (#343: a `#!` header from a file's first `BINPRM_BUF_SIZE` (256) bytes,
  as Linux's `load_script`: `parse` giving a `Shebang` (interpreter, at most
  one argument) or a `ShebangError` with its errno; `Nesting`, at most
  `MAX_NESTING` (5) interpreters an exec, else `ELOOP`) and `args` (#343:
  `ArgBudget`, `execve`'s byte budget for argv, envp and execfn charged
  string by string in `do_execveat_common`'s order before each copy,
  `rewrite` swapping `argv[0]` for an interpreter's words in one
  transaction, `E2BIG` before `ENOMEM`; `MAX_ARG_STRLEN`, `MAX_ARG_STRINGS`,
  `check`, `normalize_argv`) (host-tested).
- `crates/lumen-perf`: the sampling profiler's logic (#205): `Buffer`, a
  lock-free sample buffer with one writer (a CPU's timer interrupt) armed
  for a count (an arming generation in its state word), `by_function`/
  `by_thread` `Ranking`s with idle counted apart, `all_taken`, `Silence`
  (a CPU that seems to have stopped ticking: a hint), `header` over
  `Taken`s, `FnIndex` (functions by binary search, agreeing with
  `lumen_elf`'s scan) and `perf top`'s arguments (host-tested, incl. a
  writer racing re-arms and aliases at scale).
- `crates/lumen-font`: anti-aliased TrueType text: `fontdue` (no `simd`)
  behind `Face` (parsed once, shared read-only; `measure`) and `Font`
  (a face plus its own glyph cache keyed by (glyph, pixel size), bounded
  in bytes, least recently used evicted), one-line layout (`render`;
  `render_where`, which skips a glyph whose box a caller refuses before
  rasterizing it; `render_mask` into a `LineMask`) into plain coverage masks, and the
  embedded UI font, Atkinson Hyperlegible (`assets/fonts/`, SIL OFL)
  (host-tested, incl. a model LRU, a `kern` fixture and a golden mask
  whose hash the kernel's ktest also checks).
- `crates/lumen-gfx`: the desktop's pixel core: `Rect` (signed origin,
  intersect/union/clip, subtract into at most 4), `Damage` (a fixed-size
  list that coalesces, a bounding box past its size, never loses
  coverage), `Surface` (xRGB8888 with a stride: fill, clipped blits, `move_rows` (a scroll),
  `blend_mask`/`blend_mask_in` for coverage masks such as glyphs, the
  latter clipped to a damaged rect, `bands_mut`: disjoint row bands,
  `BandMut`, for threads) and `Target`, what a surface is
  presented to a span at a time (a surface; the desktop's front buffer,
  `term::ScreenCanvas`) (host-tested, incl. randomized coverage and
  every-edge clipping).
- `crates/lumen-wm`: the desktop's window-manager logic (#152): frame
  geometry (`Metrics`: title bar, close and minimize boxes, client, borders; `Part`),
  minimize/restore and panels (#157: `open_panel`, frameless, kept in front, never
  focused, its rows reserved),
  `Wm` (stacking, focus: a click focuses and raises, closing the focused
  one focuses the front one; hit testing; a left drag from a title bar
  clamped to keep a strip on the screen; the implicit grab of a client
  press, until its last button is up, it loses the focus or input is
  interrupted; close-box clicks on a left release; `interrupt` (a pause,
  a pointer gap, a key gap) and `resume(baseline)` so no window keeps a
  button or key, focus changes held back while paused; every operation
  returns its `Damage` and `Delivery`s, `#[must_use]`), a window's
  `EventQueue` of `Entry`s (key events through `KeyLike`, which
  `lumen_input::KeyEvent` implements; pointer events (`Pointer::from_read`
  carries a ring read's gap, #151),
  requests, a refused one handed back by `push_request` and the queued
  ones returned by `kill`; sticky `Lost`/focus/`Close` flags delivered in place and never
  pushable, presses, moves and requests rationed past 48 of 64, releases
  only after their press, unpaired releases counted apart: keys, buttons
  forgotten, buttons never pressed), `deliver` (a `Delivery` onto its
  window's queue) and the light and dark `Theme`s (host-tested, incl. a
  randomized model of damage-only repainting with focus, and
  press-pause-resume-release through the queue).
- `crates/lumen-ui`: the desktop's toolkit (#156): a retained widget tree
  (`Ui`: stacks (`Stack` handles), labels (`fixed_label`), buttons, checkboxes, text fields, text areas
  (`text_area`, #203: a `textfield` node its app draws), lists (`set_selected`: the app's own
  choice, the focus left where it is),
  menus with items (#157); `Ui::bar` (across, one widget pinned right); `remove`
  (generational `Widget`s: a freed slot taken again); per-widget damage (`take_damage`,
  `paint_in`); layout,
  hit testing, dispatch, painting through a `Text` trait, `FixedText` for
  tests), every widget an automation node (`NodeId` from one counter,
  `Role` with its actions, value kind and flags, `Value`, `Action`,
  `StateFlags`, `AutomationNode`), `Ui::invoke` through the click's own
  handler, `App` (`ui_mut`, `apply`, `paint_over` handed an `Over`: surface, text, palette, the rects
  repainted; `take_drawn`: the app's own damage), `serve` (a request run on an app, what to answer)
  and `repaint` (lay out, paint only what changed, the app's own damage too, publishing only for the
  tree: the kernel's toolkit and an app's share it, #207; rects merged and disjoint, every rect given
  the tree's paint first), `Pacer` (an animated window's next tick: `poll`, `ticked`, `MIN_PERIOD_NS`),
  `Reply` (`ui invoke`'s reply slot, its state private: pending, running,
  cancelled, done, taken) and `counter` (the demo app's logic)
  (host-tested).
- `crates/lumen-input`: pointer input: evdev events into whole-frame
  pointer states, axes scaled to the screen (`Tablet`, `AbsAxis`), the
  buttons held (`Buttons`, one bit per `Button`: the one button set the
  tablet and lumen-wm share, #172), a
  frame's most events from the device's `EV_BITS` (`frame_bound`) and
  whether a batch may have lost frames (`may_have_lost_frames`, the one
  statement of what overruns guarantee; a model of QEMU's ring checks
  it), the arrow sprite with its save-under (`Cursor`, over a
  `Surface`; a randomized model of drawing around the arrow checks it),
  the keys pressed and held in PS/2 set 1 bytes (`Presses`, for
  `kbd`'s `presses` and `held`), and the desktop's input (#151): numbered
  event rings (`Ring`, `Seq`, `InputDevice`, a `Reader` counting what it
  missed), `PointerEvent` (with `AfterOverrun` marking frames that may
  follow a loss at the device), `KeyEvent`/`KeyAction`/`Mods` from set 1
  bytes (`KeyDecoder` over `pc-keyboard`: one press per key, agreeing with
  `Presses` wherever pc-keyboard refuses no byte, one listed exception;
  `AfterDrop` marks the first event decoded from bytes queued after a
  drop, maybe late, never early; `MarksLoss`/`Read::gap` the one loss
  check) (host-tested).
- `crates/lumen-mm`: frame-allocator logic (bitmap), typed addresses, the
  page-table format and mapper (host-tested, incl. a model comparison), the
  window tokens that confine `paging::map` (`Window::join`), and the
  kernel-stack layout (`StackWindow`: `StackSlot`s, freed ones kept as
  joined free ranges tagged with a TLB `Generation`, first fit once a
  `Flushed` covers it, #300; a model with out-of-order give-backs and a
  page-table count host-test it), lock-free stack registry (`Registry`,
  `KERNEL_STACK_RESERVE` and `register_user` reserving slots for kernel
  threads, #382), and
  `frame::RefTable` (#375: the kernel's per-frame reference counts over
  caller storage, `set_one`/`inc`/`dec` giving `Remaining`, overflow,
  underflow and re-initialisation refused; host-tested incl. a model and
  racing last decrements).
- `crates/lumen-heap`: heap allocator logic (address-ordered free list,
  coalescing; host-tested, incl. a randomized stress test against a shadow map).
- `crates/lumen-vm`: a process's user address space as logic: the user
  layout (`USER_MIN`/`USER_MAX`, `PIE_BASE`, `MMAP_TOP`, the initial stack's
  `USER_STACK_TOP`/`USER_STACK_SIZE`/`USER_STACK_BOTTOM`, `StackLimit` and
  `StackSize::from_rlimit`/`arg_limit`, #381, #391), `Prot`, `Vma`s
  anonymous (private or shared, `Backing::Anonymous { shared }`, #375) or
  file-backed (`Backing`, `Backing::is_shared`, `FileMapping`), the `VmaMap` (sorted
  by start in a `BTreeMap`: `map` with `MapPlace` hint/fixed/no-replace,
  `unmap`, `protect`, coalescing), the `Brk` cursor, and `maps`
  (`write_maps`: `/proc/<pid>/maps` lines, the heap split at the break's
  edges, `[heap]`/`[stack]`, a file's device, inode and path (its bytes,
  each newline as `\012`, #268), names padded as Linux's `seq_pad`; `ImageRange`, an ELF image's named pages:
  `from_segment`, `trim`) (host-tested).
- `crates/lumen-bcache`: block storage (M7, ARCHITECTURE section 9): the
  `BlockDevice` trait (sectors; `BlockInfo`, `BlockError`), `BlockCache`
  (4 KiB blocks, LRU over a fixed budget, `DEFAULT_CAPACITY`; pinned blocks,
  `PinnedBlock`/`PinnedBlockMut`, never evicted; `read_at`/`write_at`;
  `sync`/`flush_blocks` in block order, then the device flush owed,
  `needs_device_flush`; a failed device write kept dirty and cached for the
  next writeback (while writeback runs), its error reaching whichever call
  wrote the block back, #331; #349: `first_failure` (every cache: the first
  failed device write or flush, kept for good); `Writeback` running, stopped
  (`stop_writeback_on_failure`: at the first failure) or frozen (`freeze`:
  writes into the cache refused too), a read miss with no clean slot then
  read uncached, `read_uncached`; `stop_writeback`/`thaw` behind the `test-util`
  feature, the ktest kernel's) and `MemBlockDevice` (in memory: injected
  errors, power loss) (host-tested).
- `crates/lumen-vfs`: the POSIX VFS core (M7, ARCHITECTURE section 9): the
  `FileSystem`/`VfsInode` traits (`StatFs`, #391; names as bytes, #268; `Escaped`, a name's
  bytes as text, never lossily; `FileSystem::errored`, read-only after an
  error, a `MountInfo`'s `errored`, #349), the path and symlink resolver
  (`resolve_parent`'s `ParentTarget`, its last component a `Final`: root,
  `.`, `..` or a name, #309; `NamedPath` and `resolve_named_path`, #381), the mount
  table and the Lock-9 inode and dentry caches (`VfsTables`, generic over a
  `LockFamily`: the kernel's and the host tests'; `mounts_with_fs`, each
  mount with its filesystem; `InodeCache` dropping no inode under Lock 9,
  handing back each one it lets go, `Released`, #320, and keeping removed
  directories' parents for `..`, `record_removed_dir`/`removed_dir_parent`,
  #318), `OpenFile` (its inode,
  `inode()`, read from its `OpenGuard`, #264; M7's `inc_fd_ref`/`dec_fd_ref`)
  and the `FdTable` (`MAX_FDS` fds; `reserve`/`install`/`cancel`,
  `FdReservation`, `try_preallocated`, `src/tests/hazards.rs`, #379, #381;
  the limit-taking family `alloc_below`, `alloc_from_below`,
  `reserve_below`, `insert_at_below`, what syscalls use under
  `RLIMIT_NOFILE`, #435),
  `poll` (`PollMask`, `PollWaker`, `Pollable`, `PollGuard`, `PollHead`,
  #374), the namespace operations (`open_inode`, `open_inode_guarded_with_truncate_error` (#414), `mkdir_at`, `mknod_at` (#386),
  `unlink_at`, `rename_at2`, ...), `flock` and POSIX record locks (each
  call saying what it `released`: `FlockOutcome`, `RecordLockOutcome`,
  whose `WouldBlock` alone names the holder; `PosixWaitGraph`, who waits
  for whom, for `EDEADLK`, its search's `WaitSearch`),
  `procfs` (`/proc`'s `ProcKind`s, their inode numbers and types, `ProcFd`:
  an fd in `0..MAX_FDS`, `ProcFd::parse`, and `parse_canonical_u32`: one
  name per number), `pseudo` (what devfs and procfs share: `fill_readdir`,
  a listing keyed by entry, and `fixed_namespace_ops!`, the seven namespace
  operations each `-EROFS`), `ext4` (#292, #294: what the ext4 adapter encodes and
  decodes itself, a device node's `i_block` as `st_rdev` and Linux's signed times
  with their epoch bits, clamped as `timestamp_truncate` does; #330: the
  first block `ext4plus`'s `delete_file` frees, `first_block_freed`, and
  `Geometry`, a block's byte and its bitmap bit, `block_byte`,
  `bitmap_bit`; #355: `check_allocation`, a volume's allocation state
  checked over a reader (counts against bitmaps, every in-use inode's
  blocks and the fixed metadata owned once, live slots and directory
  entries against the inode bitmap, shared xattr refcounts) into an
  `AllocationReport` or an `AllocationError` (`Inconsistent`, or why it
  can't check), memory budgeted; `visit_inode_blocks`, `i_block_kind`/
  `i_block_kind_raw`, Linux's rule for what `i_block` holds, `has_super`;
  `testdata/allocation.ext4.zz`, made by `tools/ext4-allocation-image.py`),
  `nlink`
  (#265: `NlinkBounds`, ext4's in-memory link-count corrections: a floor
  and maybe a ceiling per inode, never trivial, the floor never above the
  ceiling; `raised_to`, after a raise on disk landed or failed), and `page`
  (#265: the page cache's arithmetic, `spans` (`PageSpans`),
  `pages_in`, `valid_len`, `resize_tail`; `short_count`, a read or write
  failing part-way, #321); `SetTime::resolve` (#302) in
  `types` (host-tested; `tests/ext4plus_htree_hash.rs`: the vendored
  ext4plus's htree hash against e2fsprogs', #313).
- `crates/lumen-futex`: futex wait queues as logic (M8, #338; ARCHITECTURE
  section 8): `FutexTable<K, W>` (an opaque key, FIFO per key: `enqueue`,
  `wake` with a bitset and Linux's count rule, `requeue`, `requeue_into`
  across buckets, `cancel`), caller-allocated `FutexNode`s, `Woken` drained
  after the bucket lock drops, `BucketIndex` (Linux's `lock_ptr` recheck:
  `cancel_in` finds a requeued waiter's bucket, `CancelOutcome::AlreadyWoken`
  for a wake that won), `check_requeue_counts`; the `futex(2)` protocols in
  its crate docs (host-tested, incl. a model, and no allocation in any queue
  operation, `tests/zero_alloc.rs`).
- `crates/lumen-signal`: Linux x86-64 signals as logic (M8, #339;
  ARCHITECTURE section 6): `Signal`, `SigSet`, `Blocked` (never `SIGKILL`
  or `SIGSTOP`), `DefaultAction`; `pending` (`PendingSignals`: a process's
  shared queue and one per thread, `PendingSignal`, `ThreadSlot`, `Target`, `SigInfo`
  (`fault` with `si_addr`, `timer`/`set_timer_overrun`), `SenderClass`, `Disposition`, stop/continue
  cancellation, `select_target`, timer enqueue/dequeue/discard); `timer`
  (#392: `Setting` with `realtime`, `forward`, `missed`, `delivered`,
  `enqueue_refused`, `alarm_seconds`, `tests/timers.rs`); `frame` (`build`: an `rt_sigframe` image,
  `FrameImage`; `restore`: `rt_sigreturn`'s checks, `Restored`,
  `ValidatedFp`, `FpFallback`, `RestoreConfig`); `restart` (`decide`:
  `ERESTART*` against the handler and `SA_RESTART`, `RESTART_SYSCALL`);
  `altstack` (`AltStack::configure`, `SS_AUTODISARM`); `abi` (the frame's
  layouts, pinned) (host-tested).
- `crates/lumen-pipe`: a POSIX pipe as logic (M8, #342; ARCHITECTURE
  section 10): `Pipe` (a byte ring of `DEFAULT_CAPACITY`, `set_capacity`
  for `F_SETPIPE_SZ`, `PIPE_BUF` writes all or nothing, `peek` and a
  `Peeked` committed after the copy out, reader and writer counts,
  `Released::Last`, `readable`/`writable`/`reader_hung_up`/`writer_error`,
  `fionread`; `fifo`, no ends open), `WriteProgress` (a blocking write's
  chunks and its wake rules; `WriteStep`, `InterruptedWrite`,
  `FaultedWrite`) (host-tested, incl. no allocation outside creation and
  `set_capacity`, `tests/zero_alloc.rs`).
- `crates/lumen-tty`: a terminal line discipline as logic, after Linux's
  n_tty (M8, #340; ARCHITECTURE section 10): `LineDiscipline` (`receive`
  each input byte: canonical editing, echo, `ISIG`'s `JobSignal`; `read`
  after `read_decision`'s `Decision` with a `ReadTimer` for
  `VMIN`/`VTIME`; `write` through `OPOST`; `set_termios` with `Flush`,
  `flush_input`, `set_winsize` saying whether `SIGWINCH` is due,
  `poll_readable`, `readable_bytes`, `accepts`), `Termios`
  (`console_default`), `Winsize`, the `Output` sink, `Effects`/`Wake`, and
  `background_write_signal` (`SIGTTOU` or `EIO` for a background writer)
  (host-tested, incl. no allocation after construction).
- `crates/lumen-proc`: the POSIX process tree as logic (M8, #341;
  ARCHITECTURE section 8): `ProcTree` (pids from 2 to a maximum, wrapping,
  never one in use as a pid, pgid or sid; `spawn_init`, `spawn_root`,
  `fork`, `exec`, `exit` with a reaper giving an `ExitOutcome` of
  `ExitAction`s (`Notify`, `Released`, `HangupThenContinue`), `stop`/`cont`
  giving a `StateChange`; `wait4_request`/`waitid_request` and `wait`
  (`WaitReady`, `Report`); `setsid`, `setpgid`, `getpgid`, `getsid`,
  `tcsetpgrp`, `tcsetpgrp_job_control`, `Controlling`, `Sigttou`,
  `Tcsetpgrp`, `PidRef`/`retain_pid` (#388), `pgrp_members`,
  `is_orphaned_pgrp`; a process's threads,
  `add_thread`/`remove_thread`/`threads`, their tids busy, #372;
  `nproc_task_count`, the tasks `RLIMIT_NPROC` counts, and
  `allows_new_task`, the soft-limit check, #435;
  `check_invariants` behind `test-util`), `WaitStatus` (`word`,
  `siginfo`), `ExitStatus`, `RusageSlot`/`SlotRef`, `Errno`/`Bug`/`Error`
  (host-tested, incl. a model, `tests/alloc.rs`: what each operation
  allocates, `tests/threads.rs` (#382), and `src/tests/pid.rs` (#341, #388)).
- `crates/lumen-trace`: lock-free trace ring (host-tested, incl. threads;
  `read_since`: a polling reader's events since a sequence number, each
  number copied or counted missed once), `kind` (every kind's number, `Stream` and
  `stream_of`: which ring each goes to, and typed layouts: `Switch`/`ThreadState`,
  `RunQueue`, `Irq`, `LockWait`), `TextRing`, the byte ring of log lines `dmesg` reads (not lock-free), `repeats` (`klog::hold_repeats`' table; `Seen::is_held`, the logger's print-or-hold),
  and `ThreadName` (#276: a thread's name, 16 bytes inline like Linux's `comm`: at most 15 bytes cut
  at a character boundary, control characters (C0, DEL, C1) and U+2028/U+2029 as `?`; `from_path`,
  a path's last component, the name a binary run by `linux /path` gets; `From<&str>`, the kernel's
  names, refuses one that wouldn't fit by a debug assertion; names written as literals at a call
  naming a thread are linted, `tests/harness/test_thread_names.py`; app names are at most 11 bytes).
- `crates/lumen-time`: timekeeping arithmetic: `slice_ticks` (nice-weighted
  scheduler slice, #377), calibration (windows'
  steadiness, the ones with the smallest bounds filling in, how far off a
  median can be for either rate, `tsc_error_ppm`, a window slow to read
  stretched, the verdict against each clock's tolerance, `within_bounds`,
  the stall knobs' parser and `stall_ticks`, their share of a window), tick/ns
  conversion, timer counts, log time formats (`Stamp`, `Millis`),
  percentages and rates, one clock's reading placed between two reads of
  another and retaken while too wide (`Bracketed`, `first_narrow`: the
  ktests' clock cross-checks), the RTC's registers as a date (`rtc`), and
  Limine's boot times against the timeline's origin (`handoff`: the
  `before kmain` line and its check, none a time bound) (host-tested).
- `crates/lumen-rand`: ChaCha20's block function (RFC 8439 vectors), the
  entropy pool (mixing, fast-key-erasure streams), and pure helpers for
  the sources: `retry`, `fill_words`, `all_same`, `below` (host-tested
  with fakes).
- `crates/lumen-queue`: lock-free single-producer, single-consumer byte
  queue (interrupt handler to thread; host-tested, incl. threads).
- `crates/lumen-term`: terminal emulator core: `vte` escapes into a cell
  grid, scrollback, damage tracking (host-tested, incl. a randomized model
  renderer), `render::Renderer` (drawing only what changed on a
  `CellTarget`, the cursor solid or hollow: the console's and the
  terminal windows' renderer, #155), and `vt::Router`, a key ring per virtual terminal (the
  kernel fills them with each key routed to the terminal shown then).
- `crates/lumen-shell`: shell core: word splitting with quotes (checked
  against `shlex`; commands split at `|`), the line editor (history,
  completion; ANSI and plain output), the command table (names, completion
  words, usage and help lines), pipelines (text or records in tables, a
  cell that wouldn't read back as one word printed quoted (`quoted`); the
  filters `grep`, `where`, `sort`, `first`, `count`, `select`), serial
  bytes to keys, a key press to the line editor's key (`Key::from_press`,
  shared by text mode and terminal windows), `Key::Closed`/`Event::Closed`
  (a closed console's input) and `Key::interrupts` (Ctrl-C or Closed: what
  commands polling for a key stop on) (host-tested).
- `crates/lumen-acpi`: ACPI table parser: RSDP, XSDT/RSDT, MADT, HPET, MCFG,
  FADT (PM1 control blocks) and the DSDT's `_S5_` by byte search
  (host-tested on handcrafted tables).
- `crates/lumen-pci`: PCI configuration space behind a `ConfigSpace` trait:
  headers, BAR sizing, the capability list walked with a loop guard, a
  brute-force multi-function scan into `DeviceInfo`, ECAM offsets, the
  warnings' text (`Problem`), names for q35's and the virtio devices,
  finding the MSI/MSI-X capability a write would land in
  (`CapList::interrupt_cap_at`, `msi_len`), MSI-X's table location and
  entry encoding (`msix`), and virtio's capabilities with
  the checks around `PciTransport::new` (`virtio`) (host-tested against a
  fake configuration space).
- `crates/lumen-uart`: 16550 UART logic: presence probe (scratch or
  interrupt enable register), FIFO detection, `\n` as `\r\n` in 15-byte
  bursts (one FIFO slot left for the crash reporter's byte), the receive
  interrupt's setup, the handler's drain bounded by its queue's room, and
  the reader's policy (`read_next`, `available`) (host-tested against a
  model UART, the reader also against an interrupt that comes only on an
  edge).
- `crates/lumen-png`: PNG for Lumen: the encoder `screenshot` uses
  (`encode`: RGB8 `Rows` into a `Png`, unfiltered, zlib from
  `miniz_oxide`, the rows' and the file's buffers refused rather than
  aborted, the compressor's own boxes excepted), the decoder the
  wallpaper and the viewer app use (`decode`: any PNG into `0x00RRGGBB` pixels with
  `zune-png`, only the image's own chunks handed to it; a bad, truncated,
  corrupt or too big file an error, never a panic), CRC-32, and
  the base64 frame a PNG travels in over serial, streamed from the stack
  (host-tested, incl. a refusing allocator, a golden file PIL decodes,
  every truncation and every flipped byte of a file).
- `crates/lumen-app-api`: the WASM app interface's types (#199, section 8),
  shared by the kernel and the apps (built for wasm32 too): `Error` codes,
  `LogLevel`, `Kind`, `Grant`, `Listable`, `Grants`, `manifest::find`
  (walking the module's sections itself, never panicking), `limits` (each
  app's worst case `APP_WORST`, `APP_OVERHEAD`'s named parts, checked at
  compile time; `slots`), `handles::HandleTable` (ids never reused),
  `range` (guest ranges), `text` (`LogText`: log text cut and sanitised,
  line separators too), `end` (`Outcome`, `EndReason`, `EndCell`: the
  first recorded wins; `CloseGrace`), `host` (the `lumen` functions'
  signatures, `ENTRY`, `MEMORY`, all pinned), `trace` (`trace_read`'s
  layouts), `args`, `layout` (#206: `Canvas`, `Rect`, `TextDraw` and their
  checks: `Canvas::pixels` giving a `CheckedCanvas`, the one way to a
  clip's rows, `Rect::clip` giving a `Clipped`, `commit_count`), `event`
  (#206: `Event`, the wire type `window_next_event` hands an app,
  postcard; its wire tables `ThemeId`, `KeyCode`, `Mods`/`mods`,
  `Buttons`/`buttons`, `SetValue`, and the conversions from `lumen-input`'s,
  `lumen-wm`'s and `lumen-ui`'s types and back, `toolkit_key`), `tree` (#207:
  `Tree`, what `window_publish` takes: role-fused `Node`s in pre-order,
  decoded node by node and checked, `from_automation`/`rows` both ways to
  lumen-ui's rows), `answer` (#207: `AppAnswer`, `Refusal`), `request`
  (#207: `take`, `Running`: a window's request on the kernel's side), `IdMap`
  (#207, in `tree`: a window's ids both ways), `wire`
  (`wire_enum!`, `DecodeError`, `decode_exact`, `Capped`, `CappedVec`), `sys`
  (the `lumen` imports, declared once for the SDK and testkit; wasm32 only)
  (host-tested, incl. `tests/decode_peak.rs`: the heap's peak under hostile
  lengths, and `tests/publish_peak.rs`: `window_publish`'s steps on the
  worst trees against `TREE_COPIES_BYTES`, counting allocators).
- `crates/lumen-calc`: the calculator app's logic (#207): keys in, the
  display out, left to right in exact decimal fixed point (host-tested).
- `crates/lumen-clock`: the clock app's logic (#200): `TimeOfDay` (from
  Unix seconds, `--at`'s `H[H]:MM[:SS]`, `HH:MM:SS` shown), an integer sine
  table in half-degree steps (no floats), `Hands`, `point` and `line`
  (Bresenham) on the face; `face::Face`, the clock's window as a
  `lumen_ui::App` (its drawing and its hands' boxes as damage, checked
  against full repaints); `args` (`run clock`'s arguments); `rtc` (what it
  shows while RTC reads fail, as the taskbar's clock) (host-tested).
- `crates/lumen-edit`: the text editor app's logic (#203): a buffer with a
  cursor, edits undone and redone (a typing run as one), the text's, a
  line's and the history's `Limits`, `normalise`, `Loaded`, the status
  line, a view's scroll, the `Change` naming the lines to redraw, the keys
  (`command`: undo and redo by the layout's letters) and the view across
  (`view`: bounded by what it shows) (host-tested, incl. a randomized model
  against a naive editor and real keymaps through `lumen-input`).
- `crates/lumen-snake`: Comet's logic (#201, an original snake-style
  game): the sky's cells, steering, growth, the edge, tail and asteroids
  ending a flight, levels (`rocks`, sparing the cells ahead), stardust on
  empty cells, `Rng` (seeded SplitMix64: one seed, one game), `Game::step`
  reporting exactly the cells it changed (the app's damage), the labels'
  texts and `LABEL_W` (each measured in the UI font) (host-tested).
- `crates/lumen-view`: the image viewer app's logic (#204): an image at 1:1
  (`actual`, `Pan`) or fitted (`fitted`, `scale`: nearest neighbour), what
  it decodes (`admit`: the file's length, then the size and inflated bytes
  from the header), `prepare` (its one call for an image: `Prepared`, or
  `Why`, out of memory told apart), and its memory plan (`MEMORY_BYTES`;
  `tests/memory.rs` runs `prepare` on the bundled images, worst-case PNGs
  and the longest file through `lumen-heap` over its heap size)
  (host-tested).
- `crates/lumen-synth`: sound synthesis for the shell's `beep` and
  `play`: fixed-point oscillators (square, triangle, sine table,
  LFSR noise), ADSR envelopes, saturating mixing, the melody notation
  (`t=140 w=sine C4/8 r/8`), and `Source`s that fill interleaved 16-bit
  PCM buffers (host-tested).
- `crates/lumen-xray`: the X-ray app's logic (#208): trace events read from
  each CPU's main and X-ray rings into per-CPU state (`View`, `Cpu`:
  `Presence` unread, online or offline; the run queue from the newest `runq`
  event, the thread from the newest `switch`, counts known from a ring's
  first read, numbers missed, odd events (undecodable, or a kind in the
  wrong ring by `stream_of`), where each ring's next read starts; a ring's
  backlog (its first read, after `resume`, after coming back online) giving
  only the newest queue and thread) and the lanes'
  columns (`Column`, `Span`: empty, counted, a backlog's gap, offline;
  `COLUMNS`); `replay`, the fixed trace of the frozen view (host-tested).
- `vendor/ext4plus`: ext4plus 0.1.0-rc.2's crate file with Lumen's fixes
  on top, each its own commit, its code marked `Lumen (#n)`
  (`[patch.crates-io]`, out of the workspace; #313: the htree hash's
  signedness from `s_flags`; #350: the allocators' search bounded to one
  pass over the groups, only `NoSpace` retried shorter, a count/bitmap
  mismatch reported as its own error).
- `apps/`: the WASM apps (M5), their own cargo workspace (out of the root
  one; every directory but dot ones a member, named after its crate; output
  in `target-apps/`) for `wasm32-unknown-unknown`, `no_std`
  `cdylib`s; `make build` (`make apps`) builds each into
  `build/apps/<name>.wasm`, the runtime's test apps (`test-<name>`) into
  `build/apps/tests/`, skipping libraries (`lumen-app`, the SDK, #206;
  `testkit`); `template`, the app `make app NAME=` copies
  (`tools/new-app.sh`); `calculator` (#207, the first app on the SDK's
  `ui::Toolkit`, lumen-ui inside the app); `clock` (#200, an analog face
  on `run_until`, `--at` freezing it); `comet` (#201, the snake-style
  game on `run_until`, `--seed N --step` for tests); `editor` (#203, on `lumen-edit`); `life` (#202, Conway's Game of Life on
  `run_until`, lumen-demo's rules; the launcher's Life); `viewer` (#204, bundled images
  decoded inside the app by `lumen-png`, `apps/viewer/images/` made by
  `tools/viewer-images.py`); `xray` (#208, the trace rings
  live, `--replay` frozen); the SDK's loops (`run_with`'s
  `Taken`, `run_until` for a `Ticking` app, both returning `Ended`;
  `Event::offer`; `with_area`, `palette`) for the animated apps; the
  layout, the SDK, the
  test apps and `testkit` are described once, in ARCHITECTURE section 8
  ("Building and bundling"). `make lint` and `fmt` cover them. The kernels embed them
  (`app::BUNDLED`, `app::TEST_APPS`); the harness validates them in
  `wasmtime` and checks the app interface's shape.
- `tools/harness/lumen_harness/`: `qemu.py` (the only place QEMU command lines
  are built; virtio devices through `Devices`; `press`/`type_text` for keys,
  `move_pointer`/`click` for the tablet), `qmp.py`, `tap.py`, `image.py`,
  `audit.py` (also checks `.lumen_fixup_nofault`, #373), `status.py`,
  `wasm.py` (the built apps checked: `wasmtime` validation with the
  proposals past rustc's defaults that wasmtime-py can switch off
  switched off (`OFF`, each a real `Config` setting), and a reader for custom sections,
  a name twice refused: `target_features`, the `name` section's functions),
  `crash.py` (crash-report parsing), `audio.py` (wav files as QEMU's
  wav audiodev writes them: pitch by zero crossings, Goertzel amplitude),
  `screen.py` (the screen's rows through the serial shell's `term text`, a
  terminal window's through `term text <window>`),
  `gui.py` (input steps confirmed one at a time: `pointer_move`,
  `pointer_down`/`pointer_up`, `drag` against `mouse`'s frames and state,
  `confirmed_keys`/`type_confirmed` against `kbd`'s presses, bytes and
  held keys, `KEY_BYTES` the keys it models; the desktop's `windows`,
  `desktop_status`, `wait_settled`, `desktop_check`, `settled_screendump`;
  `wait_window`, `wait_no_window`, `close_window` (`ui invoke`'s close, then
  the window's going), `TITLE_H`, `client_box`, `client_pixels`;
  the automation tree's `ui_tree`, `node`, `wait_ui`, `invoke`,
  `press_buttons` (each answered, or still running and then settled: `Invoked.still_running`),
  `press_button`, `press_animated` (in a window that may animate: confirmed by `wait_ui`, never
  settling; `invoke`'s `animating` retries a held slot instead of settling, `INVOKE_HELD_TIMEOUT`),
  `desktop_counter`; `parse_records` reads quoted cells with `shlex`),
  `calibration.py` (a boot calibration's verdict line, and the TSC's
  HPET-or-PIT choice, read back and checked against the numbers they print:
  `judged`, `verdict`, `weighed`, `no_other_warnings`, `warned_exactly_when_past`;
  `fallback`, `tsc_reference`, `unexpected_boot_warnings`, `assert_quiet_boot`;
  the clocks' tolerances, and `agree`: two rates within 2% or their bounds,
  `CANT_TELL_PPM` the most those may add up to), `clock_diags.py` (the
  clock cross-check ktests' diagnostics, `check_clock_diags`),
  `census.py` (the flake census: CI's red first attempts per day, how many
  passed on a re-run, and the tests that failed them, from the failed jobs'
  logs through `gh`; `make census`, and weekly in the `flake-census`
  workflow's summary),
  `app_limits.py` (#278: `lumen_app_api::limits`' `MAX_APPS`,
  `KERNEL_RESERVE` and `APP_WORST` copied, checked against limits.rs by
  `test_app_limits`, and `slots`, the count `run` works out from the frames
  `mem` says the allocator owns),
  `screenshot.py` (`screenshot`'s frame read back and checked;
  `Machine.screenshot_via_serial`),
  `shell.py` (`run`: a serial command's output, the log's lines taken out
  and kept, a WARN or ERROR failing it unless allowed or host-timed;
  `PROMPT`), `klog.py` (the kernel log line's format, as kernel/src/klog.rs
  prints it; `HOST_TIMED`, the warnings a loaded host can cause on its own,
  each tied to the kernel's text — incl. the 2 s TLB shootdown stall warning, #376 — and `unexpected_warnings`, a run's "no
  warning" check without them; `HOLDING`, `HELD_BACK`, `HELD_ALL`: a
  `klog::hold_repeats` guard's lines), `rootfs.py` (matched `e2fsprogs` discovery,
  per-test `alpine-rootfs.ext4` (`copy_rootfs`) and `alpine-dev.ext4`
  (`copy_dev_rootfs`, CoW clone via `cp -c`/`--reflink=auto`, #344) copy
  isolation, `e2fsck -fn`, `debugfs`
  and `debugfs_names` (reading an image back, any error line failing),
  `llvm-readobj` dynamic musl ELF checks, `run_busybox`: a `linux
  /bin/busybox` run's output, exit code and leaked frames (`BusyboxRun`,
  `BUSYBOX_EXIT`), #324, and `warm_up`: one uncounted
  `linux /bin/busybox true`, leaking 0 frames or 1, before exact counts),
  `syscalls.py` (#337: M8 syscall census — stdlib-only `build` turning
  `strace -f -X raw` traces into `tests/linux/m8-census.toml`, and `report`
  comparing it against the kernel's dispatch table and planned milestones),
  `libctest.py` (#345, #396: musl libc-test transcripts, one
  `EXIT <path> <code>` line per test, read as `Outcome` (`PASS`, `FAIL`,
  `TIMEOUT`, `ERROR`), a runner error never a verdict;
  `tests/conformance/libc-test-xfail.txt`, each entry a reason and an issue
  with optional `flaky` kind, a listed pass failing the run; single retry on
  timeouts and flaky failures with `RETRY ` prefix; `NATIVE_RETRY_FAILURES`,
  #422; the built binaries' linkage checked; `run_native`, the host
  reference run, `runtest -t 30`).
- `tools/fetch-zig.sh` (`make deps-zig`, #368): zig 0.15.2 into
  `build/deps/zig`, sha256-pinned per host, ziglang.org then two community
  mirrors, installed under `build/deps/zig.lock`; a C cross-compiler for
  host-built Linux test binaries only. `tools/build-libc-test.sh` (`make
  libc-test`, #369, after `tools/fetch-libc-test.sh`'s pinned checkout):
  libc-test's `src/functional` and `src/regression`, static and dynamic,
  with `zig cc -target x86_64-linux-musl` into `build/libc-test/`
  (`tests.txt`, `not-built.txt`). `tools/syscall-census.sh` and
  `tools/census/workloads.sh` (#337): Docker/Podman strace census on stock
  Alpine x86-64 generating `tests/linux/m8-census.toml`.
- `tools/mkrootfs.sh`, `tools/mkrootfs-dev.sh`, `tools/linux-m7/`,
  `tools/linux-m8/`: deterministic `build/alpine-rootfs.ext4`
  assembly (`s_flags` saying signed directory hashes on every host,
  `tools/ext4-signed-hash.sh`, #308); `tools/mkrootfs-dev.sh` (`make
  rootfs-dev`, #344) builds `build/alpine-dev.ext4` from the minirootfs,
  `build/linux/*`, and the Alpine v3.21 closure of `gcc`, `g++`, `make`,
  `musl-dev`, `binutils`, `git`, `sqlite`, `vim` resolved and unpacked by
  `tools/alpine-lock.py` (`tools/alpine-packages.{txt,lock}`), fetched by
  `tools/fetch-alpine-packages.sh` (`make deps-dev`) with durable GitHub
  release mirror `tools/mirror-alpine-packages.sh` (`alpine-apk-mirror`);
  `tools/linux-m7/` `#![no_std]` dynamic musl test binaries (`m7_test`, its modes
  for #255's exit scenarios, and `libm7.so`;
  `m7_vfs`, #254's exit scenarios' program: `posix`, `locks`, `keep` (a
  tree left for fsck), and `lock-hold`/`lock-probe`, two processes at once,
  every check a raw syscall by number); `tools/linux-m8/` (#371): M8's
  test programs, built the same way into `build/linux/` and staged in both
  images, sharing `common.rs` (the raw `syscall`, `say!`, argv, `Fail`):
  `m8_proc` (`fork-wait`, plus submodules `exec.rs` (#381), `signals.rs`
  (#384), `signal_waits.rs` (#385), `futex.rs` (#383, #390), `threads.rs`
  (#382, #390), `sessions.rs` (#387), `timers.rs` (#392), `small.rs`
  (#391); and `posix-shm`/`posix-shm-fail`, its inline `posix_shm`:
  `shm_open` and named semaphores on `/dev/shm`, #433), `m8_io` (`console-poll`, #374, #389; `pipes`, #386), `m8_vm`
  (`shootdown`, #390), `m8_pty` (PTY integration checks, #388;
  `ptyrun-relay`/`ptyrun-child`, the relay's backpressure, partial writes
  and `SIGWINCH`, #437), and
  `ptyrun` (D3's PTY runner: `--script <file> -- <program>` scripted PTY
  master and interactive console-to-PTY relay forwarding `SIGWINCH`, #395, #437).
- `tests/scenarios/`: QEMU tests, incl. `test_linux_user.py` (M6/M7 ring-3
  entry, faults, ABI, busybox and `dlopen`), `test_linux_proc.py`
  (processes, `fork`/`exec`/shebangs, signals/`sigaltstack`, signal waits,
  `futex`/nanosleep, pthreads, sessions/groups, interval/POSIX timers, small
  syscalls, MT stress, POSIX shared memory and semaphores on `/dev/shm`
  #433), `test_linux_io.py` (`poll`/`select` on console,
  pipes/FIFOs, PTY termios/job control, `ash -i` console job control,
  `ptyrun`'s console relay, #437),
  `test_linux_vm.py` (cross-CPU `munmap`/`mprotect` TLB shootdowns),
  `test_alpine.py` (Alpine minirootfs coreutils plus M8's D1 `ash` #393,
  D2 `gcc`/`g++`/`make` #394, D3 `git`/`sqlite3` and `vim` over `ptyrun`
  #395 gates, and #355's allocation check on both images, no leaked
  blocks), `test_conformance.py` (D4 musl `libc-test` core against
  `tests/conformance/libc-test-xfail.txt`, #396), and `test_smp.py`
  (incl. MT stress, #390). `tests/harness/`: harness self-tests
  (incl. `test_rootfs.py`, `test_rootfs_dev.py`, `test_libctest.py`,
  `test_syscalls.py`, `test_ci_config.py`),
  and `test_scenario_typing.py`, a lint: a raw key in a scenario needs
  `# unconfirmed: <why>`, and lumen_harness types raw only where it
  implements it; `test_thread_names.py`, a lint: every thread name written
  as a literal at a call naming a thread in kernel/src fits in a
  `ThreadName` (15 bytes, #276), failing on a name argument it can't read
  and on a function taking a `ThreadName` it doesn't list.
  `tests/features/*.toml`: feature → test map (the CI feature gate and STATUS.md).
