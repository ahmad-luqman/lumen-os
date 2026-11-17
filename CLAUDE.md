# Lumen: notes for agents and contributors

Lumen is a hobby/teaching OS in Rust for x86-64 UEFI (Limine, QEMU q35 first).
Read `docs/ARCHITECTURE.md` before changing interfaces; `docs/ROADMAP.md` lists
milestones with test-based exit criteria; `docs/STATUS.md` is generated.

## Commands

```bash
make deps            # pinned Limine + OVMF into build/deps (sha256-checked)
make build           # both kernels: build/lumen-kernel, build/lumen-kernel-ktest
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
- **Golden images** live in `tests/golden/`. Re-record with `make golden` only
  when the change to the screen is intended, and open the PNG to check it.

## Map

- `kernel/src/main.rs`: boot sequence. `boot.rs`: the Limine requests
  (and, by hand, the TSC-frequency and bootloader-performance ones;
  `before_kmain`).
  `cmdline.rs`: kernel command line (flags and `key=value`, e.g.
  `lumen.log=debug`). `sync.rs`: `SpinLock` (interrupts off while held).
  `smp.rs`: starting the other CPUs, `run_on`. `sched.rs`: kernel threads
  (per-CPU run queues, preemption on the timer, `sleep`, `WaitQueue`
  with or without a deadline, `spawn`/`join`); `arch/x86_64/context.rs` switches threads.
  `arch/x86_64/`: GDT/TSS (per CPU), IDT, entry
  stubs and the vector map, `percpu.rs` (GS base, CPU index), `irq.rs` (nested interrupt disable), local and
  I/O APIC, 8259 and PIT registers, serial, IST stacks. `crash.rs`: crash
  reporter. `ktest.rs`: test runner. `script.rs`: rhai scripts (the shell's
  `rhai`; built-in scripts in `script/`). `shell/`: the shells, one on serial
  (plain text unless `lumen.ansi`) and one on each virtual terminal (keyboard): `mod.rs`
  the consoles and `Ctx` (keys through `poll_key`/`wait_key`, the screen
  through `canvas`), their commands (`commands.rs`: each row carries its
  help line; the system ones in `sysinfo.rs`), pipelines (`pipe.rs`: `cpus |
  where busy > 50`) and deliberate faults (`crash.rs`). `fb.rs`:
  framebuffer pixels, the header (`draw_header`, again when the desktop is left) and the crash screen. `term.rs`: the
  terminals on the framebuffer (four virtual ones, one shown: log lines on the first, the screen shells, scrollback, `view_text`, and
  `Canvas`: the pixels below the header for one command, the terminals paused meanwhile;
  and the mouse pointer, a save-under arrow that drawing over it takes off and puts back;
  `take_screen`: the desktop's `ScreenCanvas`, the whole screen, a row per hold of the screen's lock;
  `leave_desktop`). `vt.rs`: the keyboard's keys to the shown virtual terminal's shell, Alt+F1 to F4 switching;
  the decoding pass (`decode_pending`), `desktop_shown()` (keys the desktop's alone while it is), `cut()` (a
  copy of where each pause and resume falls in the input rings, with the `pause_owns_*`/`after_resume_*`
  predicates) and `enter_desktop`.
  `desktop.rs`: the desktop (#153): the `compositor` thread (the back buffer, damage-only presents, the
  pointer drawn on the screen only, both input rings through `lumen_wm::Wm`, the cut), windows
  (`open_window`, `WindowHandle`: an owner's surface and a committed one, `commit`, `next_event`/`wait_event`,
  `Desktop::without_screen` for the ktests), the two demo windows, `desktop` (`status`, `check`, `spin`,
  `hold`/`release`) and `windows`; `lumen.desktop_cut_delay_ms=` widens the race at a pause for the scenarios. `klog.rs`: logger (and the ring `dmesg` reads). `rtc.rs`: the CMOS RTC (`date`; `arch/x86_64/cmos.rs` its ports). `symbols.rs`: symbol lookup.
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
  `demo.rs`: the `mandelbrot` (bands on every CPU) and `life` commands.
  `font.rs`: the desktop's anti-aliased text (`lumen-font`): `font::face`,
  the UI font parsed on first use and shared; `font` renders the golden
  sample twice (timings, cache, golden check), as its ktest does.
  `screenshot.rs`: `screenshot`: the screen captured (`capture`: the
  desktop's back buffer while it is shown, `desktop::capture`; else the
  framebuffer read once under the screen's lock, `term::read_pixels`,
  which says if a canvas held the screen), encoded on a 256 KiB stack, sent as a base64 frame
  on serial (the format: ARCHITECTURE section 11).
  `sound.rs`: the virtio-sound driver (its `sound` thread owns the device,
  playing `lumen-synth` sources paced by the device), `beep`, `play`,
  `volume` and `sound`.
- `crates/lumen-demo`: fixed-point Mandelbrot, dealing image rows to CPUs
  in bands, and Game of Life with its patterns (host-tested).
- `crates/lumen-elf`: ELF symbol-table reader (host-tested).
- `crates/lumen-font`: anti-aliased TrueType text: `fontdue` (no `simd`)
  behind `Face` (parsed once, shared read-only; `measure`) and `Font`
  (a face plus its own glyph cache keyed by (glyph, pixel size), bounded
  in bytes, least recently used evicted), one-line layout (`render`,
  `render_mask` into a `LineMask`) into plain coverage masks, and the
  embedded UI font, Atkinson Hyperlegible (`assets/fonts/`, SIL OFL)
  (host-tested, incl. a model LRU, a `kern` fixture and a golden mask
  whose hash the kernel's ktest also checks).
- `crates/lumen-gfx`: the desktop's pixel core: `Rect` (signed origin,
  intersect/union/clip, subtract into at most 4), `Damage` (a fixed-size
  list that coalesces, a bounding box past its size, never loses
  coverage), `Surface` (xRGB8888 with a stride: fill, clipped blits,
  `blend_mask`/`blend_mask_in` for coverage masks such as glyphs, the
  latter clipped to a damaged rect) and `Target`, what a surface is
  presented to a span at a time (a surface; the desktop's front buffer,
  `term::ScreenCanvas`) (host-tested, incl. randomized coverage and
  every-edge clipping).
- `crates/lumen-wm`: the desktop's window-manager logic (#152): frame
  geometry (`Metrics`: title bar, close box, client, borders; `Part`),
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
  requests; sticky `Lost`/focus/`Close` flags delivered in place and never
  pushable, presses, moves and requests rationed past 48 of 64, releases
  only after their press, unpaired releases counted apart: keys, buttons
  forgotten, buttons never pressed), `deliver` (a `Delivery` onto its
  window's queue) and the light and dark `Theme`s (host-tested, incl. a
  randomized model of damage-only repainting with focus, and
  press-pause-resume-release through the queue).
- `crates/lumen-input`: pointer input: evdev events into whole-frame
  pointer states, axes scaled to the screen (`Tablet`, `AbsAxis`), a
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
- `crates/lumen-trace`: lock-free trace ring (host-tested, incl. threads),
  and `TextRing`, the byte ring of log lines `dmesg` reads (not lock-free).
- `crates/lumen-time`: timekeeping arithmetic: calibration (windows'
  steadiness, the ones with the smallest bounds filling in, how far off a
  median can be for either rate, `tsc_error_ppm`, a window slow to read
  stretched, the verdict against each clock's tolerance, `within_bounds`,
  the stall knobs' parser), tick/ns
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
  renderer), and `vt::Router`, a key ring per virtual terminal (the
  kernel fills them with each key routed to the terminal shown then).
- `crates/lumen-shell`: shell core: word splitting with quotes (checked
  against `shlex`; commands split at `|`), the line editor (history,
  completion; ANSI and plain output), the command table (names, completion
  words, usage and help lines), pipelines (text or records in tables, the
  filters `grep`, `where`, `sort`, `first`, `count`, `select`), serial
  bytes to keys, a key press to the line editor's key (`Key::from_press`,
  shared by text mode and terminal windows, with #155) (host-tested).
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
  aborted, the compressor's own boxes excepted), CRC-32, and
  the base64 frame a PNG travels in over serial, streamed from the stack
  (host-tested, incl. a refusing allocator and a golden file PIL decodes;
  decoding is #154's).
- `crates/lumen-synth`: sound synthesis for the shell's `beep` and
  `play`: fixed-point oscillators (square, triangle, sine table,
  LFSR noise), ADSR envelopes, saturating mixing, the melody notation
  (`t=140 w=sine C4/8 r/8`), and `Source`s that fill interleaved 16-bit
  PCM buffers (host-tested).
- `tools/harness/lumen_harness/`: `qemu.py` (the only place QEMU command lines
  are built; virtio devices through `Devices`; `press`/`type_text` for keys,
  `move_pointer`/`click` for the tablet), `qmp.py`, `tap.py`, `image.py`, `audit.py`, `status.py`,
  `crash.py` (crash-report parsing), `audio.py` (wav files as QEMU's
  wav audiodev writes them: pitch by zero crossings, Goertzel amplitude),
  `screen.py` (the screen's rows through the serial shell's `term text`),
  `gui.py` (input steps confirmed one at a time: `pointer_move`,
  `pointer_down`/`pointer_up`, `drag` against `mouse`'s frames and state,
  `confirmed_keys`/`type_confirmed` against `kbd`'s presses, bytes and
  held keys, `KEY_BYTES` the keys it models; the desktop's `windows`,
  `desktop_status`, `wait_settled`, `desktop_check`, `settled_screendump`),
  `screenshot.py` (`screenshot`'s frame read back and checked;
  `Machine.screenshot_via_serial`),
  `shell.py` (`run`: a serial command's output, the log's lines taken out
  and kept, a WARN or ERROR failing it unless allowed; `PROMPT`), `klog.py`
  (the kernel log line's format, as kernel/src/klog.rs prints it).
- `tests/scenarios/`: QEMU tests. `tests/harness/`: harness self-tests.
  `tests/features/*.toml`: feature → test map (the CI feature gate and STATUS.md).
