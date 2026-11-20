# Lumen: notes for agents and contributors

Lumen is a hobby/teaching OS in Rust for x86-64 UEFI (Limine, QEMU q35 first).
Read `docs/ARCHITECTURE.md` before changing interfaces; `docs/ROADMAP.md` lists
milestones with test-based exit criteria; `docs/STATUS.md` is generated.

## Commands

```bash
make deps            # pinned Limine + OVMF into build/deps (sha256-checked)
make build           # the apps (build/apps/<name>.wasm, test apps build/apps/tests/), then both kernels: build/lumen-kernel, build/lumen-kernel-ktest
make app NAME=foo    # a new app: apps/foo copied from apps/template (tools/new-app.sh); `make build` bundles it
make run             # QEMU with a window, serial on your terminal (Ctrl-A x quits; HEADLESS=1; SOUND=1 for beep/play; DESKTOP=1 starts the desktop)
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
`rust-toolchain.toml`), `qemu-system-x86_64`, `xorriso`, `uv`, `make`, `curl`, and `gdb` for `make debug`
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
  (including 0 from a triple fault with `-no-reboot`) is a failure. The one
  exception: 0 in the power-off scenario, with a QMP `SHUTDOWN` event whose
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
  `lumen.log=debug`). `sync.rs`: `SpinLock` (interrupts off while held; a wait recorded as an X-ray `lock` event).
  `smp.rs`: starting the other CPUs, `run_on`. `sched.rs`: kernel threads
  (per-CPU run queues, preemption on the timer, `sleep`, `WaitQueue`
  with or without a deadline, `spawn`/`join`, `scope`: threads borrowing from the caller, joined before it returns;
  each run-queue change an X-ray `runq` event, `run_queue`: a queue's length both ways); `arch/x86_64/context.rs` switches threads.
  `arch/x86_64/`: GDT/TSS (per CPU), IDT, entry
  stubs and the vector map, `percpu.rs` (GS base, CPU index), `irq.rs` (nested interrupt disable), local and
  I/O APIC, 8259 and PIT registers, serial, IST stacks. `crash.rs`: crash
  reporter. `ktest.rs`: test runner (`lumen.ktest=` runs only the tests with given name prefixes). `script.rs`: rhai scripts (the shell's
  `rhai`; built-in scripts in `script/`). `shell/`: the shells, one on serial
  (plain text unless `lumen.ansi`), one on each virtual terminal (keyboard) and one in each terminal
  window (`run_in_window`, #155): `mod.rs` the consoles (`Console::place`: anywhere, a virtual terminal
  or a window; `closed`) and `Ctx` (keys through `poll_key`/`wait_key`, the screen
  through `canvas`), their commands (`commands.rs`: each row carries its
  help line; the system ones in `sysinfo.rs`), pipelines (`pipe.rs`: `cpus |
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
  (`lumen.taskbar_wake_delay_ms=` holds back a poke's handling, for the scenarios). `klog.rs`: logger (and the ring `dmesg` reads). `rtc.rs`: the CMOS RTC (`date`, `now_quiet` for pollers; `arch/x86_64/cmos.rs` its ports). `symbols.rs`: symbol lookup.
  `mm/frame.rs`: physical frame allocator, its reservations, `OwnedFrame`.
  `mm/window.rs`: all-or-nothing mapping of fresh frames into a window
  (heap and stacks), through `paging::map` with a `Window` token.
  `mm/heap.rs`: the `#[global_allocator]`, growing in the heap window.
  `mm/stack.rs`: kernel stacks with guard pages; `arch/x86_64/stacks.rs`
  holds their registry (backtraces, overflow reports).
  `mm/paging.rs`: the kernel's page tables (HHDM, W^X image, map/unmap,
  MMIO window, crash-report walk). `arch/x86_64/paging.rs`: CR3, TLB, NX/WP,
  PAT. `trace.rs`: trace rings (one per CPU), event kinds, `ktrace!`, the boot timeline
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
  rings, or the driver itself to wake its thread: `mouse::hold`). `rng.rs`: randomness: `rng::fill`, `u64`, `below`,
  `sources`, `stats` (the shell's `random`, rhai's `random()`), RDRAND and
  the virtio-rng driver (its own queue, one deadline per request, dropped
  on a failed request) feeding a ChaCha20 pool. `power.rs`: `reboot` and `shutdown` (ACPI S5);
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
  without a UART; the UART's one reader.
  `keyboard.rs`: the PS/2 keyboard (8042 init, IRQ 1, `pc-keyboard`
  decoding through `lumen_input::KeyDecoder`, each key event into
  `input::KEYS`, `lumen.keymap=`); `arch/x86_64/i8042.rs` its ports.
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
- `crates/lumen-demo`: fixed-point Mandelbrot, dealing image rows to CPUs
  in bands, and Game of Life with its patterns (host-tested; the `life`
  command's and the Life app's, a path dependency of `apps/`, #202).
- `crates/lumen-elf`: ELF symbol-table reader (host-tested).
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
  (`Ui`: stacks (`Stack` handles), labels (`fixed_label`), buttons, checkboxes, text fields, lists
  (`set_selected`: the app's own choice, the focus left where it is),
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
  window tokens that confine `paging::map`, and the kernel-stack layout and
  lock-free stack registry.
- `crates/lumen-heap`: heap allocator logic (address-ordered free list,
  coalescing; host-tested, incl. a randomized stress test against a shadow map).
- `crates/lumen-trace`: lock-free trace ring (host-tested, incl. threads;
  `read_since`: a polling reader's events since a sequence number, each
  number copied or counted missed once), `kind` (every kind's number, `Stream` and
  `stream_of`: which ring each goes to, and typed layouts: `Switch`/`ThreadState`,
  `RunQueue`, `Irq`, `LockWait`), and `TextRing`, the byte ring of log lines `dmesg` reads (not lock-free).
- `crates/lumen-time`: timekeeping arithmetic: calibration (windows'
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
  game on `run_until`, `--seed N --step` for tests); `life` (#202, Conway's Game of Life on
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
  `move_pointer`/`click` for the tablet), `qmp.py`, `tap.py`, `image.py`, `audit.py`, `status.py`,
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
  `screenshot.py` (`screenshot`'s frame read back and checked;
  `Machine.screenshot_via_serial`),
  `shell.py` (`run`: a serial command's output, the log's lines taken out
  and kept, a WARN or ERROR failing it unless allowed or host-timed;
  `PROMPT`), `klog.py` (the kernel log line's format, as kernel/src/klog.rs
  prints it; `HOST_TIMED`, the warnings a loaded host can cause on its own,
  each tied to the kernel's text, and `unexpected_warnings`, a run's "no
  warning" check without them).
- `tests/scenarios/`: QEMU tests. `tests/harness/`: harness self-tests,
  and `test_scenario_typing.py`, a lint: a raw key in a scenario needs
  `# unconfirmed: <why>`, and lumen_harness types raw only where it
  implements it.
  `tests/features/*.toml`: feature → test map (the CI feature gate and STATUS.md).
