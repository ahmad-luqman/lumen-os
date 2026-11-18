# Lumen architecture

Lumen is a hobby and teaching operating system: AI-native, observable, and
safe by default. It targets x86-64 UEFI machines (QEMU first, then a real
laptop) and is written in Rust. This document is the design of record. It
describes both what exists today and the interfaces that later milestones must
fit into. Sections say **(built)** or **(planned)**. What is actually verified
is listed only in [STATUS.md](STATUS.md), which is generated from tests.

## 1. Principles

1. **Tests define truth.** A feature works when an automated test in CI says
   so, and not before ([hard rules](../CLAUDE.md#hard-rules)).
2. **No ambient authority.** Every process, and especially every agent, can
   touch only what it holds a capability for. There is no global filesystem
   root or "network on" switch.
3. **Everything can be undone.** Storage is copy-on-write with cheap
   snapshots, so an agent session can be rolled back exactly.
4. **Everything can be seen.** An always-on trace ring buffer feeds the X-ray
   view, and every crash produces a readable report.
5. **Boundaries before speed.** Subsystems talk through narrow, message-shaped
   interfaces, so parallel contributors (human or agent) can work without
   stepping on each other, and components can later move between kernel and
   user space.

## 2. Key decisions

| Decision | Choice | Why | Alternatives considered |
|---|---|---|---|
| Language | Rust, **stable only**, pinned in `rust-toolchain.toml` (1.99.1) | Memory safety removes whole bug classes, which matters with many agents writing kernel code in parallel. Stable-only means the build still works next year. | C17 with strict warnings and sanitizers in host tests: simpler interop and faster builds, but it relies on reviewer discipline for memory safety. Nightly Rust: `x86-interrupt` ABI and custom test frameworks, but it breaks over time. |
| Target | x86-64 UEFI, QEMU `q35` + OVMF, virtio devices first | Closest to a modern laptop; virtio is simple and well documented. | BIOS (legacy), ARM64 first (M13 port instead). |
| Bootloader | Limine 12.x (pinned binary release + sha256), `limine` crate **0.5.0** | Memory map, framebuffer, higher-half direct map, and our own ELF file for symbolization, with no loader to write. Crate 0.6.x requires nightly (`#![feature(ptr_metadata)]`), so we use 0.5.0 (base revision 3). | Custom UEFI loader (optional learning milestone), GRUB/multiboot2. |
| Kernel shape | **Modular monolith** with message-shaped internal interfaces; capabilities enforced at the syscall boundary | One address space keeps early milestones fast and debuggable. Agent safety needs capability checks between *processes*, which a monolith enforces just as well. Drivers and services expose request/response interfaces, so any of them can move to user space later. | Microkernel: stronger isolation of drivers, but it puts IPC performance and many extra servers in front of every milestone. |
| Test harness | Python (`uv`, pytest) on the host; TAP from the kernel | Fast to write scenarios, mature tooling, and QMP/PIL are easy to use. | Rust `xtask`: one language, but slower iteration on test scripts. |
| Third-party crates | Prefer a maintained `no_std` crate over writing our own when it does the job: pinned with `=` in the workspace `Cargo.toml` (`Cargo.lock` committed), a PR adding only the crates its issue names (with their own dependencies), each checked first to build for `x86_64-unknown-none` on the pinned stable toolchain, and `make audit` must pass with it linked (no SSE/AVX, no red zone, the kernel code model). Kernel crates so far: `limine`, `rustc-demangle`, `noto-sans-mono-bitmap` and `log` (from M0, before this policy; `log` is still `0.4`, to be pinned in a PR of its own), `pc-keyboard` (scancode decoding and layouts, #58, the first under it), `vte` (terminal escape parsing; brings `arrayvec` and `memchr`) and `unicode-width` (character widths), both #59. `rhai` (scripting, #67; integers only, no modules, no time, closures that can't capture (`no_closure`), `curry` disabled; brings `ahash`, `hashbrown`, `allocator-api2`, `equivalent`, `foldhash`, `smallvec`, `smartstring`, `thin-vec`, `num-traits`, `libm`, `bitflags`, `once_cell`, `portable-atomic`, `zerocopy`, `static_assertions`, `cfg-if`, `no-std-compat` and `core-error`, plus `const-random`, whose compile-time random seed stays linked but unused (the kernel sets a fixed hashing seed before any script runs), and the proc macros `rhai_codegen` and `const-random-macro`, which run only at build time). `virtio-drivers` `=0.13.0` (#95: virtqueues and the virtio device protocols; `default-features = false` with `alloc`; brings `safe-mmio` and `thiserror` (`bitflags` and `zerocopy` were already linked through `rhai`), plus the build-time proc macros `enumn`, `thiserror-impl` and `zerocopy-derive`), under the adapter rules in section 7, and `zerocopy` as a direct dependency, pinned to the lockfile's version (`=0.8.63`; the crate turns on `zerocopy`'s `derive` and `alloc` features for the whole kernel by feature unification, which `make audit` covers).  `shlex` (#62) is only a dev-dependency of `lumen-shell`, the word splitter's test oracle; `noline`, which #62 names, isn't used (no completion hook, escape codes always, and it queries the terminal for the cursor on every prompt). | Reach working, playable features sooner; crates with many users are better tested than a first version of our own. | Writing everything ourselves (more to learn, much slower); unpinned versions (a minor release could break the build or bring in SSE). |
| Firmware for tests | OVMF `edk2-stable202605-r1` from rust-osdev/ovmf-prebuilt, pinned with sha256 | macOS and CI boot byte-identical firmware, so golden screenshots match. | Distro OVMF packages (paths and versions differ per OS). |

## 3. Layers

```
 ┌──────────────────────────────────────────────────────────────────────┐
 │ Apps (WASM, sandboxed)   Shell   File manager   Editor   X-ray   Tour│  user mode
 ├──────────────────────────────────────────────────────────────────────┤
 │ Claude service: Ask bar, agent lanes, capability prompts, audit log │
 │ GUI: compositor, window manager, toolkit + automation API           │  user-mode
 │ System services: keychain, network stack, VFS server*, login        │  services
 ├──────────────────────────────────────────────────────────────────────┤
 │ Syscall ABI (capability handles)        IPC channels                │  boundary
 ├──────────────────────────────────────────────────────────────────────┤
 │ Kernel core: scheduler, processes, VM, capability tables, IPC,      │
 │ trace ring buffer, crash reporter, logger                           │
 │ Kernel subsystems: VFS + LumenFS (CoW), block cache, drivers        │  kernel
 │ (virtio-blk/net/input/gpu/console, PS/2, NVMe, xHCI...)             │
 ├──────────────────────────────────────────────────────────────────────┤
 │ HAL (`kernel/src/arch/`): CPU, interrupts, paging, timers, ports    │
 ├──────────────────────────────────────────────────────────────────────┤
 │ Limine (UEFI) → kernel ELF at 0xffffffff80000000                    │  boot
 └──────────────────────────────────────────────────────────────────────┘
   * starts in-kernel; the interface allows moving it out later
```

This is the target. To reach a usable system sooner, everything above the
boundary (the shell, the GUI, WASM apps, the network stack and the Claude
service) runs in the kernel on kernel threads until M9 adds user mode; see
[ROADMAP.md](ROADMAP.md#how-this-roadmap-is-ordered).

Module boundaries inside the kernel are enforced by Rust visibility. Code
outside `arch/` may only use what `arch/mod.rs` re-exports (the HAL surface).
Drivers are reached only through their module's functions (M3), later
through their device-class trait (section 7). Pure logic that does
not touch hardware goes in `crates/` as `no_std` crates with host-side unit
tests (for example `lumen-elf`, the symbol-table reader).

## 4. Boot (built)

1. **UEFI firmware (OVMF)** loads `EFI/BOOT/BOOTX64.EFI` (Limine) from the ISO.
2. **Limine** reads `boot/limine/limine.conf`, loads the kernel ELF, sets
   1280x800 (fixed so screenshots are comparable), builds page tables (the
   higher-half direct map at `0xffff800000000000`, the kernel at
   `0xffffffff80000000`, and no lower-half identity map at base revision 3),
   answers our requests (`kernel/src/boot.rs`), and jumps to `_start`.
3. **`_start`** (naked asm): `xor ebp, ebp` ends the backtrace chain, the stack
   top is passed to `kmain`, and the stack is aligned. `kmain`'s first
   statement reads the TSC, the boot timeline's origin (below).
4. **`arch::init_early`**: COM1 serial, then the boot CPU's GDT and TSS, its
   per-CPU record (the GS base, section "Other CPUs"), then the IDT
   with all 256 vectors. **From this point a CPU exception produces a crash
   report**, except a #DF caused by a broken stack pointer before step 6
   (#DF, NMI and #MC get their IST stacks there), which is a triple fault.

   Serial (`arch::serial`, the logic in the host-tested `lumen-uart`
   crate) is set up only if a UART answers: its scratch register holds two
   patterns, or, without one (an 8250), its interrupt enable register does.
   Without a UART, serial writes do nothing, reads find nothing, and boot
   warns on the screen once the framebuffer is up (a golden checks that row
   with `-serial none`); only the screen's copy of the log remains then:
   info and above, from the framebuffer on. Writes come in two kinds.
   Bursts: with a working FIFO (a 16550A), up to 15 bytes per wait for an
   empty transmit FIFO (THRE), one slot short of the FIFO. They assume no
   other burst goes in meanwhile, so only the log writes them, holding its
   line lock: the logger itself, and `klog::LockedSerial` for the serial
   shell, the ktest runner's TAP lines and the `crash` command's messages
   (two CPUs bursting after the same check would overrun the FIFO; not
   tested on its own: QEMU sends each byte as it is written, so its FIFO
   never fills). Byte by byte (`SerialWriter`): one wait per byte, without
   a lock, for the crash reporter, whose stop of the other CPUs is best
   effort: a CPU it couldn't stop can still log, and lines may interleave,
   but its burst and the reporter's byte after the same check fit the FIFO
   (the slot left free; model-checked by `cargo:lumen-uart`, not on
   hardware). On a UART without a FIFO two byte writers can still collide,
   as before. The waits are unbounded, as on any polled driver: a UART that
   never empties hangs its writer (not tested). The crash reporter takes
   the UART out of loopback before its first line and again once the other
   CPUs are stopped, in case one was in the loopback ktest (the reset is
   ktested, not a crash during that ktest).

   Reading is by interrupt once the shells start (`serial_in`, #120; not
   in the test kernel, whose two ktests that read COM1 read it
   themselves). The UART has one reader, `serial_in` (plus those ktests in
   the test kernel; its calls assert the boot CPU): received bytes keep
   their order only while that holds. ISA IRQ 4 is routed like the
   keyboard's (section 6, "Keyboard"); a route that is level-triggered or
   active-low (a MADT override; `Route::trigger` and `polarity` say so)
   isn't used (a warning; the shell polls; not tested). The UART
   interrupts when received bytes wait (OUT2 set, the received-data
   interrupt enabled; with the FIFO's trigger level 14, at 14 bytes or
   after a four-character timeout: a few interrupts for a line sent at
   once), and the handler moves the waiting bytes into a 256-byte queue
   (`lumen_queue::ByteQueue`) as far as it has room, waking the serial
   shell. Bytes past its room stay in the UART; under QEMU, which stops
   feeding a full FIFO, none are lost, while a real UART overruns and
   drops the next ones unseen. The worst case the design guards against
   is an edge that doesn't come again until the reader lowers the line by
   taking the waiting bytes (QEMU raises it again whenever bytes wait), so
   the reader takes the queue's bytes, then the UART's, with interrupts
   off on the boot CPU (the queue's producer), and the serial shell blocks
   only while neither holds a byte (`lumen_uart::read_next` and
   `available`, host-tested against a model of that worst case). The
   block has a 100 ms deadline as a safety net: input whose interrupt
   never wakes the shell is still read, within 200 ms (input found at the
   deadline in the UART only, nothing queued, gets one more period for a
   late interrupt to queue it: under load QEMU's character timeout was
   measured arriving 1.6 ms after such a deadline), and three waits in a
   row that find input at their deadline (no interrupt bringing input
   between) warn once to check IRQ 4's routing. `serial` counts how the
   waits ended: woken by the interrupt; readied by the deadline first,
   with an interrupt bringing the input before the shell ran (another
   thread on the CPU, or the vCPU, ran meanwhile: common under load
   during a sound, and not a missed interrupt); or input found at the
   deadline with no interrupt bringing it;
   a scenario checks that lines typed while the shell waits wake it by
   interrupt (at least one woken by the handler, none found at the
   deadline), and that with COM1 moved to IRQ 3 the safety net answers
   and warns once; how a wait ended and when it waits one more period
   are pure functions tested on the host (`lumen_uart::classify`,
   `another_period`). An idle boot CPU halts between; each block and wake is two
   thread switches in the trace ring, so an idle serial shell records
   about 20 events a second (the deadline's wakes), and the boot CPU's
   4096-event ring wraps in about 3.4 minutes of idling. Without the
   interrupt (no route, a refused one, or no scheduler running on the
   boot CPU yet, where a thread can't block) the shell polls, yielding,
   which records nothing but keeps the CPU busy; without a UART it blocks
   for good where it can block, else yields (neither tested). The serial
   shell used to poll, keeping the boot CPU 100% busy; under TCG on a
   loaded host such a vCPU was measured going without interrupts for
   longer than the sound driver's 160 ms queue (the host descheduling it
   is the inferred cause). The crash reporter's loopback reset clears OUT2
   too: harmless, as nothing reads input after a crash report.
5. Logger (level from `lumen.log=`), Limine responses checked, the symbol table
   located in our own ELF, a memory-map summary, the frame allocator
   (`INFO  mem: frames: N usable, M reserved`), the kernel's own page tables
   (`INFO  mem: paging: kernel page tables active ...`; the boot CPU no longer
   uses Limine's after this, the APs until they switch, and CPUs past the
   cap for good) and the heap.
6. Kernel stacks from the allocator, each with a guard page: the three IST
   stacks are installed, then `kmain` switches from Limine's boot stack to
   the 128 KiB main stack (`INFO  mem: stacks: ...`) and continues in
   `kmain_on_kernel_stack`: ACPI discovery (below); PCI enumeration over
   ECAM (`INFO  pci: 12 functions, ...`, section 7); the interrupt
   controllers (the 8259s remapped and masked, the local APIC enabled, every
   I/O APIC input masked; `INFO  irq: local APIC ...`); the other CPUs
   started (below); the TSC calibrated
   against the HPET, or the PIT without one
   (`INFO  time: tsc 2.9 GHz (calibrated against hpet)`), and the local APIC
   timer against the TSC, started at 1 kHz; interrupts on
   (`INFO  irq: interrupts on`); a bounded wait until the other CPUs are
   online (`INFO  smp: 4 CPUs online`); the scheduler started on every CPU
   (`INFO  sched: threads on 4 CPUs, 10 ms slices`; `kmain`'s context
   becomes the `main` thread, section 6, "Kernel threads"); the
   framebuffer hello screen and the terminals below it (`INFO  term:
   4 terminals 176x41 (Alt+F1 to F4 switch), 500 lines of scrollback
   each`, section 6, "Terminal"; log
   lines before it are on serial only), the PS/2 keyboard (`INFO  kbd: PS/2 keyboard
   on ISA IRQ 1 ...`, section 6, "Keyboard"), the drivers (`drivers::probe_all`:
   each PCI function offered to the driver table, `INFO  drivers: <n>
   bound, ... (<rows> driver rows)`, section 7), the randomness sources
   (`INFO  rng: sources: RDRAND, virtio-rng`, or `INFO  rng: no
   randomness source ...`: section 7, "Randomness"), then `boot complete`, then the boot timeline
   (below).
7. Either the in-kernel test runner (`--features ktest`) or the shells
   (`shell::run`, section 6, "Shell"): the screen's in a thread of its own,
   then the serial one on the `main` thread (`INFO  shell: shells on serial
   and the screen; type 'help'`). They run in the kernel until the shell
   moves to user mode in M9.

### Boot timeline and log timestamps (built in M1)

`kmain`'s first statement reads the TSC (a bare `rdtsc`, which can't fault,
so GDT and IDT setup still come first in effect): the origin of both the
boot timeline and the log's timestamps. Entry is recorded with the
origin's TSC; every other phase records a
`BootPhase` trace event when it ends: arch-init, logger, symbols, memory, stacks, acpi, pci, interrupts,
time (calibration, interrupts on, the first-tick check: 10 ms, up to 100 ms while no tick has come), smp (the
wait until the other CPUs are online, then the scheduler started on every CPU), framebuffer, complete (which includes setting up the keyboard and probing the drivers). After `boot complete` the kernel prints them from the ring,
converted with the calibrated TSC, and then the bootloader's own times
(from one TCG boot):

```
[    0.364036] INFO  boot: timeline: entry +0.000 ms, arch-init +0.236 ms, ..., complete +363.470 ms (total 363.470 ms)
[    0.366511] INFO  boot: before kmain: limine started at 1932.697 ms and handed off at 2514.149 ms (TSC time), kmain began +10.379 ms after the handoff
```

The bootloader's time isn't counted in the timeline; the second line
shows it. `boot.rs` asks Limine for its bootloader-performance and
TSC-frequency responses (by hand: the pinned `limine` crate has no types
for them). They give when Limine started (`init_usec`, read just after
Limine calibrated the TSC) and when it handed off (`exec_usec`, the last
TSC reading it records before the jump), in whole microseconds of the TSC
by Limine's calibration, and that frequency. Limine takes the frequency
from CPUID leaf 0x15 when the CPU reports a crystal ratio and frequency
there; otherwise it times three 10 ms firmware `Stall`s and keeps the
smallest count. Under QEMU, which zeroes leaf 0x15, it is always the
`Stall` calibration, which a starved emulator can only push up.

`lumen_time::handoff` turns the handoff back into ticks with Limine's own
frequency (rounded down: at or before Limine's reading, by fewer than
`ceil(hz / 10^6)` + 1 ticks) and checks, on every boot (none of it a time
bound):

- Limine started after the TSC's 0 and before its handoff, and handed off
  at least 10 ms after the TSC's 0 (beyond that, Limine's microseconds are
  trusted);
- the handoff tick reads back as Limine's microsecond (or one just
  before): an internal consistency assertion on the conversion;
- Limine's frequency is at least half of the kernel's own calibration
  (one-sided: a high Limine frequency only lengthens Limine's wait in our
  ticks);
- the origin is at least `ceil(hz / 100)` ticks after the handoff tick.
  After its handoff reading, Limine v12.9.4's `flush_irqs` spins
  `stall(10000)` (10 ms by the same frequency) before the jump; it stalls
  because `limine.c` sets `IRQ_PIC_APIC_FLUSH` just before, on every x86-64
  boot. This depends on those pinned Limine internals, so check them again
  on a Limine bump.

Together with the timeline's own order (the origin at or before
arch-init), that bounds the origin between Limine's handoff and arch-init,
which rules out the TSC's reset and the firmware's handoff to Limine (both
before Limine started), without a time bound that a starved emulator could
overrun (#55). It doesn't prove the origin is `kmain`'s first instruction:
one read later, before arch-init, passes. Any of Limine's responses
missing, Limine's 0 Hz, no TSC calibration or no origin logs
`before kmain: unknown (<why>)` at warn; a failed check logs the line at
warn with `; out of order: <reason>`; the scenarios fail on either. The
10 ms after the handoff is Limine's own wait; under KVM it should be about
the same (not measured).

A phase with no event is named as missing, one stamped before entry or
before an earlier phase says so, and the total is `complete`'s; an
incomplete timeline is logged as a warning (not tested: every boot's
timeline is complete; the scenario fails on the warning). Serial log lines
start with the time since that origin, `[seconds.microseconds]`; every line
before the TSC is calibrated (before `time: tsc ...`) shows `[    ?.??????]`, not a
zero that would read as a measurement. The stamps and the timeline share
the origin and the calibrated clock, and `boot complete` is logged just
before the complete phase is marked and the timeline line just after it, so
the total falls between their stamps (checked by order, with no time bound,
in `test_boot_timeline_printed`, the only check that klog's printed stamps
agree with the timeline's origin and unit: the stamps' shape is
`test_log_lines_are_structured`'s, `Stamp`'s digits lumen-time's;
`trace::log_stamps_count_from_the_timelines_origin` checks
`time::since_boot_ns` between two TSC reads). The first virtual terminal shows
lines up to `info` without timestamps, so screenshots stay comparable
(`term::log_lines_are_coloured_by_level` checks the level starts the line).

### Other CPUs (SMP, built in M1)

Limine's MP request starts every CPU and parks the application processors
(APs), each spinning on its `goto_address` with Limine's page tables, GDT
and a bootloader stack. `smp::start_all` (`kernel/src/smp.rs`), on the
boot CPU right after the interrupt controllers are set up (inside the
`interrupts` boot phase), starts them all, up to `arch::MAX_CPUS` = 8 CPUs
in all, without waiting:

- It allocates the AP's 128 KiB main stack and 64 KiB #DF, NMI and #MC
  stacks (guard pages, hard rule 4; `INFO  smp: cpu N stacks: ...`), puts the
  IST ones in the AP's own TSS, and jumps the AP to `ap_entry` with its CPU
  index.
- `ap_entry`, still on Limine's tables and stack: the AP's GDT/TSS, its GS
  base, the shared IDT (hard rule 6), EFER.NXE and CR0.WP; then
  `arch::enter_kernel_space` loads the kernel's root, flushes the TLB
  (global entries too) and switches to the allocated stack in one naked
  step, touching nothing on Limine's stack after the CR3 load. The frame
  chain starts there (`rbp` = 0). Nothing in this stretch can fault (no
  check that can fail, only register and memory writes): there an NMI, #DF
  or #MC would find the AP's IST stacks unmapped (they are in the kernel's
  tables only), a triple fault, and any other exception would lose its
  report to a fault on the crash reporter's local APIC write (one `!!!
  fault while stopping` line).
- On its own stack the AP enables its local APIC, then checks NX support,
  the PAT and its APIC id against Limine's (a failure is reported
  normally), logs `INFO  smp: cpu N online (lapic id X)` and becomes its
  idle thread (`sched::run_ap_idle`), halting in `sti; hlt` until there is
  work; once the boot CPU has calibrated the timer, it starts its own
  timer and scheduler (section 6, "Kernel threads"). After calibrating the clocks the boot CPU waits, 2 s in all,
  for every AP (`smp::wait_online`; a CPU that doesn't make it is warned
  about), then logs `INFO  smp: N CPUs online` (`N of M` at warn if short).
- CPUs past the cap are sent to a halt loop that counts them on arrival,
  and the boot CPU warns how many got there (`2 CPU(s) past the limit
  halted`). They stay on Limine's tables and stack for good.
- Why before the calibration: a CPU in Limine's park loop spins on its
  `goto_address`, and under emulation spinning CPUs take host time from
  every guest calibrating alongside. Measured under TCG, QEMU's CPU time
  to the kmon prompt over a 1-CPU boot's: `-smp 4` +0.54 s with the APs
  started after calibration, +0.34 s before it; `-smp 10` +2.09 s and
  +1.23 s (the rest is Limine's own boot, when they already spin). An idle
  `-smp 10` guest used about 250% host CPU while 2 CPUs spun in Limine,
  100% once they were halted.

**Per-CPU state.** Each CPU's GS base points at its own record
(`arch::percpu`), so `percpu::index()` (0 is the boot CPU) is one
GS-relative load, usable from interrupt and crash paths. Indexed by it: the
GDT and TSS (`ltr` marks a TSS busy), interrupt nesting (`arch::irq`), and
the trace ring. The IDT and the local APIC's mapping are shared (every
CPU's local APIC answers at the same address). The stack registry holds 128
stacks: four per CPU, and the rest for threads and tests (a build-time
assert keeps the four per CPU below it). Each CPU's `smp` slot is empty, starting or online; only online CPUs
get IPIs. The in-kernel tests boot with 4 CPUs online.

**Running something on an AP.** `smp::run_on(cpu, job)` puts a `fn()` in the
AP's mailbox and sends it the `WAKEUP` IPI (vector 0x3f, handled by an
EOI). The idle loop checks the mailbox (and its run queue) with interrupts
off and only then `sti; hlt` (`arch::irq::wait_unless`), so a wakeup can't
land between the check and the halt; the job runs on the idle thread, in
`ap_main`'s frames, and can't block. The shell's `crash <kind> [cpu]` uses it. The scheduler
sends the same IPI when it queues a thread on another CPU (`smp::wake`).

APs handle no I/O APIC interrupts: every input goes to the boot CPU. An AP
starts its local APIC timer once the boot CPU publishes the count, and the
timer's first tick starts its scheduler. Known gap: there
is no TLB shootdown; `unmap` flushes only the CPU that runs it. Mapping
needs none (x86 caches no not-present entries; an AP may grow the heap, or
map a thread's stack, from a thread or a `run_on` job there). The only
pages unmapped are dropped kernel stacks (test stacks, on the boot CPU; a
dead thread's, on its home CPU), the paging ktests' scratch pages (boot
CPU, other CPUs idle) and the rollback of a failed `map_fresh`, which
undoes pages its own CPU just mapped. Apart from a thread's spawner, which
wrote its first frame (so it may keep an entry for that page; harmless,
since stack addresses are never reused), no other CPU has accessed any of
them; stale entries from speculative fills aren't ruled out (not tested).

### ACPI (built in M1)

`acpi::init` follows the RSDP Limine gives to the XSDT (or the RSDT on ACPI
1.0 firmware, or when the XSDT address is 0), then reads the MADT, the
HPET table and the MCFG (PCI's ECAM regions, section 7). The parsing is in the host-tested `lumen-acpi` crate:
bounds-checked little-endian reads from byte slices (the tables are packed
and unaligned). A table that is used has its length (capped at 1 MiB)
checked when its header is read, its checksum before any field past the
header is used, and each MADT entry's length against the table and its type.
Looking for a table skips null entries and tables whose header can't be
read; only the table it returns has its length and checksum checked.

The kernel reads tables through one reader, which only reads memory that
holds firmware data: `acpi-*` and `reserved` entries (outside the HHDM) are
mapped uncached with `map_mmio` (a read inside a span already mapped reuses
that mapping); RAM the HHDM covers is read through it only if it is usable
memory, which is then reserved for ACPI tables under the name the RSDP's boot
reservation uses (`frame::reserve`; see "Physical memory" in section 5).
Anything else, such as a pointer into the frame bitmap or the framebuffer,
is refused. The kernel copies what it needs into `acpi::Info` (every MADT CPU
entry with its APIC id and state, the local APIC address with the type 5
override applied, the I/O APICs, the interrupt overrides, the 8259 flag and
the HPET) and logs `INFO  acpi: N CPU(s), LAPIC at 0x..., HPET at 0x...` (or
`..., no HPET`), after an `INFO  acpi: I/O APIC ...` line per I/O APIC. N
counts enabled CPU entries (a CPU listed twice, as type 0 and type 9, is
warned about); QEMU's `maxcpus` hot-plug slots are listed but not enabled.

No RSDP, or a bad RSDP, root table or MADT, is a warning
(`running without ACPI: ...`, naming why a read failed), not a panic: boot
continues and `acpi::info()` is `None` (it panics before `acpi::init`). A
missing or broken HPET alone is a warning (`no usable HPET: ...`) and leaves
`hpet` empty. A table skipped while looking for another is warned about too
(`skipped the table at 0x...: ...`).

### Reboot and power-off (built in M2)

`acpi::init` also reads what powering off takes into `acpi::Info::power_off`:
the FADT's PM1a and PM1b control blocks (each from its 64-bit `X_` field
when the table has it and its address isn't 0, which must then be in I/O
space, else from the 32-bit field; a hardware-reduced machine has no PM1
blocks, only a sleep control register, which isn't supported), the DSDT
(required), and how to switch to ACPI mode (`AcpiMode`: always, when there
is no SMI command port; ask, writing `ACPI_ENABLE` to it; or unusable, when
`ACPI_ENABLE` is 0 or the port doesn't fit 16 bits). The DSDT's `\_S5_`
package gives the S5 sleep type values: no AML interpreter builds on stable
Rust, so `SleepType::s5_from_dsdt` searches the DSDT's bytes for `_S5_`
after a `NameOp` (past one `\` or any number of `^` prefixes) naming a
`PackageOp` whose length fits the table, and decodes its first two integer
constants (each at most 7; a single value holds both in its low two bytes,
with nothing above them); if none decodes, the error counts the
`Name (_S5_, ...)` found and gives the first one's AML offset and why. The
ports come out as `Pm1Control` and `AcpiEnable`, which only `Fadt::parse`
makes, from a checksummed FACP table, and the values as `SlpTyp` (checked to
fit 3 bits); `arch::reset` writes the ACPI registers only through those, with
values from `lumen_acpi::pm1_control_value` (host-tested: SLP_TYP replaced,
SLP_EN only on the second write, other bits kept). A missing or broken FADT,
DSDT or `_S5_` is a warning at boot (`no ACPI power-off: ...`), as is an
unusable way to ask for ACPI mode, and the reason is kept for `shutdown`;
boot goes on. (The parse errors are host-tested; the boot warnings are not
tested: QEMU's tables are fine.)

The shell's `shutdown` (`power.rs`) says `shutdown: powering off`, logs
`INFO  power: powering off: ACPI S5, ...`, and with interrupts off asks the
firmware for ACPI mode if SCI_EN is clear (the SMI command, or a warning
naming why it can't; not tested: OVMF leaves q35 in ACPI mode), writes
SLP_TYP into PM1a (and PM1b, not tested: q35 has none), then SLP_EN. Only
SLP_TYP 0 is exercised in QEMU (q35's `_S5_` is four Zeros). If the machine
is still on half a second later it warns and says why (`shutdown: can't
power off: still on after ACPI S5 (...)`; not tested), as it does when ACPI
gave no way to power off (not tested). `reboot`, interrupts off for good,
logs `INFO  power: rebooting: reset through port 0xcf9` and writes a full
reset to the reset control register (0xCF9); half a second later (a warning
first) it pulses the reset line through the 8042 (command 0xFE, after a
bounded wait for it to be ready; the warning says if it never was), and half
a second after that loads an empty IDT and takes `int3` (#GP, #DF, then a
shutdown: a triple fault, which resets the machine). `lumen.reboot=8042` or
`lumen.reboot=triple` starts at a later method, which is how the scenarios
test the fallbacks (q35 always takes 0xCF9's reset); an unknown value starts
at 0xCF9 with a warning (tested). The reboot scenarios run QEMU without
`-no-reboot` (the banner appears twice, the log shows which method did it,
and under TCG `qemu.log` shows a triple fault only for that method); the
power-off one accepts QEMU's exit status 0 only with a QMP `SHUTDOWN` event
whose reason is `guest-shutdown` (a triple fault under `-no-reboot` also
exits with 0, but as `guest-reset`).

Kernel command-line flags: `lumen.qemu_exit` (allow ending QEMU through
`isa-debug-exit`, used by tests), `lumen.log=<error|warn|info|debug|trace>`
and `lumen.keymap=<us|uk|de|fr|no|se|jp|dvorak|colemak>` (the keyboard's
layout, section 6; `us` by default and for a name it doesn't know, with a
warning), `lumen.reboot=<cf9|8042|triple>` (the first way `reboot` tries;
see "Reboot and power-off"), `lumen.ansi` (the serial shell edits with escape codes, its
terminal taken to be 80 columns wide; plain text without it, section 6,
"Shell"), and `lumen.stall_tsc_windows=<end ppm>[,<start ppm>]` and
`lumen.stall_timer_windows=...` (each boot calibration window's end, and
start, stalled by a share of it, up to 10 windows' worth; a deliberate fault
for scenarios, said at INFO, section 6, "Time"), `lumen.desktop` (start the
desktop before the serial prompt; section 11),
`lumen.desktop_cut_delay_ms=<ms>` (a deliberate delay for scenarios between
the compositor's reads of the input rings and of the cut, said at INFO,
capped at 2000 ms; section 11) and `lumen.theme=<light|dark>` (the
desktop's theme, said at INFO; a name that isn't a theme is warned about
and ignored; section 11).

## 5. Memory layout

### Virtual address space (x86-64, 4-level paging)

| Range | Use | Status |
|---|---|---|
| `0x0000_0000_0000_0000 – 0x0000_7fff_ffff_ffff` | empty in the kernel's tables (null guard); user space, per process, from M9 (except the first 64 KiB) | built (empty) |
| `0xffff_8000_0000_0000 – +phys size` | higher-half direct map (HHDM) of RAM: usable, bootloader-reclaimable, kernel-and-modules and framebuffer entries; NX, RW- except kernel-and-modules (R--), framebuffer write-combining | built |
| `0xffff_c000_0000_0000 – 0xffff_c0ff_ffff_ffff` | kernel heap (grows up by mapping frames, to 1 GiB below the top; the top GiB holds a ktest scratch page) | built |
| `0xffff_d000_0000_0000 – 0xffff_d0ff_ffff_ffff` | kernel stacks: 64 KiB (the main stack 128 KiB, script threads 1 MiB), **unmapped guard page below every stack**; the top GiB holds a ktest page | built |
| `0xffff_e000_0000_0000 – 0xffff_e0ff_ffff_ffff` | MMIO mappings (uncached, RW-), `map_mmio` | built |
| `0xffff_ffff_8000_0000 – end of image` | kernel image: `.text` R-X, `.rodata` (and `.got`) R--, `.data/.bss` RW-, page-aligned segments | built |

The kernel is linked at -2 GiB because the **kernel code model** requires it
(all symbols fit in sign-extended 32-bit immediates). `make audit` verifies
this along with the absence of SSE/AVX and red-zone use and the presence of
stack probes (hard rule 3).

### Physical memory (built in M1)

The frame allocator (`kernel/src/mm/frame.rs`, logic in the host-tested
`crates/lumen-mm`) is built only from Limine's memory map. Only `usable`
regions are handed out. These are **explicitly reserved by name, even if
Limine labels them usable**: frame 0, the kernel image, the kernel ELF file
(the symbol table lives in it), the boot stack and the framebuffer (those four
found by translating their live virtual addresses through the page tables
rather than trusting the map's labels), the ACPI RSDP and the allocator's own
bitmaps. Every non-usable map entry (including the `acpi-*` entries, where
firmware normally puts the ACPI tables) and every frame of Limine's page
tables (active while the allocator is built; the kernel's own tables are then
allocated from it) is reserved too. The named list is logged at `debug`
level.

Memory found once the allocator is built is reserved with
`frame::reserve(name, range)`. Every frame must be free (it is taken) or
already reserved under the same name; an allocated frame, one reserved for
something else, or memory the allocator doesn't manage is refused, and so is
a range that needs a new named entry when the list is full, changing
nothing. A range that takes frames joins the named list (merged with a
same-name range it touches). ACPI parsing reserves each table (or header) it
reads in usable memory that way, under the RSDP's reservation name. It runs
after the kernel's page tables, heap and stacks exist (it needs `map_mmio`
and the heap), so a table whose frame was already handed out is refused: if
that is the RSDP, the root table or the MADT, ACPI is skipped with a warning
naming the frame; if it is the HPET table, only the HPET; another table's
header is skipped (with a warning) while searching. With OVMF on q35 the
tables are in `acpi-*` entries; only ktest stand-in frames exercise this
path.

It is a bitmap allocator with two bits per 4 KiB frame (64 KiB per GiB of
physical span): *owned* (usable and not reserved) and *free*. Reserved and
allocated frames both have a clear free bit; the owned bit tells them apart,
so `free` rejects a frame that was never the allocator's to give instead of
adding it to the pool. The usable set is fixed at construction; `reserve` can
only take frames away, and refuses a range containing an allocated frame.
Single frames come from a word scan starting from a hint below which no frame
is free; contiguous runs from a linear scan. Callers return exactly what they
allocated (`free` for `alloc`, `free_contiguous` with the same count for
`alloc_contiguous`), so a buddy allocator can replace it behind the same
interface if contiguous allocation becomes hot. `free` and
`free_contiguous` are `unsafe` (the caller vouches that it owns the frames
and nothing uses them); `alloc_owned`
gives an `OwnedFrame` instead, which frees itself on drop and is what
`paging::map` takes.

In-kernel tests check, independently of the reservation list (by walking the
page tables), that no frame behind a range the kernel is using (kernel image,
boot stack, current stack page, kernel file, framebuffer, frame bitmap,
memory map, cmdline) is allocatable and no page-table frame is free, and
that the check itself fires when a frame in use is reported free. (Since
the switch to an allocated stack, the current stack page is an allocated
frame: it must never be free rather than never owned.) They then allocate every
frame, check each against the memory map and the reservation list, fill it,
tag it with a sequence number, and free them all, checking each tag (catches
a frame handed out twice) (hard rule 2). Bootloader-reclaimable
memory is not returned yet; whoever adds that must first copy or stop using
everything in it (the boot stack and Limine's responses, including the
memory map), and Limine's page tables and AP stacks, which the CPUs past
the cap halt on for good.

### Page tables (built in M1)

At boot the kernel runs on Limine's tables. Right after the frame allocator
exists, `kernel/src/mm/paging.rs` builds its own hierarchy from allocator
frames (zeroed) and switches CR3:

- **HHDM** at Limine's offset, covering the memory-map entry types the
  Limine protocol says its HHDM covers at base revision 3 (usable,
  bootloader-reclaimable, kernel and modules, framebuffer), so every HHDM
  pointer Limine handed out stays valid. No-execute; read-write except the
  kernel-and-modules entries, which are read-only so the kernel's code has
  no writable alias (W^X holds per frame, not just per page); the
  framebuffer is write-combining. 2 MiB pages where a whole one fits, 4 KiB
  elsewhere. ACPI memory is not in the HHDM; ACPI parsing maps the tables
  it reads there uncached with `map_mmio` (see "ACPI" in section 4).
  Entries that share a page map it once; boot stops if they'd need
  different rights. A ktest checks every HHDM span is mapped throughout
  (sampled every 2 MiB) with its own rights
  and memory type, nothing else is mapped outside the image and the MMIO
  window, and ACPI and reserved entries are absent (spot-checked).
- **Kernel image** per segment from the linker script's page-aligned
  boundaries (`__rodata_start`, `__data_start`): text R-X, rodata R--, data
  RW-. `make audit` checks every allocated section of the ELF lies inside
  `[__kernel_start, __kernel_end)` and in the segment whose rights it needs
  (executable in text, writable in data, the rest in rodata), so no section
  of the image is left unmapped or mapped with the wrong rights (an orphan
  `.got` once was; it is now placed, read-only, in rodata: its entries are
  fixed at link time).
- **Nothing else.** The lower half is empty, so null and small pointers fault.

Before loading CR3 it checks that every page of every range the kernel is
known to use (image, boot stack, current stack page, kernel file,
framebuffer, frame bitmap, memory map and its entries, cmdline), every page
Limine mapped in the kernel's 2 GiB, and every 2 MiB step of each HHDM span
(which covers the rest of Limine's responses) translates to the same frame
under the new tables; writable where the kernel writes (the stacks, the
framebuffer, the bitmap, `.data`/`.bss`), executable exactly where Limine's
mapping of the kernel is (the text), and with each HHDM span's memory type.
A mistake is an assertion naming the range, the address and how each walk
ended, not a fault with no stack. A ktest builds a copy of the tables,
plants one fault of each kind (kernel data read-only, text NX, RAM
uncached, nothing mapped) and checks each is named. (It runs on the
allocated main stack, which a fresh copy doesn't map: it asserts a strict
check names the current stack page, then looks for the planted faults with
`RunningStack::Skip`; the real switch passes `RunningStack::Check`.) EFER.NXE and CR0.WP are set (so
read-only pages bind in ring 0 too) and the PAT layout the mappings rely on
(slot 0 WB, 3 UC, 5 WC, set up by Limine) is checked rather than rewritten.

The entry format, walks and mapper are pure logic in `lumen_mm::paging`,
tested on the host against fake table memory (including a randomized
comparison with a model). Table entries are permissive; rights are enforced
at the leaves, and walks report the rights combined over every level, which
is what the CPU enforces; `Attrs` can't express a writable and executable
page. The kernel API:

- `map`/`unmap`: 4 KiB pages in the heap and stack windows, for whoever
  holds the `Window` token for the page (`lumen_mm::window`, host-tested).
  Paging hands each window out once ever (`claim_heap_window`,
  `claim_stack_window`, and two test windows at the top GiB of each for the
  ktests); a holder can split off disjoint sub-windows (each kernel stack
  gets its own; guard pages get none, so they can never be mapped), and no
  safe code can forge or copy one (building a claims table is `unsafe`). Frames come as `OwnedFrame`s
  (move-only, freed on drop; the raw `frame::free` is `unsafe`). With both
  proven, `map(&mut window, virt, frame, attrs)` is safe: it consumes the
  frame (giving it back on failure) and still refuses what it can check
  (outside the token, outside the heap and stack windows (the
  address-space check: a window names no address space; `build` checks the
  HHDM ends below them), a frame nobody allocated,
  non-write-back or executable rights, since allocator frames are writable
  through the HHDM). `unmap` stays `unsafe`: only the caller knows nothing
  refers through the page any more. It invalidates the TLB entry and gives
  the frame back (it panics, leaving the mapping, if the page isn't a 4 KiB
  leaf on an allocated frame, which only a broken invariant could cause). What the types don't prove: that nothing uses a page being
  unmapped, and that the frame's HHDM alias goes unused (every allocated
  frame has one; only `unsafe` code, including the ktest helpers, can reach
  it). Everything else (image,
  HHDM, MMIO) is fixed.
- `map_mmio`: uncached, permanent; refuses memory the HHDM maps (two memory
  types for one frame is undefined); all or nothing (a partial mapping is
  rolled back, host-tested).
- `walk`, `translate`, and sweeps over every table and every mapping (which
  report the first table they couldn't read rather than skipping it
  silently).

Tables are never freed. A page fault's crash report includes, after the
backtrace and the trace events (serial only), the walk of CR2: every entry
read, then the page and its rights, or the level that is not present. It
takes no locks; a table outside HHDM-mapped RAM ends the walk instead of
being read, and if the walk itself faults the report still ends properly.

The kernel's upper-half entries are not yet shared with anything; processes
(M9) will need the kernel's L4 entries 256-511 copied into every address
space, so new kernel L3 tables must be allocated up front then.

### Kernel stacks (built in M1)

Every stack the kernel runs on once `kmain` switches stacks (right after
`mm::init`) comes from the allocator
(`kernel/src/mm/stack.rs`; layout and registry in the host-tested
`lumen_mm::stacks`). `KernelStack::new` takes addresses from the stack
window (`StackWindow` holds its token and splits off a sub-window for each
stack, covering the stack alone; the guard page's range is dropped, so it
has no token), upward, with an unmapped guard page below each
stack (so the page above a stack is the next one's guard), maps fresh frames
there through its own sub-window (RW, NX, write-back; all or nothing,
shared with the heap in `mm/window.rs`) and registers the stack. Dropping one retracts it from the registry, then
unmaps it and frees its frames; addresses are never reused. Each CPU's
main and IST stacks are leaked so they can't be dropped; a thread's stack
is dropped only once the thread is dead and its CPU has switched away from
it.

Boot: the IDT starts with every vector on the current stack. Right after
`mm::init`, `kmain` allocates the 64 KiB #DF, NMI and #MC stacks, puts them in
the boot CPU's TSS's IST and only then makes those three gates use them, allocates the
128 KiB main stack and switches to it (`arch::switch_stack`), which plants a
frame record so backtraces run from the new stack back into `kmain` and
`_start` on the old one. Limine's boot stack (128 KiB requested, 64 KiB
assumed if it doesn't confirm) stays reserved and is recorded as a known
stack (`stacks::set_boot_stack`, not in the registry) for that walk, but is
never run on again. Until the switch it is the boot CPU's one stack without
a guard page, and a #DF with a broken `rsp` before then is a triple fault
instead of a report. Each AP likewise runs on its own Limine stack from
`smp::ap_entry` until `enter_kernel_space` (not registered; nothing there
may fault, see "Other CPUs"), and CPUs past the cap halt on theirs.

The registry (lock-free, 128 slots, each a seqlock; registering is `unsafe`
because readers dereference what it lists) and the boot stack are what the
crash reporter trusts: a frame pointer is followed only into a known stack,
and frames move toward the top of their stack; the chain may change stacks
once (the main stack's first frame links into `kmain` on the boot stack), and
every early stop says why. A #PF or #DF whose CR2 is in a guard page is
named. It is `kernel stack overflow (guard page hit)`, with the stack's name
and bounds, only on a #DF with the interrupted `rsp` in that guard page or
the stack's bottom page: an overflow's #PF can't be delivered on the full
stack, so it always arrives as a #DF. Otherwise it is a `guard page hit`: a
stray access, or the stack below running off its top (named when `rsp` is
in it); on a #DF the verdict is hedged (nothing sets CR2 on one, and the
state it saves is not architecturally defined). A nested crash in the
reporter names a guard page only if CR2 changed since the report began.
Inline stack probes are the `x86_64-unknown-none` default, so every crate
gets them; `make audit` checks the kernel crate's IR. A frame bigger than a
page therefore can't jump over a guard page (`crash bigframe` faults in it
with a sparse 16 KiB frame).

### Kernel heap (built in M1)

`kernel/src/mm/heap.rs` is the `#[global_allocator]`; the algorithm is the
host-tested `crates/lumen-heap`. One contiguous span grows up from the
heap window's base (the heap holds its `Window` token): when no free block
fits, the heap maps fresh frames at its end (read-write, no-execute,
write-back, through `paging::map`) and appends them. It grows by its own size, between 64 KiB and 4 MiB, or by
what the request needs if that is more (so a heap that reaches N bytes took
about log2(N) growths); when that much isn't available it grows by just
the pages the request needs. Growing is all or nothing: if
frames or page tables run out part way, every page mapped so far is
unmapped and its frame freed (the tables created stay; tables are never
freed). The heap never shrinks and stops 1 GiB below the top of the window.

Free memory is a singly linked list of blocks in address order; each free
block stores its size and the next block's offset in its first 16 bytes.
Allocated blocks have no header, since `GlobalAlloc::dealloc` gets the same
`Layout` as `alloc` and both round it to the same block size (a multiple of
16 bytes). Allocation is first fit; padding in front of an aligned block and
the tail stay free. `dealloc` merges with both neighbours, so no two free
blocks touch, and refuses a block outside the heap, misaligned, or
overlapping memory that is free right now, without changing anything; the
kernel then panics. That catches a double free while the block is still
free and a `Layout` reaching into free memory or past the heap's end. It
can't reliably catch a double free after the memory was handed out again, a
`Layout` reaching into another live block, or a smaller `Layout` (which
leaks the difference; the heap check reports it once nothing is allocated).
Every walk checks each free block it reads (inside the heap, linking up to
a block inside the heap), so a corrupt link is a panic (and a crash report),
not a hang or an access outside the heap; corruption that keeps blocks in
place and in order is found only by the heap check's accounting. Both operations walk the list (O(free blocks)); size
classes for small objects can go in front of it later.

When the heap runs out (no frame for the heap or for a page table, or the
window is full) the allocation returns null: `try_reserve` callers see an error, and everything else
reaches `alloc`'s default error handler, which panics
(`#[alloc_error_handler]` is unstable). Any other growth failure is a
kernel bug and panics, as does allocating before `mm::init`. The serial crash report ends with one line of heap
state, including the last failed growth (size, alignment, reason); the
shell's `heap` command prints the same line. Lock order: heap, page tables, frame
allocator; nothing holding the last two may allocate. The allocator never
logs (a log argument that allocates would re-enter it under the logger's
lock) and formats its shell and crash line from a copy taken under the
lock. Like every `SpinLock`, it keeps interrupts off while held, so an
interrupt handler is allowed to allocate. The timer interrupt can free
(in `sched::finish_switch`): a detached dead thread's record and its
joiners' list (its stack goes back through the page tables and frame
allocator); the scheduler reserves room in its queues so the timer never
allocates.

## 6. Interrupts, exceptions and crash reports (built)

- One assembler `.rept` loop generates 256 entry stubs, each 16 bytes apart.
  Stubs push a dummy error code where the CPU does not, then the vector, then
  jump to `lumen_isr_common`, which saves all 15 GPRs into a `TrapFrame` and
  calls `lumen_exception_dispatch`.
- **Vector map** (`arch/x86_64/idt.rs`): 0-31 CPU exceptions; 0x20-0x2f the
  8259 PICs, remapped there and masked (only a spurious IRQ 7 or 15 should
  arrive); 0x30-0x3e unused (ktests raise 0x30 and 0x31); 0x3f the `WAKEUP` IPI
  (section 4, "Other CPUs"); 0x40 the local APIC
  timer; 0x50 + n global system interrupt n (GSIs 0-63, up to 0x8f),
  once a driver routes it (`interrupts::route_isa_irq`; every other I/O
  APIC input stays masked); 0x90-0xef message-signalled interrupts (MSI and
  MSI-X, `idt::MSI_BASE..=MSI_LAST`), for PCI devices (section 7: handed
  out lowest first by `idt::claim_msi_vector`, each with its handler); 0xfe local APIC errors; 0xff the local APIC's
  spurious vector; the rest unused.
- **Dispatch:** `#BP` is counted and resumes (a ktest proves registers
  survive the round trip); every other exception is fatal and goes to
  `crash::exception`. Exceptions are traced. Device interrupts are never
  fatal: the timer counts a tick (per CPU; `time::ticks` is the boot
  CPU's), sends the local APIC an EOI and runs the scheduler's tick (which
  may switch threads); spurious interrupts (the local APIC's, the 8259s' IRQ 7 and 15) are
  counted and get no EOI (a spurious IRQ 15 gets one at the master 8259); a
  local APIC error is counted, logged with its status (the 1st, 2nd, 4th,
  8th... time, and whenever a new status bit appears) and EOI'd; a routed
  I/O APIC vector runs the handler its driver registered and is EOI'd, and
  so does a claimed MSI vector (its own handler table, beside the I/O APIC
  one: `set_irq_handler` stays I/O APIC only); every interrupt nothing
  handles (an unused vector, an I/O APIC vector without a handler, an MSI
  vector nothing claimed, another 8259 line) is unexpected: traced, counted, reported the first time on its
  vector and again at its 1024th, 2048th, 4096th... (each report with the
  count, so a storm stays visible), and EOI'd. The report is a warning, or an error on
  an MSI vector (only a device programmed to send one can). Another 8259
  line also masks the 8259s again; an 8259 vector the local APIC has in
  service is EOI'd there. Of device interrupts, only the unexpected ones are
  traced: at 1 kHz the timer would flood the ring. The shell's `ticks`
  shows the counts (the boot CPU's timer), and boot warns if no tick
  arrives within 100 ms (it waits 10 ms, then up to 90 ms more while none
  has come, and logs a late one at info).
- **Interrupt state:** interrupts are off through boot and turned on once
  the controllers and the timer are set up. Every gate is an interrupt gate
  (handlers run with interrupts off). `SpinLock` keeps them off while held
  (`arch::irq::push_off`, whose `IrqOff` guard nests by depth per CPU, so guards
  dropped in any order never turn them on early, and can't be sent to
  another thread), so a maskable interrupt's handler never spins on a lock
  the code it interrupted holds: it may log, and is allowed to allocate.
  The crash path (after recording its own crash) and `reboot` turn them
  off for good, recording which CPU did; from then on no guard turns them
  back on, on any CPU. The lock word is the holder (free, or the holding
  CPU's index + 1), so a CPU that takes a lock it already holds
  (an exception or NMI handler taking one the code it interrupted holds, or
  plain nesting) panics with the lock's address and type, the CPU, and the
  caller's location, instead of spinning forever (`crash relock`; only
  plain nesting is exercised). `try_lock` returns `None` on a lock this
  CPU holds, as on one another CPU holds. The crash reporter never waits
  for a lock, so that report is complete.
- **Time** (`time.rs`, arithmetic in `crates/lumen-time`): the TSC's
  frequency is the median of five 10 ms windows against the HPET (the PIT's
  channel 2, polled, without one), measured with interrupts off. Each window
  starts and ends with a reading of the reference's counter between two TSC
  reads; the PIT's counter runs on past its end, so a vCPU paused while
  waiting for the end doesn't stretch the window (a yes/no "has it run out"
  did: one CI boot under KVM calibrated 11.8% high, #45). The PIT's start is read too: QEMU's
  counts from when it is programmed, not from the gate (measured under TCG). A window whose start
  or end reading was itself paused (the gaps between the TSC reads around
  them add up to more than 0.5% of the window), or, for the PIT, whose end
  was read too late to place, is set aside and another taken after a wait
  of two windows' worth of TSC ticks (the fewest any window with a bound
  spanned from its start reading: the TSC isn't calibrated yet, but every
  window spans 10 ms of the reference), up to 20 for five. An HPET window whose ends were slow to read is taken again
  longer, until the same gaps would be 0.4% of it, up to 160 ms (#139: on
  KVM a normal window's gaps already take 0.23% of it, and a slowed host's
  HPET reads took 20 times as long: 16 of 20 windows taken back to back were
  unsteady, and the timer's, 20 ms apart, failed 18 of 20). A read slow to
  come back slows every poll, a burst of stalls one read: each end's gap
  counts for at most two mean poll intervals, and the gaps are a share of
  the TSC ticks the reference counted over (with the start gap, which the
  reference counts), so one burst or stall at either end counts no more
  than two slow reads. Where reads are slow that can still stretch a window
  a little (KVM gives about 850 per 10 ms window, fewer under load: up to
  3x seen with a stall), and more if the window also lost its CPU mid-way
  (fewer polls, so a longer mean interval). The PIT can't stretch
  past 13,107 ticks, about 11 ms, the most its 16-bit counter can be shown
  not to have wrapped. If attempts run
  out, the unsteady windows with the smallest bounds fill in. A PIT read-back that can't fit how it was
  programmed is an error, and so is an HPET that doesn't count, which gives
  way to the PIT (warned). The local APIC timer (divide
  by 16) is calibrated against the TSC as the median of five 10 ms windows
  too, each end a register access between two TSC reads (the timer started
  by one write, its count read), so a window whose ends were paused (gaps
  over 0.5% of it) is taken again, 20 ms later (stalls come in bursts), up
  to 20 windows in all (#129), the ones with the smallest bounds filling in
  if those run out. Each boot calibration
  is then judged by how far off its median can be: the third smallest of
  its windows' bounds, each window off by at most what its gaps allow (for
  the timer's rate, its gaps' share of it, and the TSC's own bound on top,
  since the timer is measured against it; for the TSC's, measured against
  the reference, the end gap over the rest of the window, which it
  stretched, plus one reference tick). Within its clock's tolerance (the TSC
  1%, half the 2% its tightest cross-checks allow; the timer 2%, the TSC's
  error included, which leaves the 1 kHz check's 5% room for its own
  measurement),
  boot logs at most an INFO line with the bound when it took windows again;
  past it, a warning saying how far off; with no bound under 100% the rate
  says nothing (a TSC window's end gap as long as the rest of it, a timer
  window's gaps as long as itself). An HPET past the TSC's tolerance is
  weighed against the PIT, measured then too, and the one whose median can
  be off less is kept (said at INFO; a PIT that fails then is a warning); an
  HPET with no bound gives way to the PIT; otherwise boot panics. An HPET
  that counted is kept as a second clock (`time::hpet`, `clocks`) whichever
  calibrated the TSC (`time::reference`). A timer whose own windows are within
  its tolerance but whose bound is past it through the TSC's says so. The old rule, a warning unless most windows
  were steady, failed scenarios on a boot whose timer was within 0.7%
  (#139). Scenarios stall each window with `lumen.stall_tsc_windows` and
  `lumen.stall_timer_windows` (section 4) to reach the fill-in, the choice,
  the warning and the panic in a real boot. The timer paces preemption, when
  deadlines are noticed, the cursor's blink and the `ticks` counts; sleeps
  and deadlines themselves are TSC times. It is then set periodic at 1 kHz. `time::now_ns` (TSC-based, lock-free) and
  `time::sleep` (blocks the thread once the scheduler runs on its CPU,
  section "Kernel threads"; busy-waits before that, on an idle thread, with
  interrupts off or a lock held), `time::spin` (always busy). The boot CPU
  publishes the timer's count; every other CPU starts its own timer with
  it, and the first tick starts that CPU's scheduler, and ticks are counted per CPU (`time::ticks`
  is the boot CPU's). The shell has `ticks`,
  `sleep <ms>`, `uptime`, and `clocks` (the TSC measured again against the
  PIT and the HPET, which the tests compare with the boot calibration: a
  wrong calibration would cancel out in tick counts, since the timer is
  calibrated against the TSC; each `within <n> ppm`, how far off its median
  can be, and a measurement with no bound shown as failed). A second line lists the windows behind each:
  `clocks: windows: boot <cal>; pit <cal>; hpet <cal>`, each `<cal>` being
  `<hz> Hz [<w> <w> <w> <w> <w>] retook <n>` with each window `<w>` as
  `tsc/reference/polls/start_gap/end_gap`; a failed or missing measurement
  is left out. Under QEMU's TCG a
  busy-waiting guest gets only some of its ticks (late periodic timers skip
  periods): about 80% alone, as few as 18% with many VMs running in parallel
  (measured in M1, when `time::sleep` busy-waited; it now halts in the
  idle thread).
  Tests check the timer's rate by its count register against the HPET, check
  only that ticks arrive (at least one, none stray; the ktest also caps them
  at 150 per 100 ms), and never assert on host time. Every
  calibration poll is bounded: an HPET that doesn't count, calibrates
  implausibly, or whose median has no bound under 100% is warned about (with
  why) and the PIT used; a PIT whose
  output doesn't go low when programmed, or high again within 2^32 TSC
  ticks (over 4 s at 1 GHz), counts as missing; calibration results outside
  10 MHz to 100 GHz are rejected.
- #DF, NMI and #MC run on their own guarded IST stacks, each CPU's own (the
  boot CPU's from step 6 of the boot, before that on the boot stack; an
  AP's from `enter_kernel_space` on), so a corrupted or overflowed kernel
  stack still yields a report; an overflow is named as one (section 5,
  "Kernel stacks").
- **Crash report:** exception name and vector, decoded #PF error code and CR2,
  rip with symbol, all registers, CR0/3/4, a frame-pointer backtrace, then on
  serial only the last trace events, for a #PF the page-table walk of CR2,
  one line of heap state (never blocks: if the heap's lock is held it reads
  the state anyway and says so; the free-list walk is bounded and stays
  inside the heap), and last `interrupts: off` (sampled when the report
  begins and at its end; the line says which if they were on). It is
  written to serial and to the screen. Frame pointers are forced on
  (`-C force-frame-pointers=yes`); the walker follows `rbp` only inside known
  stacks and only to return addresses inside kernel `.text`, so a corrupted
  chain cannot fault. Symbols come from our own ELF (Limine's executable-file
  response) via `crates/lumen-elf` and are demangled with `rustc-demangle`.
- **Several CPUs:** the first CPU to crash owns the report (`CRASHING`
  holds its index plus one). Every CPU records its own crash first thing
  (vector and rip, or a panic's line). Before writing anything the owner
  takes one snapshot of the others: online CPUs get an NMI (one still parked
  in the bootloader has no handler for it), started ones are noted; then it
  waits, 500 ms in all (a bounded number of rounds before the TSC is
  calibrated), for each online CPU to halt. Serial starts with
  `cpu: N (lapic id X)`, `running on: the <kind> stack <bottom>..<top>` (the
  reporter's own stack, from the registry), `thread: <name> (id N)` (the
  thread that was running, once that CPU's threads are set up; read lock-free) and, with other CPUs online,
  `other cpus stopped: 0 2 3`: those that had halted when the wait ended,
  without a crash of their own (or halted on their own first: a thread
  that would block on a CPU that hasn't turned interrupts off for good
  itself, after another CPU did, halts its CPU, see "Kernel threads"). `!!!` lines name the rest: a CPU that didn't
  halt in time, one whose NMI couldn't be sent, one that crashed itself
  (`cpu 2 also crashed: #PF page fault (vector 0xe), rip ...` or `a panic at
  line N`), and APs started but not yet online (no NMI; one could still log
  a line into the report; not tested). With every CPU stopped, reports and
  log lines don't interleave; a CPU that wasn't stopped can still log into
  the report, its line interleaved with the report's (the report writes
  byte by byte and a burst leaves one FIFO slot free, so on a 16550A
  nothing is lost to the FIFO; see step 4 of the boot sequence). The screen copy doesn't show these lines. A
  fault while stopping the others prints one line that keeps the original
  crash.
- A fault during the report prints one line and halts, with no recursion. With
  `lumen.qemu_exit` the kernel exits QEMU with status 37; otherwise it halts.
- Planned: "Explain this crash with Claude" in the GUI, which sends the same
  report text with the user's consent (M8).

### Kernel threads (built in M2)

`kernel/src/sched.rs`. A spawned thread has its own 64 KiB kernel stack
(`run_sized` runs a closure in a thread with another size and waits for
it, so the closure may borrow: the script threads' 1 MiB)
from `mm::stack` (a guard page below it, hard rule 4; registered as a
`thread` stack), its saved `rsp`, and a state: ready, running, sleeping,
blocked or dead. (`main` and the APs' idle threads run on stacks they don't
own.) `sched::spawn(name, f)` starts one on this CPU and `spawn_on(cpu, ..)`
on another; both return a `JoinHandle`, whose `join` blocks until the
thread has finished and its stack is freed. A thread **never moves between
CPUs**: each CPU has its own run queue (a FIFO) and only that CPU takes
threads off it, so once its spawner has laid out its first frame, a
thread's saved registers are touched only by its home CPU with interrupts
off. A wake from another CPU queues the thread there and sends the
`WAKEUP` IPI, so a halted CPU runs it before its next tick.

- **Switching** (`arch::context::switch`, naked asm): pushes the
  callee-saved registers (rbx, rbp, r12-r15), saves `rsp`, loads the next
  thread's and pops its registers. There is no SSE/AVX state (`make audit`
  proves none is used; x87 isn't checked). It happens only with interrupts
  off and no critical section open (asserted): a held `SpinLock`'s section
  belongs to the CPU, so one held across a switch would be closed by
  another thread. Whether interrupts were on is a local on the switching
  thread's stack. State changes are checked (a thread moving from a state
  it isn't in panics), as is that both threads of a switch are its CPU's
  (not tested: correct code never trips them). A new thread's stack is
  laid out as if `switch` had been called with `context::thread_start` as
  its return address, which calls `sched::thread_entry` with `rbp` = 0, so
  its backtraces end there.
- **Preemption:** round robin on every CPU's 1 kHz local APIC timer: a
  thread runs 10 ticks (`SLICE_TICKS`) before the next ready thread on its
  CPU gets a turn (the idle thread gives way at once when the wakeup IPI
  ends its halt, or at the next tick while it runs a `run_on` job); the
  timer interrupt (after its EOI) switches, and the preempted thread later
  returns through the handler's `iretq`. Slices are counted in the CPU's
  own ticks (a ktest checks two threads' turns 20 ticks apart). Under TCG
  ticks arrive late, so slices are longer in time; tests bound waits by
  progress. The timer never allocates: each CPU's queues have room for
  every thread homed there (checked by a ktest, and asserted on every
  push).
- **Calls:** `yield_now`; `sleep(ns)` (a deadline from `now_ns`, checked by
  each tick on the thread's CPU, so it never wakes early; it runs once its
  turn comes after that); `exit` (returning from the entry point does it);
  `WaitQueue::wait_until(cond)` with `wake_one` (the longest-waiting thread
  first) and `wake_all` (the condition is checked under the queue's lock,
  so a waker that sets it and then wakes can't be missed; a thread woken by
  another CPU before it has switched out is neither lost nor queued twice;
  a ktest hands a turn between two CPUs 6000 times).
  `WaitQueue::wait_until_or_deadline(deadline, cond)` waits at most until
  `deadline` (whether `cond` held): the thread is on the queue and on its
  CPU's sleepers at once, and whichever of a waker and the tick that
  passes the deadline moves it out of `Blocked` first queues it; every
  wake only moves a thread that is still blocked, and a woken thread takes
  its own leftover entries off (a ktest races the two 200 times). Dropping
  a queue with threads waiting panics, as does a thread joining itself
  (not tested). `Blocked` is set in one place, `WaitQueue`'s wait, under
  the queue's lock, right after the thread is pushed onto it; from that
  push until it is woken nothing allocates for it (the deadline's sleeper
  entry uses room reserved beforehand). `yield_until_blocked(handles,
  deadline)` yields this CPU until every one of those threads is
  `Blocked` (each check one pass with interrupts off; true even past the
  deadline if they all are; `Sleeping` and `Dead` never count; not for the
  calling thread itself): the serial prompt's wait for the screen shells
  (#119; a ktest in which each of four threads blocks last in turn,
  after a sleep).
  Where a thread can't block, both waits panic (tested: `wait_until` with
  a lock held; `wait_until_or_deadline` with interrupts off; `wait_until`
  after its own CPU turned interrupts off for good, alone or after another
  CPU did too), except where it can't because a crash report or a reboot
  on another CPU turned interrupts off for good and its own CPU hasn't.
  A thread there finds them off once it closes a critical section,
  switches itself out and back in (a preempted thread returns through
  `iretq` with them on), or first runs; until the report's stop NMI (or,
  for a reboot, the reset) arrives, its wait halts the CPU instead, which
  the report counts as stopped (#106). Tested with a crash report: a
  thread closing a critical section (`crash bystander`), a thread's first
  run (`crash firstrun`), and a thread holding a lock, which still panics
  (`crash heldelsewhere`). Not tested: a thread switched back in after
  `sleep` or `yield_now`, the idle thread, a CPU without a scheduler, a
  reboot. Not told apart, so halting too: an interrupt handler, or code
  that turned interrupts off with `irq::disable`, waiting in that window.
  The other places that can't block (`run_sized`, `sched::sleep`, a
  virtio doorbell's wait) refuse or spin instead, and the stop NMI reaches
  them there. And
  `time::sleep` busy-waits (tested with a lock held, interrupts off, and
  on an idle thread; before the scheduler runs on the CPU, not tested).
- **Idle and boot:** each CPU's idle thread runs only when nothing is ready
  and is never queued. An AP's is its `ap_main` context (on its main
  stack), looping in `sched::idle_loop`, which starts the AP's timer once
  the boot CPU publishes the calibrated count and runs `run_on` jobs (a job
  runs only while no thread is ready there); the boot CPU gets an idle
  thread with a stack of its own, and `kmain`'s context becomes the `main`
  thread (the serial shell or the ktests). A CPU's scheduler runs from the first tick
  after it is armed (its current and idle threads set, its timer started):
  before that nothing could wake a sleeper there, so `spawn_on` refuses it
  and `time::sleep` busy-waits. `sched::init` (boot CPU, after
  `smp::wait_online`) wakes the APs and waits for every CPU's scheduler to
  start, 1 s in all; one that doesn't, or an IPI that isn't sent, is
  warned about, and one that starts later says so (not tested).
- **Exit:** only a spawned thread can exit, not `main` or an idle thread
  (asserted, not tested). Its stack is freed by the next thread to run on
  its CPU (the unmap's TLB flush is local; see "Other CPUs" on the
  spawner's entry); only then does `join` return.
- **Observability:** every switch records a `switch` trace event (previous
  and next thread's ids, the previous thread's state), and the serial crash
  report names the running thread (`thread: <name> (id N)`).
- **Not yet:** priorities, migration or load balancing, and a per-thread
  CPU time count (`top`, #63).

### Keyboard (built in M2)

`kernel/src/keyboard.rs`, the 8042's ports in `arch/x86_64/i8042.rs`.

- **Routing:** `interrupts::route_isa_irq(irq, handler)` (boot CPU only,
  asserted) takes the MADT's interrupt override for the ISA IRQ, if any
  (the GSI, polarity and trigger; ISA's edge, active high without one),
  finds the I/O APIC with an input for that GSI, registers the handler for
  vector `0x50 + gsi` (`idt::set_irq_handler`, while the input is still
  masked) and programs the input masked, for the boot CPU. It returns a
  `Route`, which only it can make (routes are never undone), with the
  input's trigger and polarity (`Route::trigger`, `polarity`: a driver can
  refuse an input its device can't drive, as `serial_in` does a level or
  active-low IRQ 4); the driver
  unmasks the input with it (`interrupts::set_masked`) once its device is
  ready. Redirection entries are a typed `apic::RedirectionEntry` (vector,
  delivery mode, destination mode, polarity, trigger, mask, destination;
  the I/O APIC's read-only remote IRR and delivery status are never set:
  written as 0, not tested, QEMU ignores writes to them), changed by
  read-modify-write (`IoApic::update_entry`).
- **8042 init** (once, boot CPU, after the framebuffer; IRQ 1 routed first;
  every reply polled with a deadline, 100 ms, 1 s for the keyboard's
  reset): with the controller's IRQs off, both ports are disabled (their
  clocks off), the output buffer flushed, the configuration byte
  rewritten (both IRQs off, translation to scancode set 1 on, both clocks
  still off; the system flag as firmware left it), the self-test run
  (0x55), the configuration written again (some controllers reset it
  there; QEMU's doesn't) and the buffer flushed again, port 1 tested, then
  enabled (its clock on), scanning disabled, the keyboard reset (an ACK and
  0xAA, in either order, within 1 s; 0xFC or 0xFD fails it; not resent),
  scanning disabled again, set 2 selected and scanning enabled (each of
  these commands ACKed, resent at most twice on 0xFE). QEMU never sends the
  reset's replies out of order or a failure, asks for a resend, or needs
  scanning disabled around the reset, so those branches are not tested.
  The controller's replies come while port 1's clock is off; while waiting
  for one of the keyboard's, a byte that isn't it (a key pressed during
  boot) is skipped and counted (a ktest plants one with 0xd2), up to 32 per
  reply (the bound not tested), and so is a byte from port 2; a byte the controller flags
  with a timeout or parity error ends init (no device answering; not
  tested). Then port 1's IRQ bit is set, the input unmasked and the output
  buffer drained once: a byte that was already waiting raised IRQ 1's edge
  while the input was masked, which the I/O APIC doesn't keep (a ktest has
  the 8042 put a byte in its buffer with command 0xd2 while the input is
  masked, then a port-2 byte with 0xd3, which is dropped). If init fails
  after port 1 was enabled, port 1 is disabled again (best effort); IRQ 1
  stays routed, masked. A missing controller (tested with QEMU's
  `i8042=off`), keyboard (not tested) or I/O APIC (the no-ACPI boot) is a
  warning (`no PS/2 keyboard: ...`, saying which step failed), not a
  failure. The init runs between the framebuffer and `complete` on the
  boot timeline, so its time counts toward `complete` (up to 100 ms with no
  controller and up to about 300 ms with no keyboard; not tested).
- **Bytes and decoding:** the queue's producer is the boot CPU with
  interrupts off (the IRQ handler, or the drain at unmask): it moves up to
  16 waiting bytes into a lock-free single-producer, single-consumer queue
  (`lumen-queue`, 256 bytes, host-tested; a full queue drops and counts),
  discarding the second port's and counting (and discarding) the ones the
  controller flags with a parity or timeout error (not tested).
  `keyboard::read_key` (never blocks) decodes them under a lock with
  `pc-keyboard` in the layout `lumen.keymap=` names into a `Key`: a
  character (Ctrl and a letter make a control character, Ctrl-A U+0001;
  a ktest decodes Ctrl-A, Ctrl-W and, on a German keyboard, Ctrl-Z), or
  another key pressed (arrows, PageUp, modifiers) with the modifiers
  after it (`lumen_input::Mods`); releases are skipped for the terminals
  (every key event, releases included, also goes to the desktop's key
  ring: section 7), and bytes the decoder refuses are counted (the
  keyboard's overrun code among them); Alt shows with keys that make no
  character (a ktest decodes Alt+F2).
  `keyboard::wait_for_bytes_or(ready)` blocks the calling thread until
  bytes are queued or `ready()` holds: the producer wakes the keyboard's
  wait queue whenever it queued a byte, and the waiter checks under the
  wait queue's lock, so a byte can't slip between its check and its sleep.
  The screen shells read keys only through the virtual terminals' router
  (`vt::read_key`/`vt::wait_key`, section "Terminal"): every keyboard
  scenario goes through it. The shell's
  `kbd` shows the layout, the route, the
  interrupts taken, the bytes queued, the keys pressed (`presses`: the
  producer counts set 1 make codes as bytes arrive, queued or dropped,
  before any reader decodes them, under one lock with the push, so a
  press shows with its byte or its drop; prefixes, fake shifts and Pause
  each one key or none: `lumen_input::Presses`, #149; it counts every make
  syntactically, so it matches the decoded presses only while `undecoded`
  stays flat; a ktest drives the producer's step with a full queue), the
  keys held now (`held`: pressed, not yet released), the bytes dropped,
  undecoded, flagged, and
  skipped during setup, and the desktop's key ring's newest event number
  (`newest key event`, #151, section 7), or why there is no keyboard (and
  whether IRQ 1 was routed); a non-zero skip count is also logged after
  init. `kbd events [n]` lists the key ring's newest events.
- **Tests:** QMP `send-key` drives the keyboard (`Machine.type_text` and
  `press`; QEMU queues each key behind the last one, so they arrive in
  order; its input queue holds 1024 entries and drops the rest without a
  word, so `press` paces itself on the time QEMU takes to send each key,
  and `test_a_long_line_of_shifted_keys` checks 150 shifted keys arrive;
  `lumen_harness.gui`'s `confirmed_keys` waits, before the next key, until
  `kbd` counts exactly the key's presses and bytes (`gui.KEY_BYTES`, a
  closed table `test_every_modelled_key_is_confirmed` pins against QEMU;
  any other key is refused), still exactly once QEMU has sent everything,
  with no key held and nothing dropped, undecoded or flagged, failing as
  "key N of M (...) never reached the guest", "... of its N bytes arrived
  ...: a prefix or release was lost", "... still held: a release was
  lost ..." or "... the keyboard's queue dropped N byte(s) ...": a
  confirmed key reached IRQ 1 whole, released, and was queued, while a
  loss in the decoder, the router or a shell still shows only on the
  screen, #149),
  and the scenarios read what the screen shells did
  back from the screen (`lumen_harness.screen`, through the serial shell's
  `term text`); a scenario boots with
  `lumen.keymap=de` in an ISO built for it, and one presses a key over and
  over from power-on until the keyboard is set up (some of those bytes are
  skipped during setup).
- **Not yet:** key releases for programs (Alt reaches the virtual
  terminals' router only, for Alt+F1 to F4), LEDs, the second port (a
  mouse), and hot-plug.

### Terminal (built in M2)

`crates/lumen-term` (the emulator and the virtual terminals' key rings,
host-tested), `kernel/src/term.rs` (the virtual terminals' grids and
drawing the shown one) and `kernel/src/vt.rs` (routing keys to them);
`fb.rs` keeps the pixels, the header and the crash screen.

- **The emulator:** `Terminal::write` runs bytes through `vte`'s parser
  into a grid of cells (a character, foreground and background, each the
  default or one of 256 colours, and flags: bold, inverse, the two halves
  of a wide character). It handles UTF-8, C0 controls (a line feed
  without a carriage return: `term::write_str_to`, `term::write_fmt_to` and
  log lines turn `\n` into CR LF), cursor
  movement, save and restore, erasing (in the pen's background), inserting
  and deleting cells and lines, scrolling, scroll regions, tabs, autowrap
  (the cursor waits at the last column), cursor visibility, and SGR for 16
  and 256 colours (truecolor approximated to the nearest of the 256; the
  first 16 are Lumen's palette, the rest xterm's). Wide characters
  (`unicode-width`, ambiguous ones narrow) take two cells, and breaking
  one in half blanks the other; zero-width ones are dropped; unknown and
  malformed sequences are ignored. Lines leaving the top of a scroll
  region that is the whole screen (a line feed, `CSI S`; never deleted
  lines) go into a scrollback ring (500 lines in the kernel). The view
  scrolls back through it and stays on the lines it shows as new output
  arrives, while the ring holds them; `CSI 3 J` clears it and shows the
  screen. Invalid UTF-8 prints U+FFFD. All of it is allocated in `Terminal::new`:
  writing never allocates (a host test counts allocations), because log
  lines come while the frame allocator's or the page tables' lock is held.
- **Damage:** the terminal marks the view's rows that changed and keeps a
  pending scroll (the region and how far), merging scrolls of one region.
  A renderer that moves what it drew by the scroll and then draws the
  marked rows shows exactly the view: a randomized host test checks that
  against a model renderer after every write.
- **Drawing** (`term.rs`, 176x41 cells of 7x16 pixels below the header at
  1280x800): under the screen's lock, the cursor is taken off, a pending
  scroll moves the pixels (`Framebuffer::move_rows`) and the shadow (a copy
  of every cell drawn) together, and in each marked row only the cells that
  differ from the shadow are drawn (a ktest counts them: a line feed at the
  bottom draws fewer cells than one row, not the screen; another draws
  scrolls up and down, of the screen and of a region, and checks the
  pixels against drawing every cell afresh). Bold uses the font's bold
  weight; characters the font lacks (it has Basic Latin only) are drawn as
  `?` (not tested on screen). On a framebuffer too small for one cell the
  terminal is skipped with a warning (not tested). The boot CPU's timer
  interrupt flips the cursor every 500 ms; if another CPU is drawing it
  doesn't wait, the flip is retried on the next tick (a ktest sees both
  phases; the pixels not tested). A thread would switch and trace twice a
  blink. Output shows the cursor solid for a whole blink (not tested), and
  it isn't drawn while the view is scrolled back (a ktest scrolls back with
  the cursor still in the view).
- **Writers:** log lines up to `info` are shown without the timestamp, the
  level coloured (ERROR bold red, WARN yellow, INFO green), the tag dimmed,
  errors' and warnings' messages coloured too (a ktest reads the cells),
  on the first virtual terminal. Each screen shell writes to its own
  (section "Shell"), nothing of it on serial (except what a `crash` says
  on serial and its terminal: a fault that didn't happen, the heap's line
  before a heap crash; the terminal half not tested). Each of its `write!`s is
  formatted first (it may allocate: a script value's `Debug` does) and then
  drawn under one lock, and each log line is formatted and drawn under one
  lock, so another CPU's log line can't split an escape sequence written in
  one `write!` (ktests check where each is formatted); a log line starts
  from the default colours (a ktest leaves bold, inverse and a background
  set first). A log line's arguments must not allocate (the heap asserts,
  a panic, that it isn't called under the screen's lock). A CPU that is already drawing (an
  exception in the middle) doesn't wait for itself: its line is left off
  the screen (nothing on that path logs to the screen today; not tested).
  No drawing starts once a crash report starts (`crash::enter` stops the
  terminal and makes any canvas inert; one under way on another CPU runs
  until that CPU is stopped; not tested), and the crash screen draws straight to the
  pixels, not through the terminal, its lock or the heap (a scenario
  checks its banner after an invalid opcode; drawing it with a corrupt heap
  or the terminal's lock held is not tested).
- **Shell commands** (either console, writing to the one that ran them):
  `clear` (the screen and its scrollback, on a console that takes escape
  codes), `colors`, `seq <n>`, `term` (size, cursor, scrollback, the view's offset
  and top row, and, for the whole screen, cells drawn and whether a command
  has the canvas: that part not tested) and `term text` (the view's rows, and the cursor counted in the view, or
  `cursor below the view` when scrolled back past it; tested); without a
  terminal, `term` says so (not tested); both read the shown terminal.
  In a screen shell Shift+PageUp/PageDown scroll its terminal's view half
  a screen (a ktest gives a hidden terminal's console Shift+PageUp and
  finds that terminal scrolled, the shown one not) (logged at debug
  level: serial only, the screen shows up to `info`); a key the line
  editor takes returns it to the bottom (a ktest checks that output keeps
  a scrolled-back view on its lines; the shell takes keys only between
  commands or while a command polls for them, so a scenario can't scroll
  back while a command writes). The view doesn't move while a canvas has
  the screen (not tested).
- **Canvas:** `Ctx::canvas()` (`term::canvas(owner)`) gives one command
  at a time the pixels below the header (`Canvas`: `put`, `fill_rect`,
  `clear`, `text`, in its own coordinates, clipped; `Err(CanvasError)`
  says why there is none: no terminal, a crash report, or another command
  has it). While it is live the terminal keeps taking output but draws
  nothing; dropping it clears the canvas and draws every cell again (a
  ktest checks the pause, the whole header left alone, the clipping, and
  the redraw: every cell, every visible character in ink). Pixel stores
  are relaxed atomics and `Canvas` is `Sync`, so several CPUs can draw at
  once (not tested here: #68's `every_cpu_rendered_rows` is its first
  multi-CPU user). Each command run gets its own `term::Owner`; when it
  returns, the shell takes back a canvas that owner left behind, never one
  a command on another console holds (`reclaim_canvas`, with a warning;
  a ktest runs commands that leak a canvas, with and without another
  owner's held). A canvas taken back, or any after a crash report starts,
  draws nothing more (`Canvas::is_live`; a draw under way stops at its
  next row or glyph) and its drop leaves the terminal alone (ktests check
  the taking back; the crash case is not tested).
- **Mouse pointer** (#98): drawn by `term.rs` over everything (header
  included) once the tablet reports a position (`term::move_pointer`, from
  the tablet's thread; never before, so a boot without one draws what it
  always did). A save-under arrow (`lumen_input::Cursor`, host-tested
  against a model screen with random drawing around it): each drawing
  step under the screen's lock that would touch the arrow's box (a cell, a
  scroll's rows, a full redraw, a header box) takes it off first, putting
  back what it covered, and the end of every step under the lock draws it
  again at its newest position. So the terminal's redraws never erase it
  and moving it leaves no trail (a ktest moves it over text, scrolls,
  clears, full redraws, the header's boxes and the screen's bottom corner
  and compares the pixels with drawing everything afresh; a scenario moves
  it across text, the header and its boxes, types below it and compares
  screendumps with a fresh redraw). While a
  canvas is live the arrow is off (a canvas draws without the lock, from
  any CPU, so it can't be kept out of the arrow's box); dropping the
  canvas draws it again (ktest and scenario). On the desktop (M4) the
  compositor draws the pointer instead (section 11, "The pointer").
- **Virtual terminals:** `term::VTS` (4) terminals, each its own grid and
  scrollback, all allocated at init (indexed by the shown one's number, so
  switching allocates nothing); one is drawn. Log lines go to the first;
  each screen shell writes to its own (`term::write_str_to`); writing to a
  hidden one draws nothing, and showing it (`term::show`) draws all of it
  (a ktest checks both, and that log lines stay on the first). The
  header's right end shows a numbered box per terminal, the shown one
  filled (a golden; left out on a screen too narrow to keep them right of
  the header's text, `fb::HEADER_W`: a ktest of the geometry).
- **Keys:** `kernel/src/vt.rs` decodes the keyboard's bytes under its lock
  and routes each key to the terminal shown when it routes it
  (`vt::route_key`, reading `term::shown()`), into that terminal's ring
  (`lumen_term::vt::Router`: 128 keys each, a full one drops and counts
  the newest, the first drop of each burst logged, a burst ending when the
  ring empties; host-tested against a model, and a ktest feeds 129 key
  presses to one terminal and finds one dropped and counted for it alone).
  Alt+F1 to Alt+F4 switch (a ktest maps them), so keys routed after a switch
  go to the new terminal (a ktest feeds the scancodes of `a`, Alt+F2 and `b`
  and finds `a` on the first, `b` on the second). Modifiers on their own
  take no slot; keys for a terminal whose shell didn't start are discarded
  and counted (that terminal says so; the counting ktested). Keys are routed as soon as a
  screen shell runs after IRQ 1 (whichever runs first routes for all, and
  wakes the others if a key went elsewhere), so a key typed just before a
  command on another terminal takes the canvas, but not routed yet, goes to
  that command (not tested). Every shell's `vt [n]` shows which terminal is
  shown, the keys each dropped and discarded and the switches refused, or
  switches.
- **Canvas and terminals:** a command on a terminal that takes the canvas
  brings that terminal to the front (each command's `term::Owner` carries
  its console's terminal), so keys routed from then on go to it, and while
  any canvas is held no other terminal can be shown (Alt+F`n` and `vt n`
  are refused and counted; Alt+F`n`'s logged, `vt n`'s answered on its
  console): the keys stay with the command drawing. The canvas switch
  doesn't take the router's lock, so a key being routed then lands on
  either side. A ktest runs a command through the shell on a virtual
  terminal's own console while another is shown and finds its canvas
  showing that terminal and a key routed meanwhile going there; a scenario
  types a script in terminal 1 that waits, then draws, switches to terminal
  2 meanwhile, and finds terminal 1 back in front when it draws and the
  next key reaching it; another runs `life` in terminal 1 and finds Alt+F2
  and `vt 2` refused and the ending key reaching it. A pipeline's first
  command has its console's terminal too. A command
  on serial leaves the shown terminal where it is. `term`, `term text` and
  the scroll keys work as above. A scenario types in terminal 1, switches
  to 2 with Alt+F2, types there, switches back and finds 1's text intact (a
  golden), and types on terminal 3 after `vt 3`.
- **Not yet:** a font with more than Basic Latin, underline and other SGR attributes, the
  alternate screen, origin mode, left and right margins, and resizing.

### Shell (built in M2)

`crates/lumen-shell` (host-tested) and `kernel/src/shell/` (the consoles,
`mod.rs`; the commands, `commands.rs`; the system commands, `sysinfo.rs`;
pipelines, `pipe.rs`; the deliberate faults, `crash.rs`).

- **Five shells, one per console,** each with its own line and history:
  the serial shell on the boot CPU's `main` thread (where kmon ran), and
  one per virtual terminal, each in a thread of its own (`shell-vt1` to
  `shell-vt4`, on the boot CPU, started only with a terminal and a
  keyboard: without a keyboard boot says `shell on serial (none on the
  screen: no keyboard)`, tested with `i8042=off`; without a terminal, or
  no thread for one, not tested). The serial shell blocks until COM1's
  interrupt brings a byte (`serial_in`, section 4, step 4), or polls, yielding,
  without one; the virtual terminals' shells block in `vt::wait_key`.
  The serial shell's first prompt comes only once every screen shell that
  started is blocked waiting for its first key (`sched::yield_until_blocked`
  on their threads, see Scheduler; any `Blocked` counts, which relies on a
  screen shell's first wait being the keyboard's). So when the first line
  typed on serial runs, the screen shells are parked, and a deliberate
  crash on another CPU doesn't find one of them mid-allocation with the
  heap's lock held (#119). The `lumen> ` the harness waits for comes after
  that. Not covered: other threads on CPU 0, such as the `mouse` and
  `sound` threads (no scenario attaches those devices by default; `make
  run` attaches a tablet), and a key typed on the screen, which wakes all
  the shells: `wake_all` takes the queue's buffer and the waker frees it,
  so the first shell to wait again allocates a new one (the rest fit in
  it). A key typed during boot holds the serial prompt until they all wait
  again. No time bound: after 5 s a warning names the shells not yet
  waiting, each with its state (`serial prompt held: shell-vt2 (ready) not
  yet waiting for keys (still waiting)`; `(none now)` if they all parked
  meanwhile), the wait goes on, and its end is logged at info with how
  long it held (`serial prompt released after 5012 ms: ...`; a ktest
  waits with a 1 ms warning, and test_ktest matches both texts in the
  ktest boot's output; not at boot). The warning needs CPU 0 to
  still schedule: a shell spinning with interrupts off would hang it
  silently. Logged at debug level when it starts (`serial prompt waits for
  4 screen shells to wait for keys`) and before the prompt (`screen shells
  waiting for keys: 4 of 4`, what the wait's last check found). Every
  `test_smp` boot but one (`test_cpus_past_the_limit_are_halted`, at the
  default log level as shipped) and
  `test_shell::test_serial_prompt_waits_for_the_screen_shells` check that
  line, with no prompt and no warning before it; most other scenarios
  boot at the default level.
  Log lines go to serial and the first virtual terminal. A terminal whose
  shell's thread didn't start says so on it (not tested).
- **Words** (`lumen_shell::words`): blanks separate words; `'...'` keeps
  everything; `"..."` too, except that a backslash escapes `$`, `` ` ``,
  `"`, `\` and a newline; outside quotes a backslash escapes anything;
  parts next to each other make one word, `''` an empty one; `#` at the
  start of a word starts a comment. A randomized host test checks it
  against `shlex` on 200,000 lines (`words::split`, where `|` is a
  character like any other, as in `shlex`). The shell splits with
  `words::split_pipeline`: a `|` outside quotes and not escaped ends a
  command (`a|b` too), and needs one on each side. An unclosed quote, a
  trailing backslash or a `|` with nothing before or after it is one line
  of error (`syntax error: ...`), and so is an unknown command (`unknown
  command: x (try 'help')`).
- **The line editor** (`lumen_shell::Editor`) takes keys and writes what
  the console must show: printable characters inserted at the cursor (up
  to 160 columns a line, a wide character two, fewer on a console too
  narrow: the prompt, the line and Ctrl-C's `^C` must fit; zero-width ones
  refused; a character that doesn't fit makes Enter say `line too long
  (more than N columns): not run` and drop the line instead of running it
  cut short, tested on plain serial; the screen's and `lumen.ansi`'s
  narrower limits not tested on their own; a screen too narrow for 16
  gets no shell, not tested),
  Left/Right, Home/End and Ctrl-A/E/B/F, Backspace, Delete and Ctrl-D,
  Ctrl-K/U/W, Up/Down and Ctrl-P/N through the history (100 lines; blank
  lines and repeats of the newest not kept; going past the newest brings
  back the line being typed), Ctrl-C (drop the line), Ctrl-L (clear the
  screen and show the line again), Enter, and Tab, which completes the
  word before the cursor from the command table (command names first,
  then each command's argument words: `crash` its kinds and `thread`,
  `term` `text`; after a `|`, only the filters, then their arguments
  (`where`'s operators, `sort`'s `desc`); options that aren't
  printable are skipped): one that
  fits is written in full (quoted as the word was, the quote closed) with
  a space (left out if only that doesn't fit), several up to where they
  differ (host-tested only: wherever a prefix matches several of today's
  words, they already differ right after it), or, if they already differ
  there or that doesn't fit, listed on a line of their own. With
  ANSI escapes (the screen; serial with `lumen.ansi`) it redraws the line
  (`CR`, the prompt, the line, `ESC [ K`, the cursor moved back) unless
  the change only appends or only moves the cursor. In **plain** mode
  (serial without `lumen.ansi`, so the harness can read it line by line)
  the cursor stays at the end and the console only gets characters to
  append and `BS space BS` to erase: a history line replaces the typed one
  by erasing and typing; the cursor keys, Home/End, Ctrl-A/E/B/F, Delete,
  Ctrl-D, Ctrl-K and Ctrl-L do nothing (the cursor is always at the end);
  a listing is a line followed by the prompt and the line again. Neither
  mode wraps (with `lumen.ansi` a serial terminal is taken to be 80
  columns wide; plain serial doesn't redraw, so its 160 columns may wrap
  on a narrower terminal). A
  randomized host test runs the editor's output, in both modes, through
  `lumen-term` after every key and checks the cursor's row shows the
  prompt and the line, and the cursor sits where the editor says.
- **Keys:** the screen's from `vt::wait_key` (Enter `\n`, Backspace
  U+0008, Delete U+007F, Tab, Ctrl with a letter U+0001 to U+001A, the
  arrows, Home and End; a ktest maps each); serial's from bytes
  (`lumen_shell::input::Decoder`: CR, LF or CR LF as one Enter, DEL and BS
  as Backspace, other control characters as Ctrl and a letter, `ESC [`
  and `ESC O` sequences for the arrows, Home, End and Delete, a C0
  control (below 0x20) ending a sequence it interrupts, UTF-8 without
  overlong forms; host tested). The serial shell's writes are made holding
  the log's line lock (`klog::LockedSerial`, see step 4 of the boot
  sequence), so a log line can't land inside one (not tested on its own).
  Each log line is written whole (the logger holds the line lock for the
  line; `crash`'s `Out` takes it per piece, and the crash reporter takes
  none), but one can land between two of the shell's writes: before or
  inside a typed line's echo (a write per key, Enter's newline its own),
  between two writes of a command's output, or after the output, before
  the prompt. The log isn't held while a command prints (#124): a line
  shows when it is logged, in stamp order, so a warning during a long or
  stuck command isn't hidden until it ends. The scenarios read a
  command's output through `lumen_harness.shell.run`: it takes log lines
  out wherever they start (the format of "Boot timeline and log
  timestamps", matched by `lumen_harness.klog`; change it with
  kernel/src/klog.rs) and keeps them on the result, from the prompt
  before the echo on, and a WARN or ERROR among them fails the run unless
  the test allows that line (a warning is something a test should see). A
  command whose output is log lines (`dmesg`) is read with `run_dmesg`,
  which drops (and checks) the lines logged while it ran. Scenarios put a debug line inside a
  command's echo, between two of its output writes, and after its output
  before the prompt (#124's position); harness self-tests cover all three
  positions on handcrafted text, and the position after the output on
  #124's recorded output.
  Each key's output is written to the console in
  one write, so on the screen a log line can't land in the middle of a
  redraw (not tested on its own). A running command reads keys from its
  console with `Ctx::poll_key` (never blocks) and `Ctx::wait_key`
  (`Console::try_key`/`read_key`; not tested here: #63's `top` is the
  first user).
- **Commands:** each `COMMANDS` row carries its usage and a line of help
  (`lumen_shell::Command::usage`/`about`; an `about` ending in `:` is
  followed by its first argument's words), and `help` is generated from
  them (a ktest checks it has a line for each command in the table, no
  name is there twice, and the layout), `echo` (its words joined by
  single spaces), `int3`, `crash <kind> [cpu|thread]` (the thread is
  `shell-crash`), `trace [n]`, `heap`, `ticks`, `kbd`, `term [text]`,
  `clear`, `vt [n]` (the virtual terminals: above), `colors`, `seq`, `sleep`, `random [bytes | --stats]`
  (section 7, "Randomness"), `uptime`, `clocks`, `exit` (kmon's;
  besides what this list says, the deliberate panic now says `from the
  shell`, and `crash <kind> <cpu>` reports a fault that returned) and
  `history` (the console's lines, numbered), `reboot` and `shutdown` (see
  "Reboot and power-off" in section 4). Each writes to the console that ran it (at the start of a pipeline,
  to the next command; its usage and error lines to the console all the
  same, `Ctx::fail`). On plain serial,
  `colors` and `clear` say the console is plain (tested), and with
  `lumen.ansi` they work there (tested). A `crash` whose fault doesn't
  happen says so on serial and on the virtual terminal that ran it (the
  first for serial's; the terminal not tested). `crash <kind> <cpu>` waits, asleep and allocating nothing,
  until that CPU's report stops this one or its fault returns (so the
  report doesn't find the heap's lock held by the shell freeing its line;
  a screen shell could still allocate if someone types there
  meanwhile, not tested). After 10 s (not tested) the job is taken back
  if the CPU hasn't taken it; while one is out, another is refused.
  `crash dfree` and `crash heapcorrupt` print the heap's line before the
  damage, which the tests compare with the report's.
- **System commands** (`sysinfo.rs`; each a `<name>: ...` header and,
  but for `date`, rows; output with rows is made into one string first
  and written in one write, except `dmesg`, which writes whole lines in
  chunks of about 512 bytes):
  - `cpus`: each CPU online, its local APIC id, whether its scheduler
    runs, the ticks it has taken and how busy it has been since it
    started (the share of ticks that didn't find the idle thread running:
    the scheduler counts each tick against the running thread and, on the
    idle thread, as idle; sampling at 1 kHz; a ktest spins on a CPU and
    leaves it idle and checks both), and the threads homed there
    (`sched::threads`, a registry of weak references); then CPUs stuck
    starting, and in the header those halted past `MAX_CPUS` (neither
    tested). An idle boot CPU reads nearly 100% busy only where the serial
    shell polls serial: without COM1's interrupt (`serial_in`, #120).
  - `mem`: frames used of those the allocator owns, the heap's counters,
    and the kernel stacks registered, by kind (sizes without their guard
    pages).
  - `irqs`: every I/O APIC input decoded (vector, delivery, trigger,
    polarity, destination, masked; an input no driver routed is shown only
    as masked; `no I/O APIC` without any, not tested), then the interrupts
    taken on each vector that took any, has a routed input or is a
    device's MSI-X vector (`idt::vector_count`: `lumen_exception_dispatch`
    counts every vector, all CPUs together), with what the vector map uses
    it for, or for a device's MSI-X vector `msix <pci address> #<table
    entry>` (`pci::msix_vectors`), listed with a count of 0 before it
    fires.
  - `lspci`: the PCI functions found at boot (`pci::devices`: address,
    vendor and device ids, class and device names, and the driver that
    has each (section 7)), the header
    saying why any are missing (`pci::status`).
  - `mouse`: the tablet (#98, section 7): a header with its name,
    address, screen and counts (events, frames, ignored, `SYN_DROPPED`s,
    overruns: batches that may have lost frames, the pointer ring's newest
    event number (#151, section 7), why it failed), and one
    row: position,
    buttons held, presses so far (summed), wheels, the pointer (`drawn`,
    `hidden` under a canvas, `none` without a terminal), `stale`, and why
    it failed; `no tablet` and a row of dashes without one. `mouse hold`
    stops the `mouse` thread reading the tablet until `mouse release`: it
    returns once the thread has parked (the header ends `, held`, or
    `, holding` until then), and fails with no tablet, a failed one (also
    one failing while it waits), the request taken back first (a release,
    or another console's hold timing out), or no park within
    `HOLD_WAIT_NS` (1 s, a bug, logged). Events wait in the device's ring and frames that don't
    fit are dropped, an overrun made on purpose (#142). The hold is every
    console's (`mouse hold | ...` holds too), and both are logged at INFO.
    `mouse events [n]` lists the pointer ring's newest frames (#151).
  - `dmesg`: the newest whole log lines as serial printed them: the logger
    keeps the last 16 KiB in a `lumen_trace::TextRing` (host-tested
    against a model; not lock-free: `klog`'s `RING` lock, a leaf, taken
    under the line lock by the logger and alone by readers). Only whole
    lines are shown: a full ring's oldest, cut, line and a line another
    CPU is still writing are left out; the header says whether older lines
    were overwritten (`klog::written`; the overwritten case not tested in
    the kernel).
  - `date`: the CMOS RTC (`rtc.rs`; ports in `arch::cmos`, whose guard
    serializes index/data pairs; reading register C would acknowledge RTC
    interrupts, and every index write leaves NMIs enabled): wait out an
    update in progress (at most 10 ms per read; 0xff means no clock, a
    divider other than the running one a stopped oscillator), read
    until two reads agree (at most 5 reads), refuse a clock that lost
    power (status D) or whose updates are halted (status B's SET), decode
    BCD or binary and 12- or 24-hour hours (all in `lumen_time::rtc`,
    host-tested; a decode failure is logged with the raw registers). The
    clock's zone is whatever set it; `date` takes it as UTC (true under
    QEMU's `-rtc base=utc`, in every test); Lumen never writes the clock. The century is taken as 20xx (the FADT's
    century register isn't read). QEMU's RTC ticks on a phase of its own:
    measured, it reads 1 to 2 s behind the host's clock; the test checks
    it is no later than the host's clock and at most 3 s behind its whole
    second.
  - `top [frames]`: every second, the CPUs (busy % and ticks per second
    over the last second; `stalled` for one that took no tick, not
    tested), interrupts per second by vector (from 32 on), the heap and
    frames, and
    every thread with its share of its CPU's ticks; it polls the console
    for a key (`Ctx::poll_key`) between 20 ms sleeps and stops on `q` or
    Ctrl-C (or after `frames` frames). The scenario checks frames 0.9 to
    1.5 s apart and the timers at about 1 kHz. With escape codes each
    frame replaces the last (home, erase; not tested); on plain serial
    frames follow one another as lines.
  - `threads`: every thread (`sched::threads`), its CPU, state and the
    ticks that found it running.
- **Pipelines** (`lumen_shell::pipe`, host-tested; `pipe.rs`): `cmd | filter
  | filter ...`. The first command must not be a filter, and every
  command after a `|` must be one. A command's output is lines of text,
  or records: a `pipe::Table` of named columns (names unique), each
  holding numbers (printed in decimal, hex, four-digit hex for 16-bit ids,
  or as a percentage) or text, a value possibly absent (printed `-` or
  `stalled`). In a pipeline `cpus`, `irqs` (its per-vector table only),
  `mem` (frames, the heap and each kind of stack: size and used in bytes, a
  stack's all used, guard pages not counted; and a count of frames, live
  allocations or stacks), `threads`, `lspci`, `mouse` and `sound` (one
  row: its state and counts; none without a device) give records
  (`sysinfo::RECORDS`; `lspci` first writes a note line to the console
  when PCI devices are missing, which its table can't carry, and `sound`
  when there is no sound device); `cpus`,
  `irqs`, `threads`, `lspci` and `mouse` print the same tables alone (under their header line), so the
  text of `cpus` and `irqs` is what it was (tested column by column; one
  difference: a value wider than its column, `stalled`, now widens it for
  every row). Given arguments they run as commands: `usage: cpus`, or
  what the arguments ask for (`mouse hold`, `mouse release`). Any other command
  runs as alone on a plain console (no escape codes), writing into a
  buffer of 64 KiB reserved before it runs (writing never allocates) whose
  lines go on. Its keys still come from the console, until it writes more
  than that: then a key it asks for is Ctrl-C (a command polling for one,
  `top` or a script, stops), its own failure line isn't shown, and the
  pipeline stops with a line saying it wrote too much. Its usage and error
  lines go to the console instead (`Ctx::fail`: every command's usage and
  error lines go through it; status answers such as `kbd`'s `no keyboard`
  are output) and end the pipeline; so do the refusals of what would wait
  on the console unseen or lose its line: the rhai REPL and demos, `top`
  without a frame count, whatever draws (`mandelbrot`, `life`, a script's
  drawing functions), which waits for a key after, `clear` and `colors`
  (no escape codes into a pipeline), `reboot` and `shutdown`. The filters: `grep
  <text>` (lines, or rows with a value as printed containing it), `where
  <column> <op> <value>` (`= != < <= > >=`, and `~` contains for text; a
  number column takes a number, decimal or `0x` hex (a hex column only
  hex: `vector = 64` is refused, not read as `0x40`), a percentage
  column's maybe with `%`, else an error; text compares byte by byte,
  unless the value is a decimal number and the operator isn't `~` (which
  always looks for text): then lines that are numbers compare as numbers
  and the others don't match, so `seq 12 | where line > 5` is 6 to 12; an
  absent value never matches, not even `!=`), `sort <column> [desc]`
  (stable; in a text column lines that are decimal numbers by value and
  before the rest, as `where` reads them; absent values last either way),
  `first <n>`, `count` (one line) and
  `select <column>...` (each once). Text through `where`, `sort` or
  `select` is a table with one column, `line`, and comes out as text. A
  wrong filter (unknown, or the wrong arguments, a column selected twice
  among them) stops the pipeline before anything runs; one failing on its
  input (no such column) stops it there; each is one line. The last output
  is printed in pieces of whole lines (`lumen_shell::pipe::chunks`,
  host-tested: each at least 512 bytes but the last, less than that plus
  one line; `shell::write_in_chunks`, which `dmesg` uses too: a serial
  write holds the log's line lock with interrupts off): text as lines, a
  table as its
  headings and rows (two-space indent, columns two spaces apart and as
  wide as the widest value, a left-aligned last one unpadded). Alone, a
  filter says it goes after a `|`. After a `|`, Tab completes only the
  filters' names.
- **Tests:** `cargo:lumen-shell`; `test_shell` (echo with quotes on both
  consoles, history and line editing on the screen and history on serial,
  completion on both, one line of error, `lumen.ansi`, the system
  commands' headers and rows under `-smp 2`, `date` against the host's
  clock, `top`'s frames, rates and `q`, pipes of text on both consoles,
  records through `where`, `sort`, `first` and `select`); the keyboard and
  terminal scenarios type on the screen's shell and read the screen back
  with `term text`; every serial scenario that drove kmon runs against the
  serial shell with the new prompt.
- **Not yet:** Ctrl-R search, Alt-B/F word moves, wrapping lines longer
  than the console, a full error stream (besides log lines and `crash`'s
  own reports, only `Ctx::fail`'s lines leave a pipeline for the console,
  and they end it; headers and notes, such as `dmesg`'s first line and
  `trace`'s note on the ring's size, are data like the rest), the I/O
  APIC's inputs as
  records, redirection, variables, and a shell in user mode (M9).
  Scripts: see below.

### Scripting (built in M2)

`kernel/src/script.rs`, the shell's `rhai` command, with
[rhai](https://rhai.rs) 1.27 (integers only, no modules, no time, closures
that can't capture (`no_closure`), `curry` disabled; see the crates row in
section 2).

- **Forms:** `rhai` alone is a REPL (the shell's line editor with its own
  history, prompt `rhai> `, Tab completing Lumen's functions; variables
  kept from line to line, one a name: redefining one doesn't use up the
  variable limit; `exit`, `quit` or Ctrl-D on an empty line leaves);
  `rhai -e <expr>` evaluates its words joined by spaces; `rhai demo` lists
  the built-in scripts (`kernel/src/script/*.rhai`: `rainbow`, `fib`,
  `count`; make rebuilds the kernel when one changes) and `rhai demo
  <name>` runs one, until M6 brings files. A value other than `()` is
  printed (at most 64 KiB of text, else an error saying so); an error is
  one line, `rhai: ...` (the innermost error, a limit in its own words, and
  `in function 'f'` naming the innermost function it happened in; a thrown
  value's text cut at 64 KiB).
- **Lumen's functions:** `cpus()` (each online CPU as `#{index, apic}`),
  `mem()` (`heap_size`, `heap_used`, `heap_free`, `heap_live`),
  `uptime()` (ms since the boot timeline's origin), `trace(n)` (the last n
  events of this CPU's ring, the boot CPU's, where both shells run; gap
  lines included), `sleep(ms)` (clamped to 0 to 60 s), `random()` (any
  integer) and `random(n)` (0 to n - 1, n above 0; both refuse while no
  randomness source has ever answered: section 7), `beep()` /
  `beep(hz, ms)` (440 Hz for 200 ms by default; returns while it plays,
  cutting off one still playing at its next period and dropping one still
  waiting; an error without a sound device or out of range),
  `print` and `debug` (to the console), and drawing on the terminal's canvas:
  `draw_pixel(x, y, rgb)` (nothing off the canvas), `draw_rect(x, y, w, h,
  rgb)` (clipped), `draw_text(x, y, text, rgb[, bg])` (text starting above
  the canvas is left out, so are characters starting left of it, and text
  is cut at its right edge), `clear([rgb])`. `screen_width()` and
  `screen_height()` give the canvas's size without taking it. The canvas is
  taken on the first draw (an error naming why if it can't be). A script
  that drew and succeeded says `rhai: press a key on <console>` (`serial`,
  `the keyboard`) on a strip at the canvas's bottom and on its console, and
  keeps the canvas until a key is pressed there; one that failed (Ctrl-C
  included) gives it back first, so its error shows. On the screen's
  shell, what a script prints while it holds the canvas reaches the
  terminal but is drawn only when the canvas is given back.
- **Its own thread:** each run (or REPL session) runs in a thread with a
  1 MiB stack (`sched::run_sized`, which refuses on a thread that can't
  block and otherwise waits for it, so the closure borrows the console;
  `Console` is `Send`). rhai recurses for calls, nested expressions and
  every walk over a nested value (clone, drop, print, size checks); the
  limits below bound how deep those go, and the stack is at least twice
  what the deepest cases need: the ktests
  `deep_recursion_is_an_error_not_a_crash` and
  `nested_values_are_an_error_not_a_crash` run them on 512 KiB.
- **Limits** (`script::LIMITS`, checked at compile time to be nonzero, as
  rhai takes 0 for unlimited), each an error rather than a guard-page crash
  or an out-of-memory panic:
  - 200 million operations (10 to 30 s at the 7 to 18 million a second
    measured locally; Ctrl-C is quicker), 16 call levels, expression depth
    24 (16 in functions), 256 variables in a scope;
  - per value: 64 KiB of string bytes, 1024 array items (BLOB bytes count
    as items) and 256 map items, counted by rhai over the whole value,
    nested values included, when it checks one: a literal, an operator
    assignment, a native function's result and receiver, a property
    assignment (the map), an item assignment (only the item: a key added
    that way isn't counted until the map is checked again); not plain `=`
    or a script function's arguments and result. Each nesting level is an
    item, so values nest about 1280 deep (an item assignment can add one
    level unchecked, which `script::equal` caps);
  - text made from one value: 64 KiB. rhai's `==`, `!=`, `to_string`,
    `to_debug`, `print` and `debug` on arrays and maps, and `to_json` on
    maps, call back into the engine for every item (about 1.7 KiB of stack
    a level) or make the whole text at once; Lumen replaces them with Rust
    walks (`equal`, `format_array`, `format_map`, `json`) that also stop at
    64 KiB of text. Past that a `to_string`-like function empties the value
    and stops the run ("more than 64 KiB of text"): rhai can't be let fail
    there, as it then checks the value and on that error makes the text
    itself, whole (a map with thousands of keys added by item assignment,
    all one 60 KB string, would be hundreds of MB);
  - heap growth during a session (a run, or a whole REPL): 32 MiB,
    checked every 64 operations (it can overshoot by what those allocate;
    other threads' allocations count too). In the REPL a line stopped by
    it drops every variable ("(variables cleared)"), so the next line runs.
  `curry` is disabled and closures can't capture: curried arguments nest
  without being counted.
- **Ctrl-C** stops a script: the console is asked for a key at most every
  10 ms (the first 10 ms after the start, so keys typed ahead of a short
  script stay), from rhai's progress hook (every 64 operations), from every
  drawing call, after each `print` and `debug` (a stop found in the last
  statement still stops the run), and while `sleep` waits; it is found
  inside closures and native callbacks too. Each time, the keys waiting on
  the console are taken up to a Ctrl-C; the others are dropped.
- **The console:** rhai's callbacks must be `'static`, so they reach the
  console through a slot (`ConsoleSlot`, neither `Send` nor `Sync`) shared
  with the command, which holds it only while an evaluation runs. Output
  is formatted before it is written: formatting a value allocates, which
  the heap asserts against (a panic) under the screen's lock.
- **Hashing:** rhai seeds its function hashes from a value `const-random`
  makes when rhai is compiled; the kernel sets a fixed seed before the
  first engine, so runs don't depend on it (the compiled-in value is still
  linked, so images differ by it between builds).
- **Tests:** `test_script` (`rhai -e` with `-smp 4`, the REPL, the demos,
  a map of 8000 references to one 60 KB string made into text, the
  `rainbow` golden and the terminal back after a key, Ctrl-C on serial and
  on the keyboard, the screen shell printing values whose formatting
  allocates, `screen_width()` without taking the screen, the strip and the
  terminal back); ktests for the functions, one-line errors and the
  limits' messages (integers only, no modules, curry and capturing refused
  among them), bounded text, the engine's limits, Ctrl-C (closures,
  functions, `sleep`, a last `print`, drawing, the `count` demo), the heap
  budget for a run and across a REPL, recursion and nesting on half the
  script stack, drawing and waiting for a key, and the REPL going on after
  Ctrl-C and errors.
- **Not tested:** the REPL on the screen shell or with `lumen.ansi`, and
  its Tab completion and history; `sleep`'s 60 s clamp; the heap budget's
  overshoot; running out of memory or stack window for the script thread.
- **Not yet:** scripts from files (M6), floats, capturing closures,
  scripts in user mode (M9), a script running in the background.

## 7. Driver interface (M3: PCI discovery, claiming, the virtio adapter, MSI-X, virtio-rng, the tablet and sound built; device classes later)

PCI discovery (#94), claiming, the virtio adapter (#95) and MSI-X (#96,
"Interrupts for devices: MSI-X", below) are built. The first drivers
(#97-#99) add their rows to the driver table: virtio-rng (#97,
"Randomness", below), the tablet (#98, "The tablet", below) and sound
(#99, "Sound", below) are built, and interrupt through MSI-X.

M3 lays the layer drivers stand on: finding PCI devices (#94), handing a
driver exactly its device's resources and running `virtio-drivers` on top
(#95), and message-signalled interrupts (#96). The first drivers are rng
(#97), the tablet (#98) and sound (#99). They expose plain module functions
(`rng::fill`, `mouse::status`, `sound::play`, with those issues). The message shape (an owned request in, a
completion out, no shared mutable state across the boundary, so a driver
can move to a user-space server by serializing the same messages) comes
with the device-class traits at the end of this section.

### Discovery: PCI (built, #94)

- **Configuration space through ECAM.** `lumen-acpi` parses the MCFG
  (host-tested) and judges each entry on its own: a base that is 0 or not
  1 MiB aligned, buses numbered backwards, a range not ending below 2^64,
  buses overlapping an entry accepted before it, or a place past the
  first 256 entries get that entry refused with its reason (a warning
  each), and the others are still used (a partial entry at the table's
  end refuses the whole table). The
  kernel maps each accepted region once, uncached (1 MiB per bus: 256 MiB
  for q35's 256 buses, out of the 1 TiB MMIO window; the pinned OVMF moves
  q35's from QEMU's 0xb0000000 to 0xe0000000), behind an accessor private
  to the `pci` module (the address arithmetic, `lumen_pci::ecam_offset`,
  is host-tested). No ACPI, no MCFG, no usable entry or no region mapped
  means no PCI devices: a warning, and the reason in `pci::status()`,
  which `lspci`'s header shows (`lspci: 0 functions (no PCI: running
  without ACPI, so no MCFG)`; in a pipeline, a note line on the console),
  not a crash; refused entries and unmapped regions are counted there too,
  with a pointer to the boot log. Only the no-ACPI case runs in QEMU; an
  MCFG that is missing, empty or wholly refused, and a region that can't
  be mapped, are not tested booted. The legacy `0xcf8` ports aren't
  used.
- **Decoding in a host-tested crate.** The `lumen-pci` crate holds the
  header, BAR and capability decoding behind a small config-space trait
  (32-bit reads and writes at a function and offset), tested against a
  fake config space. It is ours rather than `virtio-drivers`' `PciRoot`
  because it lands before #95 links that crate, and that crate's
  `enumerate_bus` ignores the multi-function bit, knows no segment, and
  its capability walk has no loop guard (as of virtio-drivers 0.13); ours
  is host-tested.
- **Enumeration** is a brute-force scan: every bus in each region, devices
  0-31, functions 1-7 only for multi-function devices. Bridges' secondary
  buses normally fall inside the scanned range, so no recursion; a bridge
  with no secondary bus, bus numbers that don't fit the bus it is on or
  run backwards, or buses not all in a mapped region is a warning (the
  devices behind it aren't listed; host-tested, not booted). BARs are sized with I/O and memory decoding turned off
  in the command register while all-ones is written (and restored after),
  except on a host bridge, sized with decoding on as Linux does
  (`mmio_always_on`: turning a host bridge's decoding off can cut the CPU
  off from memory on real hardware). That must happen while nothing can
  touch a device: `pci::init` runs right after ACPI on the boot CPU alone,
  with interrupts off, before the framebuffer's terminal draws (the screen
  is inside the VGA BAR), and nothing logs while a device's decoding is
  off. A panic then reports on serial only (there is no crash screen
  before `fb::init`). `virtio-drivers` sizes the BARs it uses again while probing
  (below). The firmware's BAR addresses are kept: OVMF assigns them and
  Lumen never reassigns in M3. A BAR left at address 0 is
  `Bar::Unassigned` (its size kept), and a reserved memory type, a 64-bit
  BAR in the last slot, an I/O range past port 0xffff or a 64-bit range
  ending at 2^64 is `Bar::Malformed`: each is a warning naming the
  function and the BAR (host-tested; q35's BARs have none).
  `DeviceInfo::bar_slot` says why a slot holds no BAR (out of range, not
  implemented, or the upper half of a 64-bit BAR, by `Bar::slots`). The capability walk is guarded (pointers have
  their two reserved low bits masked, as the PCI specification says, so
  offsets are dword-aligned; below 0x40, or a 49th entry, cuts the list
  off); an unknown header type's (3-127) BARs and capabilities aren't
  read (its header is), and a CardBus bridge's BARs and capability list
  (its pointer, at 0x14) aren't decoded. A device whose list was cut off
  or not read is listed, warned about, and `pci::claim` refuses it
  (recorded as `<driver>: not probed: capability list ...`; the cut-off
  detection is host-tested, the claim's refusal isn't tested). Each function is logged at debug level
  (`DEBUG pci: 0000:00:00.0 8086:29c0 host bridge (class 060000) Q35 host
  bridge`); the warnings' text is `lumen_pci::Problem`'s, host-tested.
- **`DeviceInfo`**, one per function, built once at boot (after `mm::init`
  and ACPI, before any driver) into `pci::devices()`, in address order,
  which never changes afterwards:

```rust
pub struct DeviceInfo {
    pub addr: PciAddr,                  // segment, bus, device, function
    pub vendor: u16, pub device: u16,
    pub class: u8, pub subclass: u8, pub prog_if: u8, pub revision: u8,
    pub subsystem: (u16, u16),          // vendor, id
    pub bars: [Option<Bar>; 6],         // Memory { phys, size, prefetchable, is_64 } | Io { port, size }
                                        //   | Unassigned { io, size, is_64 } | Malformed(BarProblem)
    pub caps: CapList,                  // (id, offset): MSI, MSI-X, vendor-specific (virtio's); complete, cut off, or not read
    pub irq_pin: u8,                    // INTx pin, recorded only: INTx is never routed (no `_PRT`)
    pub buses: Option<BusNumbers>,      // a PCI-to-PCI bridge's primary, secondary, subordinate buses
}
```

- **The registry.** One slot per function, in `pci::devices()` order:
  its memory BARs (recorded at enumeration) and who has the function
  (`Binding`, below). BARs are
  mapped only by the registry, through a path private to `pci`
  (`map_bar`, returning a `BarMapping { phys, virt, size }`), once per
  BAR, when its device is claimed, and never unmapped (`map_mmio`'s
  window only grows); `mapped_bar` looks a mapping up without mapping
  (for the virtio adapter's pre-probe check and `mmio_phys_to_virt`, and
  the MSI-X table). The registry is the one record of BAR mappings; everything else
  looks them up there. Nothing else maps BARs, except that the
  framebuffer Limine set up inside the VGA BAR is in the HHDM, so
  `map_mmio` refuses that BAR. A refusal says which function and BAR, and
  why (`BarError`: no such function, out of range, not implemented, the
  upper half of a 64-bit BAR, an I/O BAR, unassigned, malformed, not
  mapped yet (the lookup), or the mapping's error with the physical
  range). `lspci` lists `pci::devices()` as records: `addr`,
  `vendor` and `device` (four hex digits: `lspci | where vendor =
  0x1af4`), `class`, `name` and `driver` (the binding, below).

### Claiming and probing (built, #95)

```rust
pub struct Driver {
    pub name: &'static str,
    pub matches: fn(&DeviceInfo) -> bool,
    pub probe: fn(DeviceResources) -> Result<(), ProbeError>,
}
```

- One static table, `drivers::DRIVERS`, one row per driver (each driver's
  issue adds its row: virtio-rng first, #97, then sound, #99). `drivers::probe_all` runs once
  (guarded) at the end of boot, after the scheduler and the clocks (a
  driver may wait or start a thread) and before the shells, and is the
  only caller of `pci::claim(addr, driver) -> Result<DeviceResources,
  ClaimError>` at boot (ktests claim devices no driver takes). It offers
  each device to the matching rows in order, until one doesn't decline it.
  The claim is **once per claim**, whoever asks: the function's slot in
  the registry goes from free to probing under the registry's lock (with
  the command register to restore), and a second claim is
  `ClaimError::Taken` with the binding, which it leaves as it was; a
  function is claimed again only after its driver declined it. The claim
  maps the device's memory BARs through the registry and sets **memory
  space and bus master** in the command register, with I/O space off
  (`virtio-drivers` never sets bus master, and the firmware's state isn't
  trusted), reading the register back: bits that don't stick refuse the
  claim (`ClaimError::Command`), with the register restored first (read
  back too; not tested: QEMU keeps the bits).
- A device isn't probed when one of its memory BARs can't be mapped
  (`BarError`: the mapping failed, or the memory BAR is unassigned or
  malformed; an I/O BAR, assigned or not, unless malformed, or an
  unimplemented slot isn't mapped and doesn't stop it), its capability
  list was cut off or not read, or its command register refuses: a
  warning with the address and the reason (for a BAR: `BarError`'s text,
  with its index and, for a failed mapping, its physical range and the
  error), and `lspci` shows `<driver>: not probed: <reason>`. Before each
  probe, `probe_all` logs `probing <driver> on <addr>` (info), so a hang
  points at a device. The outcome is recorded (`pci::probed`, only from
  probing by the same driver; it writes the command register first, read
  back and warned about on a mismatch, then the binding, so no other claim
  sees the function free before its register is restored):
  - `<driver>: bound`. If the probe brought up a virtio transport,
    `probe_all` first checks that device is ready
    (`StatusHandle::check_ready`: `FEATURES_OK` and `DRIVER_OK`, neither
    `DEVICE_NEEDS_RESET` nor `FAILED`; its message names what is missing).
    The crate's `begin_init` writes `FEATURES_OK` without reading it back
    (virtio 1.2 §3.1.1 step 6) and `finish_init` checks nothing, so a
    device refusing the features would otherwise end up bound; a device
    that isn't ready makes the probe fail. Before that it checks the
    device took its MSI-X vector (`virtio::BroughtUp::check`: a problem
    the transport latched fails the probe, "Interrupts for devices:
    MSI-X", below), and a probe that failed some other way keeps that
    problem in its reason (`... (after MSI-X: ...)`). Drivers call
    `virtio::check_device` (both checks) earlier themselves, before
    waiting for an interrupt, publishing the device or starting a
    thread.
  - `<driver>: failed: <reason>`, a warning, with bus mastering turned off
    so the device can't reach memory its driver dropped. The same binding
    records a driver giving its device up after binding it:
    `pci::failed_after_bind(addr, driver, why) -> bool` (after the driver
    dropped or reset it; bus mastering off, a warning if the device's
    resources are still live; false, changing nothing, unless the function
    is bound to that driver: already failed, still probing, someone
    else's). rng (#97) uses it for a device whose request failed, the
    tablet (#98) when its device stops being ready, sound (#99) when its
    device fails while playing, or is given up.
  - Declined (`ProbeError::NotMine`: a device the row's `matches` can't
    rule out, e.g. a second tablet, rng or sound device, or another virtio
    input device with the tablet's class): the function is freed, its
    command register restored, and the next matching row is offered it;
    `lspci` shows `no driver (<driver> declined it)` if no row takes it. A
    driver may decline only before the crate's device constructor (or
    before setting up its own queues, as rng does): before
    `virtio::transport` (one tablet, one rng and one sound device), or
    after it, reading the configuration through
    `VirtioTransport::read_config_space` (virtio-input's ID_NAME, EV_BITS
    and ABS_INFO selections don't depend on the features): after the
    constructor the crate has posted buffers it never takes back
    (`VirtIOInput`: 32 bounce runs) and MSI-X is enabled (the transport
    allocates the vector lazily, at the first `queue_set`, inside the
    constructor), so a decline before it leaves nothing behind. A probe that declines but keeps its
    `DeviceResources` (the registry knows while they live) is recorded as
    failed instead, so the next row can't get a second set.
  `no driver` is shown for a function no driver took. DMA runs handed out
  during a probe attempt are recorded as that attempt's, so a failed or
  declined probe warns with the runs it left outstanding (buffers still
  posted when its queues were dropped), not counting a previous row's.
  Only runs from the probing thread or a thread made during the attempt
  (one the probe spawned) are charged to it (the attempt records its
  thread and the next thread id then), so a probe hands out DMA from
  those threads only: an older driver's thread handing out runs meanwhile
  (the tablet's re-posting its event buffers while later functions are
  probed) isn't blamed on the probe (a ktest checks both). A summary line counts the
  bound, failed, declined and refused.
- **A probe's side effects come last.** A thread started or state
  published for the rest of the kernel comes after the probe's last
  fallible check, `virtio::check_device` included (no MSI-X vector
  problem latched, and `StatusHandle::check_ready`; every driver calls it
  before publishing and spawning; a spawn that fails undoes the tablet's
  publishing), so a probe that fails its own
  checks leaves no live thread or "healthy" status behind; a driver must
  not depend on its device staying bound until `probe_all` has recorded
  the outcome (its check after the probe can still fail it). A device
  that fails after binding (the tablet's thread sees it stop being ready:
  `FAILED`, `DEVICE_NEEDS_RESET` or `DRIVER_OK` gone) is marked with
  `pci::failed_after_bind`: `lspci` shows `<driver>: failed: its status
  showed <why> after binding` and bus mastering goes off (not tested for
  the tablet: QEMU can't make the device fail; `mouse`'s text for a failed
  tablet is ktested). `check_device` failing before a driver publishes
  is tested through its vector check (`vectors=0`, and the refused-vector
  ktests' probes); its readiness check failing there, and the undoing of
  a failed spawn, are reached only by fault injection (not tested).
- **Shared errors.** `ProbeError::Unusable(String)` (a device the driver
  can't use: `<driver>: failed: unusable device: <why>`, e.g. a tablet's
  axis range, no stream or format sound can play, a virtio-rng answering
  one repeated byte) and `ProbeError::Thread(SpawnError)` (no thread for
  the driver) are shared by the M3 drivers (#97-#99).
- **A virtio driver's probe** calls `virtio::transport(res, handler)` from
  the probe itself (runs and the status handle are charged to the running
  probe attempt), may decline the device (above), then calls the crate's
  device constructor, or sets up its own queues, as rng does, calls
  `virtio::check_device` and keeps the device.
- **Blocking and locks.** A driver whose callers come from anywhere (rng)
  keeps its device in its own state (a `SpinLock` static), holds the lock
  (interrupts off) only for one operation, and waits outside it, with a
  bound, where the crate allows it; the tablet and sound give their
  device to one owner thread instead (below), which touches it with no
  lock at all (sound's commands reach its thread through a `SpinLock`ed
  request slot). Only `VirtIOInput::pop_pending_event` and sound's
  `pcm_xfer_nb`/`pcm_xfer_ok` are non-blocking. `VirtIORng::request_entropy`
  and every sound control request (`pcm_set_params`, `pcm_prepare`,
  `pcm_start`, `pcm_stop`, `pcm_release`, and `set_up`'s info queries,
  which the first `pcm_xfer_nb` also reaches) use the crate's
  `add_notify_wait_pop`, an unbounded spin. So:
  - rng (#97) drives its own `virtio_drivers::queue::VirtQueue` (public:
    `add`, `pop_used`) over `VirtioTransport`, with a bounded wait
    ("Randomness", below);
  - sound's (#99) control requests are a known unbounded wait: the stream
    is set up once in the probe, and each sound's prepare, start, stop and
    release run on the `sound` thread, outside the waits for periods
    (below, "Sound").
  `PciTransport`'s `Drop` spins until the device status reads 0, so
  `VirtioTransport`'s own `Drop` writes 0 and waits up to 100 ms for the
  status to read 0 (no early exit on the status from before the reset),
  then drops the `PciTransport`. A device that doesn't reset is logged as
  an error (naming it, the time and its status), its bus mastering turned
  off, and its `PciTransport` leaked instead of dropped, so boot doesn't
  hang in that spin (not tested: QEMU's devices reset). A bounded wait
  (`StatusHandle::wait_for(bell, timeout, ready)`, below; `wait_until`,
  spinning, only for the ktests' made-up device) checks `ready` once more at
  the deadline, gives up when the status shows `DEVICE_NEEDS_RESET` (set
  by the device) or `FAILED` (set by a driver; all ones, a device gone or
  not decoding, shows as `FAILED`), seen at the next wake or the
  deadline, and otherwise returns a
  timeout carrying the time waited and the status byte rather than
  hanging.
- **Drop order.** DMA memory must not be freed under a live device: reset
  the transport before any queue is dropped. A driver keeps its queues in
  a struct field declared after the transport (fields drop in order) or
  drops the transport first. The crate's device structs get this from
  their field order: `transport` is their first field (their own `Drop`s
  only call `queue_unset`, a no-op on PCI; `VirtIOSound` has none). A
  crate constructor that fails partway (`?`) drops the queues it made
  before the transport resets the device: allowed, because the device was
  never set `DRIVER_OK`.

### Resources: `DeviceResources` (built, #95)

A driver gets its device's resources, and nothing else:

```rust
pub struct DeviceResources { info: &'static DeviceInfo, /* private */ }

impl DeviceResources {
    pub fn info(&self) -> &'static DeviceInfo;
    pub fn bar(&self, i: usize) -> Option<BarMapping>;   // looked up in the registry
    pub fn config(&self) -> FunctionConfig<'_>;          // this function's config space only
}
```

- `info` is private (a public field could be reassigned to forge another
  device). There is no public constructor and it isn't `Clone`: only
  `pci::claim` makes one. `bar(i)` is `None` for an I/O BAR, an
  unimplemented slot, the upper half of a 64-bit BAR or `i` above 5 (a
  memory BAR the claim didn't map is a bug, a panic).
- **`FunctionConfig<'a>`** reads and writes the claimed function's config
  space, through ECAM (its lock taken for each access), as `lumen-pci`'s
  `ConfigSpace` and as `virtio-drivers`' `ConfigurationAccess` (whose
  bus/device/function has no segment: the function's own is used), and
  borrows the `DeviceResources`, so it can't outlive the claim. Another
  function is a bug, and it panics; so is a write inside the function's MSI
  or MSI-X capability (`CapList::interrupt_cap_at`, host-tested;
  `DeviceResources::msix` programs MSI-X through a path private to `pci`). Both panics are not
  tested booted. `virtio-drivers` 0.13 writes only the command register and
  the BARs, sizing them in `bar_info` during probe, before the device runs;
  full BAR emulation is out of scope for M3. Known limit: a driver could
  reach its MSI-X table through `bar(i)`, and is expected not to.
- **Ownership, not confinement.** A driver only reaches its own registers
  and config space, but DMA isn't scoped per device in M3: without an
  IOMMU a device can write any physical memory. If a driver moves out of
  the kernel, these resources become a `Device` capability's contents
  (section 8); confining DMA then also needs an IOMMU.

### virtio: the adapter for `virtio-drivers` (built, #95)

`virtio-drivers` `=0.13.0` (`default-features = false`, `features =
["alloc"]`: input and sound use `Box` and `Vec`; `embedded-io` isn't
needed) does the virtqueues and the device protocols. #95 also adds
`zerocopy` (pinned to the lockfile's version) as a direct dependency: the
crate's `read_config_space`/`write_config_space` name its traits, and it
doesn't re-export them. Lumen supplies the transport and the `Hal`.

- **One entry point.** `virtio::transport(res: DeviceResources, handler:
  fn()) -> Result<VirtioTransport, ProbeError>` takes the resources by
  value, so it runs once per claim (a function is claimed again only
  after its driver declines it), and `VirtioTransport` owns them. It is
  called from the probe itself, and runs under a probe lock, one device at
  a time. The transport calls `res.msix(handler)` itself (so a vector
  can't belong to another device), lazily ("Interrupts for devices:
  MSI-X", below); the ktests pass a handler that does nothing. One
  initialization per transport (a second `begin_init` would reset the
  device and unmap its configuration vector). It hides `RING_EVENT_IDX`
  (bit 29) from `read_device_features`, so no driver negotiates it: with
  it, `virtio-drivers` stores `used_event` and then loads `used.idx` with
  no full fence between them, and x86 may perform that load before the
  store is visible, so the device could read the old `used_event` and
  skip the interrupt while the driver, having seen no new buffer, goes to
  sleep. Without it the device interrupts for every used buffer (nothing
  calls the crate's `set_dev_notify(false)`, so `NO_INTERRUPT` stays
  clear). A ktest checks QEMU offers the bit and the transport hides it.
  - It first checks what `PciTransport::new` will look up
    (`lumen_pci::virtio`, host-tested): the first virtio capability of each
    type 1-4 (common, notify, ISR, device configuration; length field at
    least 16, the notify one at least 20) must have its offset and length
    inside a BAR mapped in the registry, the sum below 4 GiB (the crate
    adds them in a `u32`), and be at least as long as what the crate maps
    there (the common configuration's 56 bytes, a 2-byte notify register,
    the ISR byte, a device configuration dword: otherwise the crate's
    misleading "offset greater than BAR length"), and every capability the
    crate reads must have its fields inside the 256-byte header (it adds
    their offsets in a `u8`: a capability at 0xf4 or later, or a notify
    one at 0xf0 or later, would overflow): `ProbeError::Capability`
    otherwise. Other types, such as 5 (PCI configuration access, whose BAR
    is often the legacy I/O BAR), are ignored. The crate's capability walk
    stops where Lumen's masks a pointer's reserved bits and goes on, and
    only complete lists are probed, so the crate sees a prefix of Lumen's
    list and keeps the same structures (or reports one missing before
    mapping it).
  - Then it builds `PciTransport::new` over a `PciRoot` whose
    `ConfigurationAccess` is the device's `FunctionConfig` (the first 256
    bytes, enough for virtio's capabilities; the `PciRoot` is never used to
    enumerate). `PciTransport::new` reads the capabilities, sizes their BARs
    again (its `bar_info`, with decoding off) and asks the `Hal` for each
    structure's address: 3 calls, 4 with a device configuration. The modern
    virtio interface uses no I/O BAR.
  - Then it checks each queue's notify register (`queue_notify_off` times
    the multiplier) is inside the notify structure: the crate's `notify`
    indexes it with `unwrap`.
  - The result is wrapped in `VirtioTransport`, which implements the
    crate's public, unsealed `Transport` trait by delegating, so it can add
    what the crate leaves out: the checked common configuration's base
    (the MSI-X vectors are written there) and the device status handle
    derived from it (`StatusHandle`: `check_ready`, `wait_for`). The common configuration
    offsets it uses are `lumen_pci::virtio::common`.
  - **One aliasing exception.** The crate holds the common configuration
    as a `UniqueMmioPointer` (safe-mmio: nothing else may access it), and
    the `Hal` promises `mmio_phys_to_virt`'s memory isn't aliased;
    `VirtioTransport` keeps a second pointer anyway. That is benign: both
    sides make only volatile accesses through raw pointers,
    `device_status` is a plain byte register that doesn't depend on
    `queue_select`, and the wrapper writes `queue_select` only through
    `&mut self`, when the crate isn't in the middle of an access (it
    rewrites `queue_select` before each queue access of its own).
- **`Hal`.** Its methods are associated functions with no `self` and no
  device argument (`Hal::dma_alloc`, `dma_dealloc`, `mmio_phys_to_virt`,
  `share`, `unshare`), so `VirtioHal` reaches its state through statics:
  - `mmio_phys_to_virt`: `virtio::transport` sets a static "probing this
    device" scope (its address and BAR mappings) around
    `PciTransport::new`; the lookup panics outside that scope or outside
    that device's BARs, naming it (made unreachable by the check before
    it). It never maps anything.
  - **Contiguous runs** are an `OwnedRun { first, count }` (frames from
    `frame::alloc_contiguous`; `Drop` frees them). Every run handed out
    stays owned by a fixed table of outstanding runs until it comes back:
    no heap, `RUNS = 256` entries, with its own lock, taken before the
    probe owner's and the frame allocator's (a run is freed with the
    table's lock held). Each
    entry records its kind (a DMA buffer, or a bounce buffer of a given
    length) and the function whose probe handed it out. A run comes back
    only at its address with its kind and length, so the table catches a
    wrong length or kind, an address never handed out and a free of a run
    not outstanding (not a stale free after the frames were handed out
    again at the same address). 256 comes from the counts: input 36 (32
    posted events and its rings) plus sound 40 idle (rings and its event
    queue) plus rng about 3 is about 79 at boot; up to about 178 with
    sound's whole tx queue in flight (96 runs, 3 a period) and 3 for a
    control request; Lumen keeps 8 periods (24 runs): about 106.
  - `dma_alloc`: an `OwnedRun`, zeroed through the HHDM, returned as
    (physical, HHDM address); 0 pages, a full table (checked before
    allocating) or no contiguous run free returns `(0,
    NonNull::dangling())` with a warning, which the crate turns into
    `Error::DmaError` (frame 0 is reserved, so 0 is unambiguous).
    `dma_dealloc` frees only a DMA run the table recorded with that address
    and frame count; anything else returns non-zero after logging what was
    asked and what is recorded there (the crate's `Dma::drop` then panics
    on it). Every DMA frame belongs to the frame allocator (hard rule 2).
  - `share` / `unshare`: **bounce buffers, not address translation.** The
    crate passes `share` whatever buffer a request uses: rng's own request
    (a stack array: 32 bytes per `fill`), a driver's `Box` (`VirtIOInput`'s events,
    `VirtIOSound`'s `queue_buf_recv` for control replies), stack locals
    (`pcm_xfer`'s stream id and statuses), a `Vec` (`pcm_xfer_nb`), and
    heap `Box`es for indirect descriptor lists. `share` returns one
    physical address per buffer, so the device needs it physically
    contiguous, but the heap and the stacks are windows mapped on fresh
    frames one page at a time: a buffer that crosses a page there is in
    general not contiguous, and the HHDM is the only contiguous view of
    RAM. So `share` takes an `OwnedRun` of `ceil(len / 4096)` frames (one
    for an empty buffer), records it, and copies the buffer in
    (`DriverToDevice`) or zeroes it (`DeviceToDriver`, so stale memory
    never reaches the caller); `BufferDirection::Both` never reaches
    `share` (`unreachable!`, with a message). `unshare` checks the recorded
    run, copies `DeviceToDriver` data back and frees it; a run it can't
    find or free panics with the address, length and what is recorded.
    `share` has no error path: out of frames or table entries it panics,
    with the size and the runs outstanding. A virtio-rng request of any
    size (#97; `fill` asks for 32 bytes, a ktest for more than a page) goes
    to the device in buffers of at most a page, so each bounce is one
    frame.
  - The cost: an allocation and a copy per buffer, fine for rng, input and
    sound; a fast path (no bounce for a buffer within one page or in the
    HHDM) can come with networking (M7) if profiling asks for it; rng's
    early return after a wait that gave up relies on the bounce (the
    device never has the caller's buffer), so such a fast path must reset
    the device first. Each
    posted 8-byte input event holds a 4 KiB bounce frame. Buffers still
    posted when their queue is dropped are never unshared (`VirtQueue` and
    `OwningQueue` have no `Drop` that does): their frames and table entries
    leak. A probe that fails (or declines) partway leaks this way, and
    `probe_all` reports the runs that probe attempt left outstanding; so
    does a driver dropping its device with a buffer still posted (rng after
    a request that gave up, sound when its device fails while playing, the
    tablet's when its device fails or its thread can't be spawned: its up
    to 32 posted event buffers, its error line giving the DMA runs
    outstanding in all; not tested), and rng reports the runs it leaked in
    its warning and in `lspci`'s reason.
- **Waiting for the device's interrupt.** Each driver's MSI-X handler
  rings its `virtio::Doorbell` (a count of rings and a `WaitQueue`); its
  waits read the count, look at the device, and block until the count
  moves or a deadline passes (`StatusHandle::wait_for(bell, timeout,
  ready)`: the status checks on each wake, `DEVICE_NEEDS_RESET`, which a
  device announces with a configuration-change interrupt on the same
  vector, and `FAILED`, a driver's or all ones, which comes with none and
  is seen at the next wake or the deadline;
  `WaitQueue::wait_until_or_deadline` underneath), so a wait costs no CPU.
  Where the caller can't block (a lock held, interrupts off) it spins. The
  tablet's thread waits up to 1 s at a time, then looks at the status
  anyway (a device gone, reading all ones, sends nothing), and looks again
  at once after a batch of half the ring or more (held by `mouse hold`,
  it waits only for `mouse release`, and looks at nothing). No lock is held across such a wait (`SpinLock`
  keeps interrupts off, and a thread can't sleep under it): a device that
  is waited on this long has one owner thread, the only code touching it
  (the tablet's `mouse` thread, the `sound` thread), and the rest of the
  kernel reads what that thread publishes.
- **The tablet (#98).** virtio's keyboard and tablet share their PCI id
  (1af4:1052, virtio input); QEMU gives them different class codes
  (keyboard 09/00, tablet 09/80, a relative mouse 09/02), so the tablet's
  row `matches` class 09/80 only and the keyboard is never offered to it.
  Other 09/80 devices (multitouch, a passed-through host device) are
  declined before the crate's constructor: the probe reads `EV_BITS` for
  `EV_ABS` through the raw transport and needs `ABS_X` and `ABS_Y`, then
  reads both axes' `ABS_INFO` ranges (and `ID_NAME`, for the log) before
  `VirtIOInput::new` posts its 32 event buffers (a scenario attaches
  QEMU's multitouch panel, 09/80 without `ABS_X`/`ABS_Y`, and finds it
  declined). Only one tablet is used: a second is declined before its
  transport is set up.

### Sound: virtio-sound (built, #99)

- **Probe.** The `sound` row takes virtio device type 25, one device:
  the player is one (one slot, one thread, one format), so a second sound
  function is declined (`ProbeError::NotMine`) before its transport is
  touched, unless the first one's probe failed. After the crate's
  `VirtIOSound::new` it checks the device is ready, then (control
  requests: the known unbounded spins) picks the first output stream and
  a format: signed 16-bit samples (what `lumen-synth` makes), stereo if
  the stream takes 2 channels, else mono, and 44.1 kHz (the default rate
  of QEMU's audiodevs: no resampling there), else 48 kHz, else a lower
  rate, else a higher one. A stream without one of those fails the probe
  (`ProbeError::Unusable`: `unusable device: its first output stream
  lacks ...`), as does a request the device refuses (naming it). It sets
  the stream's parameters (20 ms periods, a buffer of 8), checks the
  device is still ready, and only then starts the `sound` thread (a
  failure there is `ProbeError::Thread`) and publishes the device: the
  thread owns the `VirtIOSound` from then on, and no other code touches
  it, so no lock is held across a device wait. Only the first output
  stream is considered.
- **Requests.** `sound::play(Box<dyn Source + Send>) -> Result<Sound,
  SoundError>` checks the device (its state, and while idle its status
  register) and the source's rate (it must be the device's:
  `SoundError::WrongRate`) and puts the sound in the player's `next` in
  one critical section, wakes the thread (a `WaitQueue`) and returns a
  `Sound` (a private id and a cell its `Outcome` lands in: `Played`,
  `Stopped`, `Replaced` or `Failed(why)`, so the outcome is never lost to
  later sounds). `sound`'s state is `playing` from then on, before the
  thread has taken the sound (#120: `beep` returns first, and the next
  command may run before the thread does), until it is over. A newer sound cuts the playing one off at its next
  period (up to 160 ms already queued still plays; a sound that has made
  all its samples plays out); a sound still waiting is dropped.
  `sound::stop(&sound)` fades a sound out (the periods already queued
  still play) or keeps it from starting. `beep` returns at once; `play`
  checks its sound's outcome and the keys every 20 ms (`q` or Ctrl-C
  stops it; other keys are discarded) and says `play: stopped`, `play:
  interrupted by a newer sound` or why the device failed. Sounds are made
  at the device's rate (`sound::format()`) and take the volume when they
  are made.
- **Paced by the device.** For each sound the thread checks the device is
  ready, prepares and starts the stream, keeps up to 8 periods queued
  (`pcm_xfer_nb`) and queues another each time the device returns one
  (`pcm_xfer_ok` takes it back; QEMU returns a period once its audio
  backend has consumed it), so the sound plays without gaps as long as
  the thread is back within 160 ms. Between looks at the oldest period it
  blocks until the device's interrupt (`wait_for`, at most 2 s a period,
  ending on `DEVICE_NEEDS_RESET` or `FAILED`). When the source has ended (its last
  period padded with silence) and the queue has drained, it stops and
  releases the stream. A queue found empty before the end counts as an
  underrun (a warning per sound; `sound` counts them).
- **Failure.** The thread gives the device up (drops it, resetting it;
  runs still posted leak; then `pci::failed_after_bind`, so `lspci` shows
  `sound: failed: <why>`) when the device isn't ready before a sound
  (`checking the device before a sound: status ...`), a wait gives up, a
  request the device refuses (named in the reason), or a sound of 1 s or
  more whose periods all came back in under half its length (the crate's
  `pcm_xfer_ok` doesn't read a period's status reply, so a refused period
  counts as played: this catches a device refusing them all). `sound`
  then shows `failed: <reason>`, and `beep` and `play` say the device
  failed. A control request around a sound (prepare, start, stop,
  release) that never completes spins forever (the crate's limit) and
  wedges the thread: the first look (`sound`, `beep`, `play`, or `play`
  polling `format()`) over 5 s after the sound started or the device last
  answered gives the player up for good (nothing looks on a timer) (a warning; the reason gives the
  device's status if it shows `DEVICE_NEEDS_RESET` or `FAILED`, else `no
  answer from the device for N s`; `lspci` shows it failed), `play` stops
  its sound and returns, and a thread that comes back drops the device
  (a later error of its own is warned about; the first reason stays). A
  device whose status shows it broken while idle is given up by the next
  sound or `sound`. Whichever way the player is given up, the sound
  waiting to start settles `Failed` with the reason (`Bound::give_up`
  takes it); until a stuck thread's look, the waiting sound is unsettled
  too. The sound playing settles when the thread next gets an answer, so
  on a thread stuck for good it stays unsettled (callers poll `format()`
  too). A sound the driver drops without settling (a bug) settles
  `Outcome::Lost` with an error logged, and `play` says `play: the sound
  was lost by the driver (a bug)`, not that the device failed. Nothing
  re-initialises the device. Without a sound
  device they say `no sound device` (with the function's binding if one
  was found but not bound). Underrun and overrun events on the device's
  event queue aren't read. The crate itself warns, on every boot with a
  sound device, that it got no jack and no channel map infos (QEMU's
  device has none); left as it is.

### Randomness (#97)

`rng::fill(&mut [u8]) -> Result<(), NoSource>`, `rng::u64()` and
`rng::below(n)` (unbiased: `lumen_rand::below`) hand out bytes from a
ChaCha20 pool (`lumen-rand`, host-tested against RFC 8439's vectors and a
known answer of its own) fed by two sources:

- **RDRAND**, when CPUID leaf 1 says so (QEMU's default `qemu64` has none;
  `-cpu max` has it). The instruction is in `arch::x86_64::cpu` behind the
  HAL boundary: `Rdrand::detect()` gives a token (found once, at `init`),
  and only the token runs it (`try_once`). Each 64-bit draw is tried up to
  10 times (`lumen_rand::retry`); a draw that still fails leaves RDRAND out
  of that request only (counted, warned about at the 1st, 1024th, 2048th...:
  `lumen_rand::should_warn`). At boot it draws a 64-byte seed: if this
  CPU's RDRAND gave one value for all 8 words it is never used (not checked
  per CPU); if a draw failed the seed is tried again on the next request
  (`RDRAND seeded on retry` when it works); otherwise the seed is mixed in
  and RDRAND is in use from then on, never before it seeded the pool. A
  later draw of one value for all 4 words marks it stuck too.
- **virtio-rng** (`DRIVERS` row `virtio-rng`, virtio device type 4; one
  device: a second is declined, `ProbeError::NotMine`, before its
  transport is touched): negotiates only `VERSION_1` (no indirect
  descriptors, no event index), one 8-entry queue, and drives it itself:
  `add`, notify, a bounded wait for its interrupt (`StatusHandle::wait_for`, ended early by
  `DEVICE_NEEDS_RESET`/`FAILED`), `pop_used`. A request has 1 s in all,
  however many buffers it takes (none is posted after it): at most a page
  per buffer, a short answer asked again for the rest; 8 empty answers in a
  row or a length past the buffer is an error. The probe makes one request,
  the pool's first seed, and publishes the device only after it: a request
  that fails fails the probe (`lspci`: `virtio-rng: failed: ...`), and so
  does an answer of 32 bytes of one value (`ProbeError::Unusable`, the
  variant #98 and #99 share for a device a driver can't use, with the
  reason; so are the empty and too-long answers). Later, an answer of one
  repeated byte isn't mixed in (counted, warned about rate-limited), and 8
  in a row give the device up.
- **A failed request consumes the device** (`Device::request` takes it by
  value and gives it back only on success), and so do 8 repeated-byte
  answers in a row. It is dropped: its transport resets it, and the bounce
  frame of a buffer it never answered, after a wait that gave up, stays
  outstanding, leaked (counted: the warning and `lspci`'s reason say how
  many). `pci::failed_after_bind` records it, so `lspci` shows
  `virtio-rng: failed: <reason>` and bus mastering is off. It is never
  reaped or re-initialized; the pool goes on with RDRAND, if any (the
  warning says which source carries on, or that none does).

Each `fill` draws 32 bytes from every source in use, mixes them in, and
hands out a stream keyed from the pool by fast key erasure (one block gives
the stream's key and the pool's next key). **Once seeded, the pool always
answers**, even after every source is lost (a cryptographic generator:
unpredictable without its key; the warning that drops the last source says
so). Only a pool never seeded refuses (`NoSource`: "no randomness source has
answered", saying RDRAND's state and whether a virtio-rng device failed, and
naming the QEMU options that add a source only when there is neither); boot
says so as info, not a warning, because QEMU's default machine has
neither. **Concurrent callers:** the pool's `SpinLock` is held only to mix
and split; the virtio device sits in an `Option` behind its own
`SpinLock`, taken out for a request made with no lock held (the wait
blocks until the device's interrupt, or spins where the caller can't
block; a `SpinLock` keeps interrupts off) and put back after. A caller
finding it taken skips it for that call (counted): the pool was seeded at
probe, so its output stays unpredictable, and RDRAND still adds fresh input
when in use. The shell's `random [bytes]` prints hex, `random --stats` the
sources, RDRAND's state (absent, pending, in use, stuck) and the counters;
rhai has `random()` and `random(n)`. `make run` attaches virtio-rng.

Not tested on their own: RDRAND failing or stuck in the kernel (the retry
limit, the seed tried again on the next request, the stuck checks at boot
and per request; `retry`, `fill_words` and `all_same` run only against
fakes, on the host), a device that needs a reset, answers empty or too
long, or doesn't answer the probe's request (QEMU's devices answer; tested:
a device too slow to answer later, with QEMU's rate limit, and one
answering zeros at the probe and later, backed by a file), the deadline
cutting a wait short (a ktest checks only that nothing is posted once it
has passed), and two callers really racing for the device (the busy path
is tested by taking the device).

### Interrupts for devices: MSI-X (built, #96)

- **Vectors** `idt::MSI_BASE..=MSI_LAST` (0x90-0xef, 96; section 6) come
  from `idt::claim_msi_vector(handler)`, lowest free first, lock-free (one
  compare-and-swap per slot), with its own handler table
  (`set_irq_handler` stays I/O APIC only); never freed in M3. The MSI arm
  of `lumen_exception_dispatch` runs the vector's handler, then EOIs at the
  local APIC; one nothing claimed is unexpected (an error).
- **`DeviceResources::msix(&mut self, handler: fn()) -> Result<MsiVector,
  MsiError>`** (`pci/msix.rs`, inside `pci` for its private accessors)
  follows `Route`'s pattern: once per function (a second call is
  `MsiError::AlreadyAllocated`: the registry slot records the claim as
  soon as an IDT vector is claimed, before any write to the device, so a
  failed try counts too; a driver that declines the function after
  `msix` is recorded as failed, since MSI-X stays enabled on it). It takes the MSI-X capability enumeration found
  (`caps.find(id::MSIX)`, never an offset a caller gives), reads where the
  table is (`lumen_pci::msix`, host-tested: BAR indicator, offset, size,
  the entry and Message Control encodings) and reaches it through the
  claimed BAR's mapping in the registry (no new `map_mmio`; a table outside
  a mapped memory BAR is `MsiError::Table`). It claims an IDT vector with
  the handler, programs table entry 0 **masked** (Vector Control first,
  then the message: fixed delivery, physical destination, the boot CPU's
  local APIC id in the address, edge triggered, the vector in the data),
  sets MSI-X Enable with Function Mask clear in Message Control (read,
  modify and write of the capability's first dword, through a helper
  private to `pci`: its id and next-pointer bytes are read-only, unlike
  the status register beside the command register), reading it back
  (`MsiError::NotEnabled`), and sets INTx-disable in the command register.
  Other errors: no capability (or not MSI-X), a reserved BAR indicator,
  every vector taken, the boot CPU offline or its APIC id too large for a
  message (x2APIC ids past 255). Only `MsiVector::unmask` unmasks the
  entry. `MsiVector` has private fields `{ pci, entry, vector, at }` (`at`:
  where the entry is mapped, so unmasking takes no lock) with
  `table_entry()` and `idt_vector()`, and is neither `Copy` nor `Clone`:
  one owner, the transport. Listings (`irqs`) get a read-only
  `MsixRecord { pci, table_entry, idt_vector }` from
  `pci::msix_vectors()`. One vector per device (its configuration changes and all its
  queues), to the boot CPU. A probe that fails after `msix` keeps its
  vector and handler; its bus mastering is off, so it can't send the
  message (a memory write).
- **virtio's vectors.** `virtio-drivers` 0.13 never writes the common
  configuration's `msix_config` (the spec's `config_msix_vector`) or
  `queue_msix_vector`: its `CommonCfg` declares them, but
  `PciTransport::queue_set` writes only the queue's size, addresses and
  enable, and the struct is crate-private. The virtio 1.2 specification:
  the values are MSI-X table entry numbers, `NO_VECTOR` (0xffff) for none
  (§4.1.5.1.2); a reset unmaps them, and a mapping the device can't make
  reads back as `NO_VECTOR` (§4.1.5.1.2.1); the driver MUST read them back
  (§4.1.5.1.2.2); with `NO_VECTOR` no interrupt is sent for the queue or
  for configuration changes (§4.1.5.3.1, §4.1.5.4.1); a queue's vector is
  set before `queue_enable` (§4.1.4.3.2). With MSI-X enabled, INTx is
  disabled (PCI specification). So `VirtioTransport::queue_set`, before
  delegating: the first time, calls `res.msix(handler)` (after
  `begin_init`'s reset), writes the table entry index (0) to `msix_config`
  and reads it back, and unmasks the entry; each time, selects the queue
  and writes the entry to `queue_msix_vector`, reading it back, before the
  crate's `queue_enable`. A refusal (`NO_VECTOR` read back) or an
  `MsiError` is latched (a warning naming the device, and the queue or
  configuration changes): `queue_set` returns `()`, so the error goes to
  the running probe attempt (`virtio::check_device`, which each driver
  calls after the crate's constructor, before it waits for an interrupt,
  publishes the device or starts a thread; `probe_all` checks it again),
  and the **probe fails** (`ProbeError::Vector(VectorError)`: `Msi(MsiError)`
  or `Refused(VectorFor::{Config, Queue(n)})`; `lspci` shows `failed:
  <reason>`): no polling fallback. A virtio-rng without an MSI-X
  capability (QEMU's `vectors=0`) fails this way, and so does a device
  that reads back `NO_VECTOR`, for configuration changes or a queue (a
  ktest maps an entry past the table, which QEMU refuses).
- **Aliasing.** The wrapper's writes go through its second pointer to the
  common configuration: the adapter's aliasing exception (above), whose
  argument covers `msix_config` and `queue_msix_vector` too (written
  through `&mut self`, `queue_select` rewritten by the inner transport
  before each queue access of its own).
- **Vector lifetime.** The vector is allocated lazily, at the first
  `queue_set` (inside the crate's device constructor, or rng's own queue
  setup), so a driver that declines a device before the constructor
  (above) leaves no vector behind.
- **Drivers wait for it** through their `Doorbell` (above, "Waiting for
  the device's interrupt"); `irqs` lists each device's vector as `msix
  <pci address> #<entry>` with its count. A wait that blocked to its
  deadline with no ring, after which the device turned out to have
  answered with the ring count still unchanged (`virtio::MissedRule`, one
  rule for `wait_for` and the tablet's loop: a ring in between is an
  interrupt that came late, not a miss), counts a missed interrupt,
  warned about once per device (`<addr>: the device answered but its
  MSI-X interrupt never came (said once)`, ktested), so a dead interrupt
  path doesn't hide as slow polling. A vector the device accepts must
  also read back as written, and its entry must read unmasked after the
  unmask: otherwise the probe fails too (not tested: QEMU echoes the
  entry or `NO_VECTOR`, and honours the mask).

### Device classes (later)

`BlockDevice` (M6), `NetDevice` (send/receive frames, M7), `InputDevice`
(event stream: the keyboard and the tablet), `Display` (framebuffer +
flush rectangles), `Console`: traits whose methods are the messages above,
added when a class has a second implementation and a consumer that needs
the common interface (`InputDevice`: M4's desktop, below, designed in
#146 and built in #151). `Display` stays later: M4 has one display (the
GOP framebuffer Limine hands over) and one consumer at a time (the
terminals or the compositor, section 11), so it becomes a trait with a
second display (virtio-gpu, or real hardware in M12). The shape, for
the block device:

```rust
pub trait BlockDevice: Send + Sync {
    fn info(&self) -> BlockInfo;                       // sector size, count, read-only?
    fn submit(&self, req: BlockRequest) -> Ticket;     // non-blocking
    fn poll(&self, t: Ticket) -> Poll<BlockResult>;    // or a completion queue
}
pub struct BlockRequest { pub op: BlockOp, pub lba: u64, pub buf: DmaBuffer }
```

**`InputDevice`** (its messages defined with #98, the tablet; the PS/2
keyboard is the first implementation; the trait, below, is designed for
M4 in #146 and built in #151). An input
device is a stream of messages, each one whole: nothing half applied is
ever seen.

- **Pointer:** `lumen_input::PointerState { x, y, buttons, wheel, hwheel,
  presses, seq }`, one per device frame (evdev's events up to
  `SYN_REPORT`, applied together by `lumen_input::Tablet`; a
  `SYN_DROPPED` discards its whole frame, up to the next `SYN_REPORT`).
  Positions are screen pixels, already scaled from the device's axes
  (`AbsAxis::scale`, rounded, so a pixel a host aimed at arrives exactly
  with QEMU's 0-32767 axes, on screens up to 32768 pixels); wheels count
  clicks so far, each button its presses so far, and `seq` the frames so
  far, so a consumer that only sees the newest state still sees a click
  (or a wheel turn) that came and went between two looks, while no frame
  was dropped at the device: it sees that a click happened, not where.
- **Overruns:** the tablet's ring holds 32 event buffers, and QEMU drops a
  whole frame that doesn't fit, with no `SYN_DROPPED`. A frame holds at
  most one event per code the device advertises in its `EV_BITS`, and the
  sync (`lumen_input::frame_bound`; 12 on QEMU's tablet; the boot line
  says it). A batch that read enough events that such a frame may not
  have fit (21 or more of 32 on QEMU's tablet) counts as an overrun and
  marks the state stale until a clean batch brings a frame. What that
  count guarantees, and its preconditions, are stated once, in
  `lumen_input::may_have_lost_frames`'s doc, and a host model checks them
  (#124). A scenario per frame size (moves, and 7-event frames) holds
  the `mouse` thread (`mouse hold`), sends frames until 5 past the ring's
  room, and releases it: QEMU dropped each frame past the room (traced at
  its send), the thread read exactly the frames that fit in one batch,
  one overrun was counted, and the warning names that batch and the hold;
  a second held overflow is counted but not warned about. The warning is
  said once per boot, so a held overflow uses it up (a later real overrun
  is only counted). The 7-event frames tell the device's bound from
  moves' 3. (Before #142 the scenario waited for a busy guest to fall
  behind: the `mouse` thread got the CPU back once per 10 ms slice, and on
  KVM runners QMP's roughly 0.7 to 3 ms per frame sometimes didn't
  outpace it, so the ring never filled.)
- **Keys:** the keyboard's decoded presses (`crate::keyboard::Key`: a
  character, or a raw code with its modifiers), routed into `crate::vt`'s
  per-terminal rings; each screen shell turns them into the line editor's
  `lumen_shell::Key` (`shell::screen_key`). No releases and no modifiers
  on their own there: not a desktop's key event, hence `KeyEvent` below
  (#151).
- **Delivery (M3):** the device's owner thread reads a batch of events each
  time its interrupt wakes it (or its 1 s status look; not while held)
  and publishes the newest state with the batch's counts (`mouse::
  status`: one snapshot under one lock), overwriting the last; keys are
  taken in order from their ring. In M3 the tablet's state has two
  readers (the cursor and `mouse`) and the keys one consumer (the shown
  terminal's shell). M4's desktop is the first consumer of both, and
  needs every pointer frame with its position (a click's place, a drag)
  and real key events, hence the trait.

**The trait (M4: designed in #146, built in #151; the desktop that
reads it comes with #153).** Each input device
keeps a ring of its newest events, numbered in order from 1, and a
reader asks for the first event after the last one it saw. In
`lumen-input` (host-tested), which gains the workspace's `pc-keyboard`
(`=0.9.0`) for `KeyCode`:

```rust
/// An event's number on its device: 1 for the first, never reused in a
/// boot. `Seq::ZERO` is "before the first event".
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Seq(u64);
impl Seq {
    pub const ZERO: Seq;
    pub fn get(self) -> u64;
}

pub trait InputDevice: Sync {
    /// What this device's ring holds: `PointerEvent` or `KeyEvent`.
    type Event: Copy;
    /// The first event still in the ring numbered after `after`, with its
    /// number; `None` if there is none yet (also for an `after` past
    /// `newest()`). A number past `after + 1` means the events in between
    /// were overwritten before this reader got to them.
    fn next(&self, after: Seq) -> Option<(Seq, Self::Event)>;
    /// The newest event's number (`Seq::ZERO` before the first).
    fn newest(&self) -> Seq;
}

/// One reader's place in one device's ring (the kernel's devices are
/// statics, hence `'static`).
pub struct Reader<D: InputDevice + 'static> { device: &'static D, after: Seq, lost: u64 }
impl<D: InputDevice + 'static> Reader<D> {
    /// Starting after the device's newest event.
    pub fn from_now(device: &'static D) -> Reader<D>;
    /// The next event with its number and the gap just before it (also
    /// counted in `lost`); `None` once caught up.
    pub fn read(&mut self) -> Option<Read<D::Event>>;
    /// Skip to after `seq`, never backwards and never past the device's
    /// newest: the desktop's cut (section 11). A number past this ring's
    /// newest (one from the other ring, say) is a bug a `debug_assert`
    /// catches (every profile keeps them); a smaller foreign number isn't
    /// detected.
    pub fn skip_to(&mut self, seq: Seq);
    pub fn after(&self) -> Seq;
    pub fn lost(&self) -> u64;
}
/// One event a reader got.
pub struct Read<E> { pub seq: Seq, pub lost_before: u64, pub event: E }

/// A tablet frame, and whether frames may have been lost at the device
/// before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerEvent { pub state: PointerState, pub after_overrun: bool }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub code: pc_keyboard::KeyCode, // the physical key
    pub action: KeyAction,
    pub mods: Mods,                 // after this event
    pub after_drop: bool,           // the keyboard's queue dropped bytes before it
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    /// `text`: `pc-keyboard`'s `DecodedKey::Unicode` for this press, if
    /// it made one (control characters included: Enter is '\n', Ctrl+C
    /// U+0003).
    Press { text: Option<char> },
    Release,
}
/// The modifier state (`pc_keyboard::Modifiers` isn't `Copy` in 0.9.0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Mods(u8);
impl Mods {
    pub const NONE: Mods;
    pub const fn is_shifted(self) -> bool;
    pub const fn is_ctrl(self) -> bool;
    pub const fn is_alt(self) -> bool;   // either Alt
    pub const fn is_altgr(self) -> bool; // right Alt, or Ctrl with left Alt
    pub const fn capslock(self) -> bool;
    pub const fn numlock(self) -> bool;
    // Builders, for states made without pc-keyboard (tests, other
    // crates): left Shift, left Ctrl, left Alt held, Num Lock on.
    pub const fn with_shift(self) -> Mods;
    pub const fn with_ctrl(self) -> Mods;
    pub const fn with_alt(self) -> Mods;
    pub const fn with_numlock(self) -> Mods;
}
impl From<&pc_keyboard::Modifiers> for Mods { /* ... */ }

/// Events lost just before one, other than a gap in the ring's numbers:
/// `PointerEvent::after_overrun`, `KeyEvent::after_drop`. `Read::gap()`
/// is `lost_before > 0 || event.follows_loss()`: the one check a reader
/// needs (derived, not stored).
pub trait MarksLoss { fn follows_loss(&self) -> bool; }

/// The compositor's merge of both rings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent { Key(KeyEvent), Pointer(PointerEvent) }
```

- **The ring:** `Ring<E, N>` in `lumen-input`, a fixed array of `N`
  events and the next number; `push` overwrites the oldest and returns
  the `Seq` it gave. In the kernel each ring is a static behind one
  `SpinLock`, a leaf: nothing is taken under it and nothing allocates, so
  it may be taken under any other lock (`crate::input::LockedRing`, the
  `InputDevice`; `input::POINTER` and `input::KEYS`). Reading takes
  nothing out, so several `Reader`s can read one ring. Once the desktop
  exists (#153), the `mouse` thread calls `desktop::wake()` (section 11)
  after releasing the ring's lock; the key ring needs none (IRQ 1 wakes
  the compositor, which runs the decoding pass itself). Until then no
  `Reader` follows them (only the shell's `events` commands look
  at the newest).
- **Two kinds of loss, kept apart.** A gap in `Seq` means only that
  *this reader* fell behind the ring (counted in `Reader::lost`). Frames
  QEMU dropped at the device never reach the ring and leave no gap:
  `Tablet` numbers only the frames it reads (`PointerState::seq`, and the
  `presses`). Device loss shows only in the overrun count and `stale`
  (`lumen_input::may_have_lost_frames`, above) and, on the ring, in
  `after_overrun`: set on every frame of a batch that counted as an
  overrun except its first, and on the first frame of the next batch
  (virtio re-posts each buffer as it is popped, so a frame can be dropped
  while the batch is still being read).
- **Pointer events:** the `mouse` thread pushes each whole frame as one
  `PointerEvent`, in the device's order, before publishing the newest
  state as today (so a state `mouse` shows is already in the ring). A
  press and a release sent by separate QMP `input-send-event` calls are
  separate frames (QEMU syncs after each call), so two events, each at its
  own position. A ring of 256 (QMP sends a frame every 0.7 to 3 ms, so a
  burst fills it in under a second; the compositor reads each time it
  wakes). A frame the device says it lost (`SYN_DROPPED`) isn't marked:
  QEMU never sends one. When the tablet fails, a last frame at the last
  position with every button released, marked `after_overrun`, ends a
  reader's drag.
- **Key events:** the keyboard's decoding pass (under `crate::vt`'s router
  lock, run by whichever reader wakes first, as today) pushes a `KeyEvent`
  for every key event `pc-keyboard` decodes, presses and releases, bare
  modifiers included, then routes presses as `crate::keyboard::Key` to the
  terminals as today (while the desktop isn't shown: section 11, "Input on
  the desktop"). `mods` is the state after the event: Shift+a is press
  `LShift` (shifted), press `A` (shifted, text `A`), release `A`, release
  `LShift` (not shifted). AltGr only changes the characters a layout
  makes. pc-keyboard's pseudo-keys: `RAlt2` (the fake shift `E0 2A`/`E0
  AA` QEMU sends around Print Screen) is not pushed, and the other fake
  shift (`E0 36`) doesn't decode at all (counted as undecoded, as today);
  Pause (`E1 1D 45 E1 9D C5`; nothing when let go) is pushed as one press
  of `KeyCode::PauseBreak` and its release at once (so no reader thinks it
  held), its pieces (`RControl2`, that `NumpadLock` press and both
  releases) not pushed; Ctrl+Pause sends `E0 46` (`E0 C6` released),
  which `pc-keyboard` 0.9.0 doesn't map: every `E0` goes to `pc-keyboard`
  as it comes, and its refusal of a `46` right after one is taken as Pause
  instead (the same press and release; `E0 C6` swallowed; the terminals
  get a raw `PauseBreak`, which no shell takes, where it used to be counted
  undecoded); a `KeyState::SingleShot` key would be pushed as a press then
  a release (unreachable with set 1, which makes none). The decoding is
  `lumen_input::KeyDecoder` (host-tested): one key is one press, as
  `kbd`'s `presses` counts it, wherever `pc-keyboard` refuses no byte (a
  host test feeds both one corpus; the one exception, a Pause cut off after
  its `E1 1D`, is listed there). Typematic repeat shows up as repeated
  presses with no release between. The first key event decoded from
  bytes queued after the keyboard's queue dropped one (full: the byte
  coming in is the one dropped) is marked `after_drop`
  (`lumen_input::AfterDrop`): a consumer-side count of the bytes still
  queued when the drop count moved, so the mark may be late (a stray
  release), never early (a key left held); a reader treats it as a gap
  (the desktop interrupts with `KeyGap`). A ring of 128 key events, the
  same size as a terminal's (but read, not taken: a full ring overwrites
  its oldest).
- **One mapping to the line editor:** `lumen_shell::Key::from_press(text:
  Option<char>, code: Option<KeyCode>, mods: Mods) -> Option<Key>`
  (host-tested; `lumen-shell` gains `lumen-input`), moved out of
  `shell::screen_key`: from the text if the press has one (Enter,
  Backspace, Delete, Tab, Ctrl with a letter, printable characters; other
  control characters nothing), else from the code (the arrows, Home, End).
  Text mode calls it from `crate::keyboard::Key` (`Char(c)` as `(Some(c),
  None, ..)`, `Raw { code, mods }` as `(None, Some(code), mods)`: a raw
  key carries the modifiers its key event does), and terminal windows
  (with #155) from a `KeyEvent` press's text, code and modifiers, so they
  need no second decoder.
  Toolkit text fields (#156) insert only a press's non-control `text`.
- **Order across devices:** the compositor merges the two rings in the
  order it reads them (each pass: the pointer frames read, then the key
  events). Events carry no time stamps in M4, so a key press and a pointer
  frame that arrive close together (within one compositor pass) may be
  handled in either order: a key typed just before a click that moves the
  focus may reach the newly focused window.
- **Shell (#151):** `mouse` and `kbd` show each ring's newest `Seq`
  (`newest event N` in `mouse`'s header, `newest key event N` at the end
  of `kbd`'s line), and
  `mouse events [n]` and `kbd events [n]` print the ring's newest `n`
  events (default 16) as records (`seq`, then `x`, `y`, `buttons`,
  `after_overrun`; or `code`, `action`, `text`, `mods`, `after_drop`);
  without the device they say what `mouse` or `kbd` says, and with no
  screen shell running (they run the decoding pass) `kbd events` says so
  above its table. (#149's confirming
  steps, `lumen_harness.gui`, don't need the rings: a pointer step waits
  for exactly its frame in `mouse`'s count and for the `mouse` thread's
  state to show its effect (the position, or the button held or
  released); a key waits for exactly its `presses` and `bytes` in `kbd`,
  as the keyboard's section above says.)
- **Text mode is unchanged:** the cursor follows the newest state
  (`term::move_pointer`), keys reach the shown terminal's shell through
  `crate::vt`; no `Reader` follows the rings (they fill and wrap): only the
  shell's `mouse events` and `kbd events` look at the newest.

**virtio first** (rng, input, sound in M3; blk, 9p in M6; net in M7) under
QEMU; real-hardware drivers (NVMe, AHCI, xHCI HID, a common NIC) in M12
behind the same traits.

## 8. Processes, syscalls and capabilities (planned, M9)

Until M9, apps are WASM modules running in the kernel (M5), and the
capabilities below are enforced at the WASM host interface: an app holds
only the handles the kernel passes it. M9 adds ring 3 and the syscall ABI
below for native programs.

### Capabilities

A **capability** is an unforgeable reference to a kernel object plus a set of
rights. Processes refer to them by **handle** (a `u32` index into the
process's handle table), like file descriptors but for everything.

| Kind | Object | Example rights |
|---|---|---|
| `Dir` | a directory subtree (cannot name `..` above its root) | read, write, create, delete, snapshot |
| `File` | an open file | read, write, map, exec |
| `Channel` | one end of an IPC channel | send, recv, transfer |
| `Net` | network endpoint filter (`host:port` pattern, protocol) | connect, listen |
| `AppActions` | a set of automation actions in one app (section 11) | invoke, observe |
| `Memory` | a memory object | map, write |
| `Process` / `Thread` | a child | kill, inspect, debug |
| `Snapshot` | a filesystem snapshot | browse, restore |
| `Trace` | the trace ring buffer (X-ray) | read |
| `Device` | device resources (for drivers in user space; section 7's `DeviceResources`) | mmio, irq, dma (DMA confined to the device's buffers only with an IOMMU) |

Rules: rights can only be **narrowed** when deriving (`cap_derive`), a
capability is **transferred** only through channels and only with the
`transfer` right, and there is **no ambient authority**: `open` takes a
`Dir` handle, `connect` takes a `Net` handle, and so on. A process starts with
exactly the handles its parent passes in. Every capability operation by an
agent process is appended to the audit log (section 13).

### Syscall ABI (x86-64)

- Entry: `syscall`. Number in `rax`; arguments in `rdi, rsi, rdx, r10, r8, r9`
  (Linux-like register order, our own numbers).
- Return: `rax` = result (≥ 0) or a negative `Error` code; `rdx` = second
  value when needed.
- Every syscall that touches an object takes a handle as its first argument and
  checks rights before doing anything.

| # | Call | Notes |
|---|---|---|
| 0 | `handle_close(h)` | |
| 1 | `cap_derive(h, rights_mask) -> h2` | narrow only |
| 2 | `cap_info(h, *out)` | kind and rights, for UIs and debugging |
| 10 | `chan_create() -> (h_a, h_b)` | |
| 11 | `chan_send(h, *msg, len, *handles, n)` | moves handles |
| 12 | `chan_recv(h, *buf, len, *handles, n, timeout)` | |
| 20 | `vm_map(h_mem, addr_hint, len, prot) -> addr` | |
| 21 | `vm_unmap(addr, len)` | |
| 30 | `proc_spawn(h_image, *handles, n) -> h_proc` | child gets only these |
| 31 | `proc_exit(code)` / 32 `proc_wait(h_proc)` | |
| 40 | `fs_open(h_dir, *path, len, flags) -> h_file` | path relative to the `Dir` |
| 41 | `fs_read(h, *buf, len)` / 42 `fs_write` / 43 `fs_stat` | |
| 44 | `fs_snapshot(h_dir, *label) -> h_snap` | section 9 |
| 50 | `net_socket(h_net, kind) -> h_sock` | `Net` capability filters peers |
| 60 | `trace_read(h_trace, *buf, len)` | X-ray |
| 70 | `clock_now()` / 71 `sleep(ns)` | |
| 99 | `debug_log(*str, len)` | to the kernel logger, tagged with the pid |

Numbers are reserved in ranges per subsystem so parallel work does not
collide. A user runtime crate (`lumen-rt`) wraps these. A minimal libc is
optional and comes later, only if ports need it.

## 9. Storage: VFS, LumenFS and snapshots (planned: VFS and RAMFS M6, LumenFS M10)

- **VFS:** mount table plus per-process `Dir` capabilities (there is no global
  root namespace in a process; the shell receives a home `Dir`). The trait
  includes snapshot hooks from day one:

```rust
pub trait FileSystem: Send + Sync {
    fn root(&self) -> NodeId;
    fn lookup(&self, dir: NodeId, name: &str) -> Result<NodeId>;
    fn read(&self, n: NodeId, off: u64, buf: &mut [u8]) -> Result<usize>;
    fn write(&self, n: NodeId, off: u64, buf: &[u8]) -> Result<usize>;
    fn create(&self, dir: NodeId, name: &str, kind: NodeKind) -> Result<NodeId>;
    fn remove(&self, dir: NodeId, name: &str) -> Result<()>;
    fn rename(&self, from: (NodeId, &str), to: (NodeId, &str)) -> Result<()>;
    // Snapshot hooks (filesystems without CoW return Unsupported):
    fn snapshot_create(&self, label: &str) -> Result<SnapshotId>;
    fn snapshot_list(&self) -> Result<Vec<SnapshotInfo>>;
    fn snapshot_open(&self, id: SnapshotId) -> Result<Box<dyn FileSystem>>; // read-only view
    fn snapshot_restore(&self, id: SnapshotId) -> Result<()>;
    fn sync(&self) -> Result<()>;
}
```

- **RAMFS** for early boot and tests; **LumenFS** as the main disk format.
- **LumenFS (copy-on-write, journaled):** blocks are never overwritten in
  place. A write produces new blocks up to a new root, which is committed
  atomically by writing one of two alternating superblocks (with checksums and
  a generation counter). A **snapshot is a retained old root**, so taking one is
  O(1). Freed blocks are tracked with reference counts per snapshot. A small
  intent journal covers metadata updates between commits. v1's SFS on-disk
  layout (`~/src/build-your-own-os`) is a reference for directory entries and
  inodes only.
- **Agent undo:** the Claude service takes a snapshot when an agent session
  starts and after each agent step. "Undo step" restores the previous step's
  snapshot for that session's `Dir` subtree. "Undo all" restores the session's
  start. The file manager browses snapshots read-only.
- **Persistence test:** write, sync, kill QEMU, reboot against the same disk
  image, and verify (M6 exit criterion with the RAMFS image, M10 with LumenFS).

## 10. IPC (planned, M9)

- **Channels:** bidirectional message queues created in pairs. A message is up
  to 64 KiB of bytes plus up to 16 handles (moved, not copied).
- **Typed protocols:** every service defines its messages as Rust types in a
  shared `no_std` crate (`lumen-proto-*`), encoded with a compact, versioned
  binary format (postcard-style). The same types serve in-kernel and
  user-space implementations.
- **Service registry:** `init` holds a registry channel. Services register a
  name and receive requests; clients ask the registry for a service and get a
  channel only if their capability set allows it.
- **Notifications:** a lightweight `event` object (signal and wait) for IRQs
  delivered to user-space drivers and for UI wakeups.

## 11. GUI and the automation API (planned, M4: designed in #146, built by #147 to #157)

Like the shell, the desktop runs in the kernel on kernel threads until M9
(see [ROADMAP.md](ROADMAP.md#how-this-roadmap-is-ordered)): the
compositor and every window's owner are kernel threads, and a window's
"shared memory" is heap memory both reach. M9 moves the owners into
processes (surfaces in shared pages, the protocol over IPC channels)
without changing the shapes below. Each heading names the issue that
builds it. The desktop's kernel code is `kernel/src/desktop.rs` (the
compositor, windows, `windows`, `desktop`) and `kernel/src/ui.rs` (`ui
tree`, `ui invoke`).

### Desktop mode (#153)

- **Opt-in.** The text console stays the default boot, so every golden in
  `tests/golden/` and every scenario stays as it is. The desktop starts
  with the `desktop` command (from any shell, serial included) or the
  `lumen.desktop` boot flag; `make run DESKTOP=1` passes the flag.
  Starting allocates the back buffer, then decodes the wallpaper (once a
  boot: a failure warns and the theme's colour is drawn instead; #154),
  then starts the `compositor` thread, then takes the screen; if a step fails, what was done is undone
  and the command prints `desktop: not started: <reason>` (the boot flag
  logs it as `WARN desktop: not started: <reason>` once and the boot goes
  on in text mode).
- **The desktop owns the whole screen**, header included: no "Lumen"
  title, no terminal tabs. The taskbar (#157) is its own header.
- **Holding the screen:** `term::take_screen(owner) ->
  Result<ScreenCanvas, CanvasError>`: a `Canvas` covering the whole
  screen, with a `Canvas`'s rules (one holder at a time, the terminals
  drawing nothing meanwhile but still taking output, their pointer off,
  its draws ending when the epoch changes). So `desktop` is refused while
  a command's canvas is held (`CanvasError::InUse`: "the screen is in use
  by another command"), and a command's canvas is refused while the
  desktop is shown. The compositor holds it with its own `term::Owner`,
  made once when the desktop starts, so the shell's `reclaim_canvas` after
  `desktop` returns leaves it alone.
- **States:** `off` (never started), `shown` (its `ScreenCanvas` is live)
  and `paused` (started, canvas given back). `vt::desktop_shown()` says
  whether it is shown (a private atomic, `vt::DESKTOP`); it is set and
  cleared only under `crate::vt`'s router lock (by start, resume and
  Alt+F`n`), so each key is routed on one side of the change.
- **The cut:** read as a copy through `vt::cut()` (a `vt::Cut` behind a
  private leaf lock, `vt::CUT`; its fields private, its writers
  `paused`/`resumed` in `vt`, its readers the `pause_owns_*` and
  `after_resume_*` predicates and `is_paused()`), holds a generation that only grows
  (one per change: 0 before the first start, odd while shown, even while
  paused), both rings' `newest()` at the latest pause and at the latest
  start or resume (`vt::Marks`, kept apart, so a compositor that sees a
  resume and then a pause in one look still has the marks of each), the
  buttons the pointer ring's newest frame holds at the latest start or
  resume (the baseline `Wm::resume` takes; captured here because the ring
  may overwrite that frame before the compositor reads it), and the
  compositor's `term::Owner`. Pausing and resuming write it under the
  router lock, before any later key is pushed; the compositor reads it
  each pass, after reading both rings (so a pause written after that read
  has marks past every event read: those were the desktop's), and moves
  its readers (`Reader::skip_to`). A resume first waits (up to 5 s, else
  `desktop: not resumed: the compositor hasn't caught up with the last
  pause (try again)`) until the compositor has acted on the pause before
  it, so no older marks are ever needed. For the scenarios,
  `lumen.desktop_cut_delay_ms=<ms>` (said at INFO, a bad value warned
  about, capped at 2000 ms) makes the compositor wait between reading the
  rings and reading the cut, widening the window a pause can be written
  in.
- **Leaving: Alt+F1 to F4** show that terminal: the one exception to the
  canvas rules (`term::show` still refuses while a canvas is held, which
  `test_term` checks; the header is redrawn by `fb::draw_header`). In the
  decoding pass, under the router lock, an Alt+F`n` press (the predicate
  `vt::switch_key(code, alt) -> Option<usize>`, which the compositor uses
  too) is pushed to the key ring like any key, then:
  1. `term::leave_desktop(owner, vt)`, under the screen's lock: if `owner`
     holds the `ScreenCanvas`, it changes the canvas epoch, releases the
     screen, shows terminal `vt`, and draws the header (`fb::draw_header`,
     moved out of `fb::init`), the tabs and the terminal. Any other
     holder's canvas still refuses the switch, as today.
  2. `vt::DESKTOP` is cleared and the cut written (paused, both rings'
     `newest()`).
  3. The pass goes on: later keys route to terminal `vt`. With every lock
     released, it calls `desktop::wake()`.

  The compositor, reading the key ring, forwards the key events before
  the Alt+F`n` press (which it recognises with the same predicate: a
  barrier it holds back, events after it kept, until it reads the cut
  that names it) and never that press or anything after it, and the
  pointer events up to the cut's pointer `Seq` (reading both rings again
  first: a screen shell's decoding pass may have pushed events and written
  the pause between the compositor's read of the rings and its read of
  the cut). So a window gets the Alt press of Alt+F1 (then `FocusLost`,
  which releases it), never the F1 press or release; F1's and Alt's
  releases come after the cut and are no window's (a release is never
  routed to a terminal: the decoding pass routes presses only). Then the
  focused window gets `FocusLost` (the
  grabbing window, if any, is the focused one; `Wm::interrupt`), and
  nothing more is presented.
- **Paused, not ended:** the windows live on (their threads keep running
  and drawing; their damage gathers). The pointer is the terminals' again
  (`term::move_pointer`). `desktop` resumes, under the router lock: it
  decodes the keyboard's pending bytes (into the shown terminal), takes
  the screen, writes the cut (not paused, both rings' `newest()`: nothing
  typed in a terminal reaches a window), sets `vt::DESKTOP`, releases the
  lock and wakes the compositor, which sends the focused window
  `FocusGained` (`Wm::resume` with the cut's baseline buttons) and
  presents everything once. The first start is entered the same way:
  the window manager starts paused, so `Wm::resume` with the first cut's
  baseline ignores a button held when `desktop` runs (pressed in the
  terminal) until every button is up.
- **Not ended in M4:** there is no `desktop off`; windows go when closed. A
  crash ends everything, as always.

### The compositor (#153)

A kernel thread, `compositor`, started by the first `desktop` and running
until a crash.

- **Its wake-up:** it sleeps on `desktop::WAKE` (a `WaitQueue`) until
  `desktop::PENDING.swap(false) || keyboard::bytes_pending()` (atomics
  only, no lock). Every waker calls `desktop::wake()`, which sets
  `PENDING` itself (and does nothing before the desktop started): IRQ
  1's handler when it queued bytes, the `mouse` thread after pushing a
  frame, every commit, every request (a shell command, `ui invoke`), and
  pause and resume. So desktop keys never wait for a screen shell: the
  compositor runs the keyboard's decoding pass itself
  (`vt::decode_pending`, which #153 splits out of `vt::read_key`), with
  none of its own locks held.
- **The back buffer:** a full-screen `lumen_gfx::Surface` (1280x800, 4
  MiB), allocated when the desktop starts and touched only by the
  compositor thread, so it needs no lock: the scene (wallpaper, then the
  windows from back to front with their frames), never the pointer.
  (`desktop check`'s scratch surface, below, is the same size.)
- **A pass:** read the cut and both input rings and act on them (below);
  under `desktop::COMPOSITOR`, take the gathered damage (screen
  coordinates) and a snapshot of the stacking order (an `Arc<desktop::
  Window>` per window), then release it; recompose only the damaged
  rectangles in the back buffer (the wallpaper, then each overlapping
  window from back to front: its frame, then its committed surface,
  blitted at most 16 rows per hold of the window's lock); copy the same
  rectangles to the screen with `Surface::present` into the `ScreenCanvas`
  (below: one row per hold of the screen's lock); and redraw the pointer
  if it moved or a copied rectangle touched it. No fixed frame rate:
  damage arriving during a pass waits for the next. A full present (a
  resume, a theme switch) copies 4 MiB, slow under TCG; everything else
  copies only what changed.
- **Shell commands:**
  - `desktop`: start or resume (`desktop: started (Alt+F1 shows the
    terminals)`, `desktop: resumed`); while shown it prints `desktop
    status`. A refusal says `desktop: not started: <reason>` or
    `desktop: not resumed: <reason> (try again)`; a second `desktop`
    while one is starting or resuming the desktop is refused with
    ``another `desktop` is starting or resuming it`` (the whole line:
    ``desktop: not resumed: another `desktop` is starting or resuming it
    (try again)``, or `not started: ...`); a first start never replaces a
    desktop already there, started or starting. And a
    resume refused because the compositor hasn't acted on the last pause in
    5 s also logs a WARN.
  - `desktop status`: one record, below.
  - `desktop check` (for the tests): the compositor runs it right after a
    pass: it recomposes the whole scene into a scratch surface (allocated
    first by the command) and counts the pixels that differ from the back
    buffer (damage-only drawing equals a full repaint), then reads the
    framebuffer, converting its channel layout back, and counts the pixels
    that differ from the back buffer outside the pointer's box. It prints
    `desktop check: <n> pixels differ from a full repaint, <m> from the
    screen`, or `desktop check: busy` if damage or a commit arrived since
    the pass (the harness retries, a bounded number of times).
    `desktop check` and `screenshot` (while the desktop is shown) share
    one request slot, reserved from
    the moment a request is accepted until its asker takes the answer (or
    gives up after 20 s, when an answer still coming is dropped): a second
    request meanwhile is refused at once with `desktop check: busy with
    another request (...): try again` (or `screenshot: the desktop's back
    buffer: busy with ...`), never handed the first one's answer. An
    asker giving up also logs `WARN desktop: no answer from the
    compositor in 20 s`; while the compositor still runs the request
    given up on, later ones are refused with `the compositor hasn't
    finished a request given up on (stuck?)`, and its late answer is
    dropped (said at INFO). A request still queued when the desktop pauses is
    answered at the paused pass: ``desktop check: paused (`desktop`
    resumes it)``, and `screenshot` reads the text screen instead (what is
    shown then). A check whose screen is taken back (Alt+F`n`) while it
    reads it says `desktop check: the screen was taken back during the
    check`.
  - `desktop spin` (for the tests): opens a window whose owner commits a
    changed frame (a pure green bar moving) every 20 ms above a toolkit
    Stop button (which closes it: a request its queue can refuse as busy,
    #156) and reads its events.
  - `desktop counter` (#156): opens Counter, the toolkit's demo app (a
    count, Add and Reset buttons, a Lock checkbox that disables Add, and
    a Quit button that closes it; its logic is `lumen_ui::counter`).
  - `desktop hold <window>` and `desktop release <window>` (for the
    tests): a `desktop spin` window's owner stops, then resumes, reading
    its event queue (other windows refuse).

  `desktop check`, `desktop spin`, `desktop counter`, `desktop hold` and
  `desktop release` refuse while paused.

- **`desktop status`**, one record (reading it doesn't wake the
  compositor): the desktop's columns under `COMPOSITOR`, the keyboard's
  count, the rings' `newest()` and the cut read just before it (so not one
  instant of all of them). Its columns, in order:

  | Column | Meaning |
  |---|---|
  | `state` | `off`, `shown` or `paused` |
  | `theme` | the theme drawn with: `light` or `dark` (also while `off`) |
  | `windows` | windows open |
  | `frames`, `rects`, `pixels`, `present_ms` | passes that presented, rectangles presented, the sum of their areas (clipped to the screen; an overlap counted in each, so what `present` set out to copy, not a count of pixels written), time spent copying |
  | `pointer_read`, `pointer_newest`, `keys_read`, `keys_newest` | the compositor's `Reader::after` on each ring, and the ring's `newest()` |
  | `lost` | events the compositor's readers lost (gaps) |
  | `damage` | rectangles gathered and not yet presented |
  | `no_focus_keys` | key events dropped because no window was focused |
  | `stray_forgotten`, `stray_unpressed` | button releases ignored: their press was forgotten (`FocusLost`, `Lost`) or dropped (expected), or the button was never pressed (never expected) |
  | `key_releases_unpaired` | key releases ignored because their press didn't reach that window (expected: keys go to the window focused when they arrive) |
  | `ignored_presses` | presses not taken as presses on a window: made in an `after_overrun` event, or while buttons held across a pause or gap or at a resume (the first start included) were down |
  | `rationed`, `overflowed` | presses and moves dropped past a queue's 48 entries, and releases that didn't fit its 64 (each set `Lost`): their sum is `windows`' `lost`, totalled over every window, closed ones too |
  | `grab_ends` | grabs ended before their last button came up (their window closed or lost the focus, or input was interrupted) |
  | `dead_pushes` | events and flags for a window that had closed: its queue's own count after it was killed, plus deliveries and `ui invoke` requests whose window is no longer on the list (a delivery: 0 by construction, the list and `lumen-wm` change in one hold; a request: one asked for as its window closed) |
  | `glyph_misses` | glyphs the compositor rasterized for the titles (its own glyph cache's misses, not the windows'): flat once every title's glyphs are cached; `-` with no UI font (#154) |
  | `glyphs_dropped` | glyphs any `font::Text` in the kernel left out because `lumen_gfx::Mask::new` refused their mask (a bug; the first warned about with its sizes) (#154) |
  | `wallpaper` | `shown`, or why not: `unreadable` (not a PNG, cut short, corrupt), `too-big`, `no-memory`, `none` (#154) |
  | `cancelled`, `late` | `ui invoke` requests cancelled at their deadline, and results that came after it |
  | `settled` | `yes` or `no`, below |

  **`settled`** is `yes` when all of these hold: the state is `shown` and
  the compositor has acted on the cut's generation; every byte the
  keyboard queued has been decoded (the bytes queued, `kbd`'s count kept
  under the keyboard's `ARRIVED` lock, are no more than the bytes the
  decoder finished, an atomic counter bumped after each byte's key event
  is pushed: `keyboard::all_decoded`); both readers
  are at their ring's `newest()` and the compositor holds no event back
  (the Alt+F`n` barrier, or a resume whose screen hasn't come yet); no
  wake is pending, no damage is gathered and no pass is running; no
  commit is under way; every window's queue is idle (no entry and no
  flag), its owner has been told of the last theme switch, and it is
  blocked in `wait_event` (an idle bit per window, read after the other
  two, so a look never takes an owner mid-redraw for a waiting one); and
  no `ui invoke` request is outstanding (from its push to its asker's
  end, answered or not; the `desktop check`/`screenshot` slot doesn't
  count). A test first polls for the effect it expects (a `windows` row,
  a `ui tree` value), then calls the harness's `wait_settled(m)`
  (`tools/harness/lumen_harness/gui.py`, #153), and brackets each
  screendump between two `desktop status` reads with the same `frames`.
  Windows that animate (a blinking cursor, `desktop spin`, Life) never
  idle: tests close them first or mask them, and their goldens leave them
  out. A scenario that only checks state ends with `desktop check`
  reporting 0 and 0.

### Windows and who draws (#153)

- **Opening:** `desktop::open_window(title, w, h) -> Result<WindowHandle,
  OpenError>`. `OpenError::{NoDesktop, TooMany, TooBig, OutOfMemory}`: at
  most `MAX_WINDOWS` (32) windows; a client no bigger than the screen; the
  surfaces allocated with `try_reserve`, so a failed allocation is an
  error, not a panic. Window ids count from 1, never reused in a boot.
- **Shared and owned parts:** `desktop::Window`, shared as `Arc<Window>`
  by the compositor's list and the handle, holds the window's lock (its
  committed surface, its event queue, its sticky flags, its idle bit).
  `WindowHandle` holds that `Arc` and the owner's surface; it is `Send`
  (a thread may open a window and hand it to the thread that will own
  it), not `Sync`, not `Clone`. Dropping it destroys the window: its
  queue is killed (dead), every `Pending` request is answered "window closed",
  `lumen-wm` drops it (the damage of its frame), and its surfaces are
  freed with the last `Arc`.
- **The desktop's state** is a `desktop::State`: the window manager
  (`lumen_wm::Wm`), the open windows, the gathered damage, the
  statistics, the counts of closed windows' queues, the commits under
  way, whether the start completed, the compositor's view of the cut and
  its readers' places, the screen handed over for a resume, the request
  slot, and the wallpaper (#154). The live one is in
  `COMPOSITOR`, a static private to the module, and `desktop::Desktop`
  names where one lives (`Desktop::LIVE`, or a test's own). Windows open
  only once the start completed (`OpenError::NoDesktop` until then), so a
  start that fails opens none and window ids are never reused in a boot.
  `Desktop::without_screen()` builds one with no compositor thread and no
  screen, for the `desktop::` ktests (the window limit, dead pushes, the
  request slot, the wallpaper, the titles' cache, the theme's delivery),
  so no ktest leaves a desktop running.
- **Two surfaces per window**, each of its client size: the *owner's*,
  inside the handle (`surface_mut(&mut self)`), drawn with no lock at the
  owner's pace; and the *committed* one, behind the window's lock, which
  the compositor reads. `commit(&mut self, &[Rect])`, rectangles in
  client coordinates, copies them across at most 16 rows per hold of the
  window's lock (a `SpinLock` keeps interrupts off), then, under
  `COMPOSITOR`, moves them to screen coordinates by the window's current
  place and queues them. A pass may compose a rectangle while a commit is
  part way through it (old rows below new ones for one frame); the
  commit's damage, queued after its last rows, makes the next pass draw it
  whole. An owner never waits for a present.
- **Lent bands (#157):** the Mandelbrot window draws with every CPU.
  #157 adds `sched::scope` (it joins every thread it started before
  returning, `run_sized`'s argument applied to several threads) and
  `WindowHandle::bands_mut(&mut self, n) -> [BandMut<'_>]`
  (`split_at_mut`-style disjoint row bands, each with `row_mut` and `impl
  lumen_gfx::Target`). Because `surface_mut` and `commit` take `&mut
  self`, the compiler refuses a commit while bands are lent.
- **Who may draw where:** an owner draws only into its own surface (its
  client area); the frame (title bar, border, close box, later minimize)
  is drawn by the compositor from `lumen-wm`'s geometry and the theme. The
  compositor alone writes the back buffer and, while the desktop is shown,
  the screen.

### Window events (#153)

`WindowHandle::next_event()` (non-blocking) and `wait_event()` (which sets
the window's idle bit while it blocks until the owner has an event:
`has_event()`, below) return
`WindowEvent`s: `Key(KeyEvent)`, `PointerDown`, `PointerMove`,
`PointerUp` (client coordinates and the buttons held), `FocusGained`,
`FocusLost`, `Lost` (events for this window were dropped), `Close` (the
close box, or `ui invoke <root> close`), `Request` (a `ui invoke`
request, `crate::ui::Request`, #156: the node, the action and its reply
slot; not `Clone`, and it answers "window closed" if dropped
unanswered) and `Theme(&'static Theme)` (#154: the theme to draw the
client area with, a window's first event and again after each switch).
`WindowEvent` derives `Debug` only (owners match it with `matches!`).

- **The theme reaches every owner, first.** `Theme` isn't a queue entry
  or a flag: each window keeps the count of theme switches its owner has
  been told of, starting one behind the desktop's, so a window's first
  event is always `Theme(current)` (an owner's first draw and every
  redraw are one path, and nothing it read before opening can be stale).
  While the count is behind, `next_event` returns `Theme` ahead of
  anything queued (entries, `Close` and requests included, unlike the
  flags below, which keep their place). What an owner has to read (the
  queue's state and the count) is private to one small module, which
  answers only `has_event()` (something queued, or a theme to tell);
  every wait goes through one private `Window::wait(deadline, also)` that
  always ORs it in (`wait_event`; #155's `wait_event_until` builds on it,
  never on a predicate of its own). So it is never dropped; several
  switches before the owner reads give at least one `Theme`, the last
  carrying the current theme (a switch between the owner's read of the
  count and of the theme gives a second). An owner treats `Theme` as
  "redraw" and draws with `WindowHandle::theme()`, the theme it was last
  told of (the theme at the open until the first); `desktop::theme()` is
  private to the compositor, `desktop status` and `theme`. A window's
  count is behind until its owner reads it, and `settled` needs every
  owner told of the last switch: a test after `theme` waits for the
  owners' redraws too. Only windows hear a switch: something drawn
  outside a window (#157's taskbar, if it isn't one) needs a count of its
  own.

- **Requests are never dropped under a desktop lock** (a dropped one
  answers and wakes its asker): `EventQueue::push_request` hands a refused
  request back (`lumen_wm::Refused::Busy` past the ration, `Refused::Dead`
  once the queue is dead), `desktop::push_request` does the same for a
  window no longer on the list, and `EventQueue::kill` returns the
  requests still queued; each is answered once the locks are released
  (the asker prints "busy" or "window <id> closed", `WindowHandle`'s drop
  answers "window closed").

- **Sticky flags beside the queue.** `Lost`, `Close`, a focus loss and a
  focus gain are flags beside the queue (`set_lost`, `request_close`,
  `set_focus`) and need no slot, so they are never dropped. The queue's
  entries are `lumen_wm::Entry<K, R>`: `Key(K)` (#151's key event, through
  the `KeyLike` trait: its code and whether it is a press), `Pointer`
  (`PointerEntry::{Down, Move, Up}`, client coordinates, one
  `lumen_input::Button` each) and `Request(R)` (#156); it has no variant
  for a flag, so one
  can't be pushed. A press or release is of a `Held::{Code(K::Code),
  Button(Button)}`. Each flag is delivered in its place, after the entries queued
  before it was set (set again, it moves to the end): delivered ahead of
  older entries, `FocusLost` or `Lost` would clear the owner's held keys
  and buttons before a press queued earlier, whose release (a stray by
  then) never comes. A loss is never folded into a gain: a pause and its
  resume before the owner reads deliver `FocusLost` then `FocusGained`; a
  gain then a loss delivers only the loss. Due together, they come in the
  order `Lost`, `FocusLost`, `FocusGained`, `Close`.
- **The queue** (`lumen_wm::EventQueue`, a host-tested pure type): 64
  entries. A `PointerMove` replaces a `PointerMove` at the tail. Presses,
  moves, key presses and requests are queued only while fewer than 48
  entries are; past that a press or move is dropped (`rationed`) and a
  request refused (`ui invoke` says "window <id> busy"). A release is
  queued (in any free slot) only if the queue took that key's or
  button's press; any other release is ignored and counted: a key's as
  `key_releases_unpaired` (expected: keys go to the window focused when
  they arrive, so Shift held across a click, or the Enter of `desktop`
  itself, is released in a window that never saw it go down), a button's
  as `stray_forgotten` if its press was forgotten (`FocusLost`, `Lost`) or
  dropped, else as `stray_unpressed` (never expected: the window manager
  delivers a button's release only after its press). A release of a
  press taken that doesn't fit sets `Lost` (`overflowed`).
  Every drop is counted in the window's `lost` (`rationed` plus
  `overflowed`, a `windows` column) and logs one `WARN desktop: window <id>
  is behind: dropping events` per burst (until the queue empties).
  `lumen_wm::deliver` puts each `lumen_wm::Delivery` on the queue of its
  window (`Delivery::window`): a pointer event as an `Entry::Pointer`, the
  rest as flags.
- **Held state never goes stale:** on `FocusLost` and `Lost` an owner
  treats every key and button as released. The grabbing window is always
  the focused one (a grab ends when its window loses the focus or closes,
  or input is interrupted: `grab_ends`). `Wm::interrupt`:
  - a pause: the focused window gets `FocusLost`; the drag or grab ends;
  - a gap in the pointer ring (a `Reader` gap, or an event with
    `after_overrun`): the focused window gets `Lost`; the drag or grab
    ends; with buttons held, nothing more is delivered until a pointer
    event reports every button up (a press in the frames lost wasn't
    seen), and a button down in the `after_overrun` event itself is no
    press either (`ignored_presses`); with none held, the next press is a
    press;
  - a gap in the key ring (`Interruption::KeyGap`: a `Reader` gap on the
    key ring, `Read::lost_before`, or a key event with `after_drop`, both
    #151): the focused window gets `Lost` (a key it holds may never be
    released); a grab on it ends (its buttons ignored until up); a
    title-bar drag goes on. While paused, pointer events are
  ignored and focus changes (a window opened or closed meanwhile) aren't
  delivered. The resume (`Wm::resume(baseline)`, where `baseline` is the
  buttons of the pointer ring's newest frame at the cut) tells the
  focused window, as it is then, `FocusGained`; buttons in `baseline`
  are ignored until a pointer event reports every button up (pressed in
  the terminal: counted as `ignored_presses` if more come meanwhile).
- After a window is destroyed its queue is dead (`EventQueue::kill`):
  events and flags for it are counted (`dead_pushes`), never queued.

### Stacking, focus and dragging: `lumen-wm` (#152)

A host-tested crate with no pixels and no locks. Windows (id, title,
frame rectangle, client rectangle derived from it) in stacking order;
hit testing front to back (title bar, close box, client area, desktop);
focus (a click on a window focuses and raises it; closing the focused one
focuses the next one down; exactly one is focused, always the one in
front, unless there are none); the drag state machine over pointer events
(a left press on a title bar starts it, moves move the frame, clamped so a
strip of the title bar stays on the screen, and the left button's release
ends it; other buttons held then are ignored until they are up); and the
implicit grab (a press on a client area, the moves while it is down, and
its release go to that window, in client coordinates; other buttons
pressed meanwhile go there too, and the grab lasts until the last of them
is released, since the window was told of each press and must be told of
each release, or until the window loses the focus or closes). A left
press on the close box and its left release over it is a `Close` for the
window (other buttons meanwhile are ignored); the owner decides, and
`Wm::close` removes it. A press on the desktop or a border (or with
another button on a title bar or close box) does nothing until every
button is up (a press on a border first focuses and raises the window).
Moves with no button held aren't delivered (no hover in M4).

- **API:** `Wm::new(w, h, Metrics::DESKTOP)`; `open` (in front, focused,
  clamped like a move, by the frame's whole width), `close`, `focus`
  (raise and focus: the taskbar, `ui invoke <root> focus`) and `move_to`
  (each `None` for a window not open), `pointer(Pointer)`,
  `interrupt(Interruption::{Pause, Gap, KeyGap})`, `resume(baseline)`, and
  queries (`windows` back to front, `window`, `id(n)` for a number typed
  in `ui invoke` or read from `windows`, live windows only, `focused`,
  `hit`, `client`, `dragging`, `grabbing`, `is_idle`, `is_paused`,
  `counts`). Each operation returns an `Outcome`: its `Damage` (a moved
  window's old and new frames, a raised window's frame, both frames of a
  focus change, since a focused frame is drawn differently) and its
  `Delivery`s (`PointerDown`/`PointerMove`/`PointerUp` in client
  coordinates, one `lumen_input::Button` each, `FocusGained`,
  `FocusLost`, `Lost`, `Close`), which #153 puts on each window's queue with
  `lumen_wm::deliver`. `Outcome` and the `Option`s are `#[must_use]`.
- **Coordinates:** screen pixels as `i32`, as `lumen-gfx` uses (a window
  may hang off the left, right or bottom edge, never the top);
  `Pointer::from_read` takes a pointer ring's read (#151): the tablet's
  `usize` position (saturating, through `Pointer::from_state`), its
  buttons (`lumen_input::Buttons`, the one button set the tablet and the
  window manager share) and `Read::gap()` (a gap in the ring or
  `after_overrun`, handled before the event).
- **Geometry** (`Metrics::DESKTOP`): a 24-row title bar spanning the
  frame (the side borders' top rows are title bar), 1-pixel borders, a
  16-pixel close box 4 pixels in from the right border; a moved frame
  keeps its title bar's rows on the screen and at least 48 pixels of its
  width (all of it, if narrower).

A model check runs random operations (opening, closing, focusing,
moving, pointer events and clicks aimed at title bars, close boxes and
clients, pauses, pointer and key gaps, `after_overrun`) and checks after
each: repainting
only the reported damage equals repainting everything (painter's
algorithm, each pixel knowing its window, part and whether that window is
focused); the focused window is the front one and holds any grab; hit
testing agrees with the picture; a drag follows the pointer, clamped, and
only while the left button is held; a first press on a client is that
window's; and, through each window's `EventQueue`, only the grabbing
window believes buttons are held, and no stray release reaches a queue.
Host tests run press, pause, resume, release, and the same with a ring
gap and with `after_overrun` in place of the pause: the interruption
alone releases the owner's buttons, and no window is left holding one.
`EventQueue` (#153) and the themes' palettes (#154: `Theme::LIGHT`,
`Theme::DARK`, `by_name`) live here too.

### Input on the desktop (#151, #153)

- **Routing in the decoding pass:** while the desktop is shown, the pass
  routes only Alt+F1 to F4 to `term` (above; `Routed::Left`); every other
  press but a bare modifier (`Routed::Modifier`, as in text mode) goes to
  the key ring alone and is counted as `Routed::Desktop` (`vt` ends with
  `N keys to the desktop`); every key event, releases and modifiers
  included, is in the key ring already (`keyboard::read_key`). Whether
  the desktop is shown is `vt::desktop_shown()`.
  So typing into a window never reaches the hidden terminal's shell.
- **The compositor** reads the tablet's ring and the keyboard's ring
  (section 7) with one `Reader` each. Pointer input goes in as
  `lumen_wm::Pointer::from_read(&read)`, which carries `read.gap()` (a
  gap in the ring or a frame after an overrun), so no separate `Gap`
  interruption is needed; through `lumen-wm` it becomes focus, a drag, the
  close box, or a window's `PointerDown`/`PointerMove`/`PointerUp`. For
  keys, `if read.gap() { wm.interrupt(KeyGap) }` first; then the key event
  (a `lumen_wm::KeyLike`) goes to the focused window's queue; with none
  focused it is dropped (`no_focus_keys`).

### The pointer (#153)

The compositor draws the pointer straight onto the screen, never into the
back buffer. It copies the back buffer's pixels under the arrow's box
(11x17, `lumen_input::cursor::SPRITE_W` by `SPRITE_H`) into a small
array, paints the arrow there with `lumen_input::cursor::draw_arrow(pixels:
&mut [u32], stride: usize, size: (usize, usize), tip: (usize, usize),
outline: u32, fill: u32)` (draw-only, clipped to `size`, its stride and
the buffer, built on the
sprite's `cursor::paint`; #153 adds it, as `Cursor::show` reads its target
to save what it covers), and copies the array to the screen. Taking it off
is copying the back buffer's box. The screen is never read (except by
`desktop check`). In text mode the terminals keep `lumen_input::Cursor`'s
save-under, as today. So the back buffer, and a desktop screenshot, never
show the pointer, while QEMU's `screendump` does once a tablet reported a
position: desktop goldens run without a tablet or keep the pointer away
from what they compare.

### Locks (#153)

| Lock | Holds | Held for |
|---|---|---|
| `desktop::COMPOSITOR` | the window list (`lumen-wm`), the gathered damage, the requests, the statistics | a few list operations, a snapshot |
| a window's lock | its committed surface, its event queue, its flags, its toolkit's published tree | 16 rows of a commit or a blit, one queue operation, swapping in a tree |
| a `ui invoke` reply's lock (`ui::ReplyCell`, #156) | the reply's state | one transition (a leaf: taken under its wait queue's lock by the asker's wake-up check, and never held across a wake) |
| `vt::CUT` | the cut | a read or a write (a leaf) |
| `desktop::BOOT_WALLPAPER` | the embedded wallpaper's decode, once done (#154) | a read or a write of two `Arc`s (a leaf: nothing allocated or freed under it) |
| `desktop::DONE`'s wait-queue lock | the threads waiting for an answer, for the compositor to catch up, or for a window `ui invoke <root> close` closes (`close_window`) | a wake, or a wait's predicate, which takes `COMPOSITOR` (so this lock comes before it, and no wake is made holding `COMPOSITOR`) |
| `term`'s screen lock | the terminals; and each row the compositor presents | one row of a present: at most 800 holds for a full present |

- **Order:** `COMPOSITOR`, then a window's lock, then the heap's locks
  (the heap, the page tables, the frame allocator), then
  `klog::LINE_LOCK` (any log line), then the screen's lock. The router
  lock comes before the screen's lock (`leave_desktop`, `take_screen`),
  `vt::CUT` and the input rings' locks (leaves, which may be taken under
  any of these, `COMPOSITOR` included: the compositor reads the rings
  again under it when it acts on a pause). The desktop's statics are
  private to `desktop.rs` and the cut is read only as a copy
  (`vt::cut()`), so outside those modules nothing can take them out of
  order.
- **Never upward:** nothing holding the router lock, `LINE_LOCK` or the
  screen's lock takes `COMPOSITOR` or a window's lock, and nothing holding
  those takes the router lock: the two families are never nested (the
  pause is an atomic, the cut and a wake; the compositor runs the decoding
  pass holding none of its locks). Every wake happens after the lock that
  guarded the change is released.
- **Presenting under the screen's lock:** `ScreenCanvas`'s `write_span`
  takes the screen's lock for its row and writes only if the canvas is
  still live, checked under that lock. `leave_desktop`'s redraw runs under
  the same lock, so a present preempted mid-rectangle can't land a stale
  desktop row on the redrawn terminal.
- **Allocation:** allowed under `COMPOSITOR` and a window's lock (the
  heap's path doesn't log, and its locks come after them). Nothing
  allocates under the screen's lock, as today (`term::drawing_here`,
  checked by the heap). A `WaitQueue`'s own lock allocates when a thread
  waits (its `VecDeque` grows), so nothing waits holding any of the locks
  above.

### The crash screen still wins (#153)

`fb::with_crash_screen` draws straight to the pixels, with no lock and no
heap, after `crash::enter` called `term::stop` and stopped the other CPUs.
`term::stop` changes the canvas epoch, which ends the desktop's
`ScreenCanvas` too: no present row starts after it (a row already being
written on another CPU finishes, or that CPU is stopped first).
Nothing on the crash path takes a desktop lock.

Tests: `ktest:lumen_kernel::desktop::a_present_stops_at_the_next_row_once_the_canvas_dies`
drives the real span path (`ScreenCanvas::write_span`) with a test hook
that changes the epoch after `k` rows, and reads back that the later rows
are unchanged; `scenario:test_crash::test_crash_report_over_desktop` (under
`-smp 2`, with `desktop spin` committing, a fault from the serial shell)
checks for a full symbolized report and exit code 37 on one machine, and
on a second one booted without `lumen.qemu_exit` (so the screen stays up)
the crash screen covering the desktop: the banner, and not one pixel of
the spinner's green.

### Pixels: `lumen-gfx` (#147)

A host-tested `no_std` crate every desktop part draws with. Pixels are
`0xXXRRGGBB` (`u32`, the top byte unspecified).

- `Rect`: signed origin, unsigned size; intersection, bounding box,
  containment, clipping, subtraction into at most four disjoint pieces,
  and `translate(dx, dy)` (each edge clamped to `i32`'s range: a part
  pushed past either end is cut off, never kept at its size).
- `Damage`: a fixed-size list that never allocates. A rectangle added
  merges with those it overlaps or touches only when the merged box costs
  no more pixels than drawing both; past the list's size everything
  collapses into one bounding box (`collapses()` counts it). The
  rectangles reported may overlap (overlapping parts are drawn twice),
  and always cover every pixel added.
- `Surface`: an owned buffer with a stride: fill a rectangle, read and
  write a pixel, copy a rectangle to any `Target` clipped on both sides
  (`copy_to`, `blit`), `present` (a list of rectangles to a `Target`), and
  `blend_mask_in(&Mask, x, y, color, clip: Rect)` (an 8-bit coverage mask
  in one colour, rounding as `blend_channel` states, touching only pixels
  inside `clip`; `blend_mask` calls it with the surface's bounds). Its
  purpose is redrawing inside one damaged rectangle: blending is not
  idempotent, so a glyph blended again over pixels outside the rectangle
  being recomposed would darken them. The rule for #153: the compositor
  draws window-frame text (titles) with `blend_mask_in(.., damage_rect)`
  once per damaged rectangle it recomposes, never with `blend_mask`, or
  `desktop check` (damage-only against a full repaint) fails.
- `Target`: `size() -> (u32, u32)` and `write_span(x: u32, y: u32,
  pixels: &[u32])`. `lumen-gfx` only calls it with spans inside `size()`;
  a span outside is the caller's bug, and no target writes outside itself
  for one. An in-memory target (a `Surface`) may panic on such a span, as
  `Surface::row_mut` does. The kernel's `ScreenCanvas`, the front buffer,
  converts each pixel to the framebuffer's channel layout, and its
  `write_span` never panics: it drops spans it can't take (outside the
  screen, or once the canvas isn't live or no longer holds the screen).

### Text and images (#148, #154)

- **TrueType** (#148): `lumen-font` over `fontdue` (without its `simd`
  feature) and one embedded OFL font. The `Face` is parsed once into a
  shared `&'static Face`; each drawing thread owns a `Font<'static>` over
  it, with its own glyph cache bounded in bytes (float math is soft-float
  in the kernel, so a cached glyph is never rasterized again), so drawing
  takes no shared lock. One-line layout gives a `LineMask` (width, height
  and coverage through getters), which reaches the surface through
  `lumen_gfx::Mask::new(width, height, coverage)`; if that returns `None`
  (the sizes disagree, a bug), the line isn't drawn and one `WARN` says so;
  nothing panics. The desktop draws glyph by glyph instead (below), with
  no line mask. The terminals keep the bitmap font.
- **Text on the desktop** (#154): `font::Text` (`kernel/src/font.rs`) is
  one drawing thread's text: its own `Font<'static>` glyph cache over the
  shared face (`Text::new()`, which says why if the font didn't load),
  `draw(surface, x, y, text, px, colour, clip)` (returning nothing),
  `measure` (checked against the drawn ink: `font::text_ends_where_measure_says`)
  and `line_height` (a `u32`), at the public sizes `TITLE_PX`
  (15), `TEXT_PX` (16) and `HEADING_PX` (24). The compositor has one for
  the titles (their line height worked out once), each window's owner its
  own (#156's widgets use it too). Each glyph's `Placed` mask goes through
  `Mask::new` and `blend_mask_in` with the clip it is drawn in (titles:
  the damaged rectangle being recomposed, once each), so no line mask is
  allocated. Redrawing a title rasterizes nothing (its glyphs are cached:
  `desktop::a_title_drawn_again_rasterizes_nothing`; `desktop status`'s
  `glyph_misses` shows the compositor's count). A mask `Mask::new`
  refuses (a bug) isn't drawn: counted (`glyphs_dropped`), the first
  warned about with its sizes (not tested: `lumen-font`'s masks are
  always their size). If the UI font doesn't load, one `WARN` says so and
  no text is drawn (not tested: the embedded font loads).
- **Wallpaper** (#154): `assets/wallpaper/lumen.png` (1280x800, about 150
  KB, original: `tools/wallpaper.py` makes it, `assets/wallpaper/README.md`),
  embedded in the kernel and decoded once a boot, at the desktop's first
  start, after the back buffer is allocated (the desktop needs that; the
  wallpaper is optional), said at INFO with its size and the time it
  took. The result, or why it failed, is kept (`BOOT_WALLPAPER`), so a
  start that fails after it doesn't decode or warn again; a decode the
  heap couldn't hold isn't kept, and is tried (and warned about) again at
  the next start (`desktop::a_wallpaper_is_decoded_and_warned_about_once`).
  `lumen_png::decode` over `zune-png` (`=0.5.2`, without its `std` and
  `sse` features; strict mode: every chunk's CRC and the zlib stream's
  Adler-32 checked): any PNG into `0x00RRGGBB` pixels, alpha dropped and
  16-bit samples cut to their top 8 bits, its pixels handed to the
  surface with no copy (`Image::into_pixels`, `Surface::from_pixels`).
  Only the image's own chunks reach the decoder: IHDR, PLTE, the IDATs
  and IEND are copied whole into a buffer of their own, and every
  ancillary chunk is dropped, so nothing but the image data is ever
  inflated (zune-png inflates text and colour-profile chunks while
  reading the headers, and its parsers don't always take what a chunk
  declares; host-tested with hidden and many-chunk bombs under a refusing
  heap). The size is checked from IHDR before any image data is inflated:
  either side past 2048 pixels is refused (a 2048x2048 RGBA file peaks at
  about 40 MiB while it decodes, since the decoder's inflated rows are
  `Vec`s that abort on a heap that can't hold them; the copy, the
  samples' and the pixels' buffers refuse). Not a PNG, cut short,
  corrupt, an unknown critical chunk or too big is an error, never a
  panic (host-tested: every truncation and every flipped byte of a small
  file, an Adam7 file). It is drawn centred: cut where it is bigger than
  the screen (`desktop::a_wallpaper_bigger_than_the_screen_is_cut_centred`),
  the theme's `desktop` colour around it where it is smaller
  (`desktop::a_smaller_wallpaper_is_drawn_centred_over_the_flat_colour`).
  If it doesn't decode, the desktop uses that colour and logs `WARN
  desktop: wallpaper not shown: <reason>`, and `desktop status`'s
  `wallpaper` says why in a word. One wallpaper for both themes.
- **Themes** (#154): `lumen-wm`'s `ThemeId::{Light, Dark}` and their
  palettes (`Theme::get(id)`, `Theme::LIGHT`, `Theme::DARK`). `theme`
  prints the current one (`theme: light`); `theme light|dark` switches
  (`theme: dark`, or `theme: dark (already)`; another name is refused),
  before the desktop starts too (it starts in that theme), and
  `lumen.theme=` picks one at boot. The theme is the kernel's (one
  screen): a switch, under `COMPOSITOR`, changes it and its count and
  damages the live desktop's whole screen (one full recompose: frames
  and wallpaper); each window's owner is then told (`Theme`, above) and
  redraws its client area in `WindowHandle::theme()`. Themes are named by
  `ThemeId` (`index`/`from_index` checked at compile time; a `Theme`
  carries its `id`). `desktop status`
  shows it (`theme`). The terminals keep their own palette (#155 decides
  whether terminal windows follow the theme).

### Terminal windows (#155)

`term.rs`'s grid renderer draws onto a `lumen-gfx` `Surface` as well as
onto the screen (the text console renders exactly as before). A terminal
window owns a `lumen_term::Terminal`, its scrollback and a shell thread;
its keys are the window's key presses through `lumen_shell::Key::
from_press` (section 7). Only the focused terminal window's cursor blinks;
an unfocused one's is drawn hollow and steady. `term text <window>` prints
a terminal window's rows as `term text` does the screen's.

### Toolkit and the automation tree (#156)

`lumen-ui` is a retained widget tree; every widget is also an automation
node, produced from the widget itself, so the tree can't drift from what
is drawn:

```rust
pub struct AutomationNode {     // fields private; one constructor per role
    id: lumen_ui::NodeId,       // stable for the node's lifetime
    role: Role,
    name: String,               // accessible label: "Add", "File name"
    value: Option<Value>,
    state: StateFlags,          // focused, disabled, selected, expanded
    bounds: lumen_gfx::Rect,    // client coordinates
    children: Vec<lumen_ui::NodeId>,
}
pub struct NodeId(NonZeroU64);  // private; NodeId::fresh(), NodeId::get()
pub enum Value { Text(String), Checked(bool), Selected(Option<usize>) }
pub enum Action { Press, SetValue(String), Select, Focus, Close, Minimize, Restore }
pub enum ActionKind { Press, SetValue, Select, Focus, Close, Minimize, Restore }
impl Role { pub fn actions(self) -> &'static [ActionKind]; }
```

- **`lumen_ui::NodeId` is global** (always written qualified, the kernel's
  `desktop.rs` included: the VFS's
  `NodeId`, section 9, is another type). `NodeId::fresh()` is one counter
  for the whole system and an id is never reused in a boot. Lookups go by
  the raw number (`NodeId::get()`): `ui invoke 12 press` looks up 12, and
  text never becomes a `NodeId`; an id kept after its window closed names
  nothing. The compositor owns each window's root node (role `window`,
  name the title); a toolkit window's widgets hang under it, and a window
  without a toolkit (a terminal, a demo) has the root alone. Window ids
  are a separate count.
- **Roles** (a host test checks each role's value kind, actions and the
  state flags it may carry; a new role is added here first). A role's
  actions are `Role::actions()`; a list is built from its items and one
  `selected: Option<usize>`, and the items' `selected` flags are derived
  from it.

  | Role | Value | Actions | Flags it may carry | Answered by |
  |---|---|---|---|---|
  | `window` (a root) | none | `focus`, `close`; `minimize`, `restore` (#157) | `focused` | `focus`, `minimize`, `restore`: the compositor; `close`: the owner (a `Close` event) |
  | `group` (a stack, the taskbar) | none | none | `disabled` | |
  | `label` | `Text` | none | `disabled` | |
  | `button` | none | `press`, `focus` | `focused`, `disabled` | the owner |
  | `checkbox` | `Checked` | `press` (toggles), `focus` | `focused`, `disabled` | the owner |
  | `textfield` | `Text` | `set_value <text>` (replaces the text, as one edit, not keystrokes), `focus` | `focused`, `disabled` | the owner |
  | `list` | `Selected` | `focus` | `focused`, `disabled` | the owner |
  | `listitem` | none | `select`, `press` | `selected`, `disabled` | the owner |
  | `menu` | none | `press`, `focus` | `focused`, `disabled`, `expanded` | the owner |
  | `menuitem` | none | `press` | `disabled` | the owner |

  Values print as the text, `checked`/`unchecked`, or the index (`-` for
  none). The focused window's root carries `focused`, and so does the
  focused widget inside it.
- **Same path as a click:** an action runs the handler a real click or
  key reaches after hit testing, on the owner's thread (`cargo:lumen-ui`
  checks `press` and a click give the same state, and that every action a
  widget's role lists reaches a handler). Errors change nothing. The
  asker's own come first: no desktop, a node that doesn't exist (or whose
  window closed), wrong arguments (`set_value` takes one, the others
  none), an action a window's root doesn't take, "window <id> busy" (its
  queue is full) and "window <id> closed". The owner's: an action its
  role doesn't list, a `disabled` node, and a node it no longer has. An
  action that ran and then lost its node answers that it ran (`ran; node
  <n> is gone`), so a retry never acts twice.
- **Publish before answering:** a toolkit window's owner hands each event
  to its tree, then (if anything changed) repaints, commits and publishes
  the tree, and only then answers a request (`ui::Toolkit::step`). So once
  `ui invoke` returns, `ui tree` shows its effect, and a window its action
  closed is gone (the request is answered after the handle is dropped).
- **A request is a reply slot** (`lumen_ui::Reply`, its state private),
  each transition made once and checked, under the reply's lock, against
  the state it starts from: `Pending` to `Running` (the owner, when it
  takes the request: `ui::Request::start` gives a `Started`, the only
  thing that can answer it) or `Cancelled` (the asker, at its 1 s
  deadline); `Running` to `Done(result)`; `Done` to `Taken` (the asker
  takes it). Each answer (`Done`) wakes the asker. The owner skips a
  cancelled request (`cancelled`). If the asker's
  swap fails because the owner already runs it, the asker waits a second
  1 s: a result then is printed as "ran late" with the row (`late`
  counts it), and none gives "still running" (its answer, if it comes,
  lands for nobody: not counted). Destroying the window answers its
  `Pending` requests with "window closed"; a request dropped before it ran
  answers itself the same way in its `Drop`, and a `Started` dropped
  unanswered answers "dropped while running" (a bug); an action that
  closed its own window (a Quit button) answers "ran; window <id>
  closed". So `ui invoke` always ends, and a retry after "cancelled"
  can't press twice. The harness's `invoke(m, node, action)` (`gui.py`,
  #156) retries a cancelled request (counting the retries) and takes "ran
  late" with its row, or with its message, as done. `cargo:lumen-ui` checks that a request cancelled at its
  deadline and then reached by the owner leaves the state unchanged.
- **Reading the tree:** a toolkit window publishes a snapshot of its tree
  after each change (`WindowHandle::publish`: built, then swapped in under
  the window's lock), so `ui tree` reads it without asking the owner. A
  window's root's bounds are its client area.
- **In the kernel** (`kernel/src/ui.rs`): `ui::Toolkit` is a toolkit in a
  window: the UI font through `lumen_ui::Text` (`font::Text` at
  `TEXT_PX`, a glyph cache per owner thread: the desktop's one text path),
  the palette from the `Theme` its window was handed, painting,
  committing and publishing after a change, and the window's events handed
  to the tree (the left button's press and release, key presses, focus;
  `FocusLost` and `Lost` cancel an armed press). `ui::Toolkit<A:
  lumen_ui::App>` is one step per event (above, "Publish before
  answering"); `lumen_ui::serve` runs a request on the app (`Ui::invoke`,
  the app's handling of what it reported, then the node's row, laid out
  again, the error, "ran; node gone" or "closed its window"), the same
  code the host tests run. `lumen_ui::Reply` is the state machine; the
  kernel keeps it behind a lock with the asker's wait queue
  (`ui::ReplyCell`). The palette starts from `WindowHandle::theme()` and
  follows every `Theme` event (`ui::palette(&Theme)`, then
  `Ui::invalidate()` repaints the whole tree): a toolkit window's first
  event, `Theme`, is its first draw. The harness's `ui_tree`, `node`, `wait_ui`,
  `invoke`, `parse_invoke` and `desktop_counter` are in `gui.py`.

**The commands** print records, so the pipelines work on them (`where`
compares with `=`). **Records quote** (every records command, from the
shared printer, `lumen_shell::pipe::quoted`): a text cell that is empty,
contains whitespace, or is exactly `-` prints double-quoted (`"` and `\`
escaped by a backslash), as the shell's word splitting reads it back, so
each row splits into its cells (the harness's `parse_records` uses
`shlex`) and `where name = "File name"` works; a bare `-` is always an
absent value. So `cpus`' "not started", `lspci`'s names and drivers,
`irqs`' uses ("local APIC timer", "msix <addr> #0"), `mouse`'s buttons
("left right") and `sound`'s formats print quoted (a `-` meaning none, as
in `kbd events`' mods, is an absent value, unquoted).

- `windows`, one row per window, front to back:

  | Column | Meaning |
  |---|---|
  | `id`, `title` | the window |
  | `x`, `y`, `w`, `h` | its frame, decoration included, in screen pixels |
  | `z` | 0 is the front |
  | `focused`, `minimized` | `yes` or `no` (`minimized` is `no` until #157) |
  | `owner` | the thread that last read its events (its opener's until then) |
  | `lost` | events its queue dropped |
  | `keys` | key events its queue took (#153's scenarios check where keys went with it) |

  Without a desktop it prints `windows: no desktop` and no rows.
- `ui tree [window]`, one row per node, depth first, each window's root
  first (just that window's with an id; an id with no window is an
  error):

  | Column | Meaning |
  |---|---|
  | `id`, `window`, `depth` | the node, its window, 0 for a root |
  | `role`, `name`, `value` | as above (`-` without a value) |
  | `state` | the set flags joined by `,` (`-` for none) |
  | `actions` | joined by `,` (`-` for none) |
  | `x`, `y`, `w`, `h` | its bounds in screen pixels |

  So `ui tree | where role = button` lists the buttons, and the bounds let
  a test mask a node in a golden (the taskbar clock, #157).
- `ui invoke <node> <action> [args]` prints the node's row after the
  action: from the owner's reply, or from the compositor for the actions
  it answers (`focus`, `minimize`, `restore`). A successful `close` prints
  `ui invoke: window <id> closed` (it succeeds only once the window is
  destroyed; an owner that keeps it open gives "window <id> still open" at
  the deadline: `ui invoke: window <id> still open (its close is still
  queued)`, since the owner reads it when it can and the window closes
  then). On an error it prints why. Its other lines: `ui invoke: ran late`
  above the row or message; `ui invoke: ran; window <id> closed`; `ui
  invoke: ran; node <n> is gone`; `ui invoke: window <id> busy (try
  again)`; `ui invoke: window <id> closed` (a widget's window closed as
  it was asked); `ui invoke: no node <n>` (none, or its window closed,
  from the asker or the owner); `ui invoke: no desktop`; `ui invoke:
  cancelled: window <id> didn't take it in 1 s (try again)`; `ui invoke:
  still running (window <id>)`; `ui invoke: window <id> closed before it
  ran`; `ui invoke: dropped while running (window <id>): a bug`; and the
  owner's other errors (`ui invoke: a label doesn't take press`, `the
  node is disabled`) and the asker's (`wrong arguments ...`). `ui tree
  <window>` with no such window says `ui tree: no window <n>`, and without
  a desktop `ui tree: no desktop`.
- **Records with arguments:** today `sysinfo::MakeTable` is `fn() ->
  pipe::Table`, and the ktest `records_come_from_commands_given_no_arguments`
  checks that every records command, given an argument it doesn't take,
  says its usage. The first PR that needs arguments (#151, for `mouse
  events [n]` and `kbd events [n]`; otherwise #153 for `desktop status`)
  makes it `fn(&[String]) -> Option<Result<pipe::Table, String>>`: `None`
  means "not records: run it as a command" (`mouse hold`), and the error
  is the line printed. That ktest then checks both: arguments a command
  takes give its records or run it, and others still give its usage. The
  records commands taking arguments are `mouse events`, `kbd events`,
  `desktop status` and `ui tree [window]`.

**The rule: agents act only through this tree.** The automation commands,
and later the protocol, never inject synthetic mouse or keyboard input: an
action is the widget's own handler, not a fabricated event. The serial
commands are for tests and people. Agents (M8) use the protocol below and
hold an `AppActions` capability (section 8; threat model, section 13).
Tests may still drive QEMU's real input devices over QMP (#149's
confirming steps): that path is the user's, and the guest can't tell it
from a person.

**Later (M8, M9):** the same operations become a protocol on a channel
per app: `get_tree(window)`, `invoke(node, action, args) -> Result`,
`subscribe(changes)`, and app-level semantic actions (`files.copy(src,
dst)`, `editor.replace(range, text)`) an app may declare beside widget
actions; the `AppActions` capability names the app and the allowed
actions. The same tree powers accessibility (a screen reader) and GUI
tests (golden screenshots plus tree assertions). **X-ray mode** is an app
(and an overlay) that reads the `Trace` capability.

### The taskbar clock (#157)

A `label` named `Clock`, of a fixed size, that draws only inside its
bounds. The desktop shell polls the RTC every 1000 ms and redraws the
label when the minute changes, damaging only its bounds (a test checks
the `rects` and `pixels` it adds in `desktop status`). Desktop goldens
mask its bounds (from `ui tree`); `test_taskbar_clock_matches_rtc`
accepts the minute of its first `date` minus 1 s, or polls until the label
and `date` agree.

### Screenshot (#150)

`screenshot` on the serial shell sends the screen as a PNG over serial (to
disk once there are files, M6). Capture and encoding are separate steps
(`kernel/src/screenshot.rs`), so the desktop brings its own source.

**Source.** In text mode the kernel reads the framebuffer once, row by
row, into `lumen_png::encode::Rows` allocated beforehand (3 MiB at
1280x800: nothing allocates under the lock), holding the screen's lock
(`term::read_pixels`), so terminal output, a cursor flip or a pointer
move can't land halfway through (by design; not tested). The mouse
pointer is in the image where the terminal drew it (by design; not
tested: the scenarios run without a tablet). A command holding a
`term::Canvas` draws without that lock: `read_pixels` then says a canvas
held the screen (whether or not it was still drawing: a finished
`mandelbrot` waiting for a key counts), the summary line ends in `, a
command held the screen (a canvas): may be torn`, and the harness
refuses such a capture unless asked not to (`allow_torn`). While the
desktop is shown, `screenshot` reads the back buffer
(`screenshot::capture` asks `desktop::capture`): the compositor fills
`lumen_png::encode::Rows` (allocated by the command) from it right after
a pass, so it never shows the pointer. While the desktop is paused or off,
it reads the text screen as in text mode (`capture_framebuffer`).

**Encoding** (`lumen_png::encode`): RGB8, unfiltered (on the text console
that compressed 16 to 20% smaller than the `Sub` or `Up` filters), zlib
level 6 from `miniz_oxide`, compressed straight into the file's one
`IDAT`. The rows' and the file's buffers are taken with
`try_reserve_exact` (the file doubles as it fills and always keeps the
16 bytes of its end back, so finishing it never allocates), so a heap
that can't grow gives an error line, not a crash; the compressor's own
fixed boxes (about 250 KiB in all, the largest about 83 KiB) are the
exception. The compressor keeps 64 KiB inline and needs about 72 KiB of
stack, more than a thread's 64 KiB default, so the kernel encodes on a
thread with a 256 KiB stack (`sched::run_sized`; a ktest encodes from a
default-stack thread).

**Wire format** (`lumen_png::frame`; the harness's reader is
`lumen_harness/screenshot.py`):

```text
@@png-begin 1280x800 bytes=46835 crc32=e130dd76 lines=521
@@png iVBORw0KGgoAAAANSUhEUgAABQAAAAMgCAIAAADz...    (base64)
...
@@png-end
screenshot: 1280x800, 46835 bytes of PNG (read 9.455 ms, encoded 62.003 ms, sent 30.954 ms)
```

- The begin line: the size in pixels, the PNG file's length in bytes, the
  CRC-32 of the whole file (the algorithm `zlib.crc32` uses; eight
  lowercase hex digits) and how many body lines follow.
- Body lines: `@@png ` and the file in standard base64 (RFC 4648, `=`
  padding), 120 characters (90 bytes) on every line but the last.
- The end line: `@@png-end` alone. Then a summary line (the same size and
  length; the times vary; the warning above when a canvas held the screen)
  and the prompt.

Every frame line starts with `@@png`, which no log line does (they start
with a timestamp). `frame::write` hands out one line at a time from
buffers on the stack and the shell writes each whole under the log's line
lock, so a log line can land between two frame lines but never inside
one. The reader keeps lines by prefix, checks the line count, the line
lengths, the length and the CRC, then decodes the PNG with PIL and checks
it is RGB of the stated size. On a screen shell the command refuses
(base64 on the screen is no use).

Cost, for example (one TCG run on the text console; the times vary): about
9 ms reading the pixels (interrupts off, under the lock), 62 to 64 ms
encoding, 31 to 40 ms sending 47 to 63 KB of PNG. QEMU's UART doesn't pace
output at the baud rate.

## 12. The Claude service (planned, M8)

- **Keychain service:** holds the API key, which is entered at setup and stored
  encrypted on disk. It is never in source, logs, crash reports or agent
  memory. Only the keychain and the API client process can use it; the agent
  runtime asks the client to make requests and never sees the key.
- **API client:** HTTPS (TLS 1.3) to the Anthropic API, using current model IDs
  (load the `claude-api` skill before writing it). It streams responses.
- **Ask Claude bar:** a system-wide input available in every app.
- **Agent runtime and lanes:** each agent session is a *lane* with its own
  process, capability set, snapshot chain and audit-log stream. The panel shows
  lanes running in parallel with live status, pause/resume, per-step undo, and
  "Undo all".
- **Capability prompts:** when an agent needs a capability it lacks, the user
  sees a readable prompt ("Allow *Tidy Downloads* to move files inside
  `~/Downloads` for this session?") and can grant it narrowed, time-limited and
  revocable.

## 13. Threat model for agents

**Assets:** user files, credentials (API key, passwords), network identity,
the integrity of the system and of the audit log, and the user's attention.

**Adversaries and failure modes:**
1. *Prompt injection* in content the agent reads (web pages, documents, file
   names) that tries to make it exfiltrate data or destroy files.
2. *Mistaken model output*: plausible but wrong actions (deleting the wrong
   folder).
3. *A malicious or buggy app* that exposes deceptive automation actions or
   tries to escalate through the agent.
4. *Network attackers* on the path to the API (handled by TLS and certificate
   validation).
5. *A compromised agent process* trying to escape its sandbox.

**Out of scope:** physical attackers, malicious firmware, and a compromised
kernel. The kernel is the trusted computing base.

**Mitigations (design requirements):**
- **Least privilege by construction:** an agent starts with *no* capabilities
  except a channel to the agent runtime. Each grant is narrow (a subtree, one
  host, one app's named actions), time-boxed to the session, and revocable.
- **Human in the loop for new authority:** capability prompts are rendered by
  the system (not by the agent or the app), state the scope in plain words, and
  cannot be clicked by agents (agents have no input-injection path at all).
- **Exfiltration control:** network access only through `Net` capabilities
  that name hosts. Reading secrets and having network access are never granted
  together without an explicit user decision.
- **Tamper-evident audit log:** every agent capability use and action is
  appended to a hash-chained log (each entry contains the hash of the previous
  one). The log lives on a volume the agent has no write capability for, and
  its chain head is periodically shown in the UI and included in snapshots.
- **Undo:** filesystem effects can be reverted through snapshots. App actions
  declare whether they are reversible, and irreversible ones (sending email,
  network POSTs) require confirmation by default.
- **Provenance:** the UI marks content and actions coming from agents, and
  crash or explain flows send data to Claude only with consent.
- **Kill switch:** pause and stop for any lane, effective immediately
  (capabilities revoked, process suspended).

## 14. Observability (M1 and M5)

- **Trace ring buffer (built):** fixed-size binary events (timestamp, CPU,
  pid, kind, 4 arguments) in a lock-free ring that overwrites the oldest
  (`crates/lumen-trace`, host-tested; `kernel/src/trace.rs`, `ktrace!`). Each
  slot is a small seqlock: writers claim it by compare-and-swap and drop
  their event (counted) rather than wait if it is busy, so recording never
  blocks, even from an interrupt or exception handler that interrupted
  another record (simulated in host tests; not exercised in the kernel);
  readers skip events that change while they copy them, so they never see a
  torn event.
  Events are in `seq` order; timestamps can be out of order across nested
  writers. One 4096-event ring per CPU (up to 8); events carry the CPU's
  index, and the boot timeline reads the boot CPU's ring.
  Recorded so far: boot phases, CPU exceptions and device interrupts nothing
  handles (not the timer, spurious interrupts or local APIC errors), log
  lines, thread switches (`switch from=<id> to=<id> (<state>)`, the
  previous thread's state; an idle boot CPU records about 20 a second,
  the serial shell's 100 ms safety-net wakes: section 4, step 4). Later: syscalls, page faults, allocator
  activity. Cost per event is printed by
  `ktest:lumen_kernel::trace::trace_overhead_is_small` (tens of TSC ticks
  under TCG locally; TCG's TSC rate depends on the host, so this is not
  comparable across machines or to cycles). Timestamps are raw TSC ticks
  (the boot timeline converts its phases; the shell and crash reports show them
  raw). The shell's `trace [n]` prints the newest events of the boot CPU's ring
  (a gap line says why an event is missing), and the serial crash report
  includes the last 16 of the reporting CPU's (tested for a page fault and a
  panic, and on an AP).
- **Boot timeline:** boot phases emit trace events, printed as a timeline at
  boot complete (section 4). The fast-boot target is under 1 s from kernel
  entry to the shell, measured in the guest from TSC timestamps (never by
  host wall-clock time in tests). About 140 ms under TCG at M1, most of
  it calibrating the TSC and the timer.
- **Logger:** `log` facade with level and subsystem tags, and on serial a
  timestamp since kernel entry (built). Every emitted log line also records
  a `Log` trace event (level, tag).
- **Learning:** `explain <subsystem>` and a guided tour link each subsystem to
  its source and docs inside the OS (M5+).

## 15. Testing architecture (built)

| Layer | Tool | Where |
|---|---|---|
| Pure logic | `cargo test` on `no_std` crates | `crates/*` |
| Kernel internals | `ktest!` in-kernel tests → TAP over serial → `isa-debug-exit` | anywhere in `kernel/src` |
| Whole system | pytest scenarios: boot QEMU headless, `expect` on serial with timeouts, `send_line`, keys and the tablet over QMP (`press`, `type_text`, `move_pointer`, `click`; `lumen_harness.gui`'s steps wait for each to show exactly in `kbd`'s and `mouse`'s counters before the next), exit codes; run in parallel (pytest-xdist, one worker per CPU) | `tests/scenarios` |
| Screens | QMP `screendump` compared to golden PNG regions | `tests/golden` |
| Codegen | `make audit`: disassembly and LLVM IR checks | `tools/harness/lumen_harness/audit.py` |
| Harness itself | self-tests that the TAP parser, audit and image diff catch failures, and that the QEMU command line, artifact layout, CI wiring and status generation behave | `tests/harness` |
| Feature gate | every feature in `tests/features/*.toml` must pass (CI); `docs/STATUS.md` generated from the same data | `make status-check`, `make status` |

QEMU exit codes: 33 = tests passed, 35 = a test failed, 37 = crash report.
A triple fault exits with 0 (`-no-reboot`) and therefore fails; the power-off
scenario alone accepts 0, with a QMP `SHUTDOWN` event whose reason is
`guest-shutdown`. Each scenario
keeps `serial.log`, `qemu.log` (`-d int,cpu_reset,guest_errors`),
`cmdline.txt`, `environment.txt` (QEMU version, accelerator, OVMF version) and
screenshots when it fails, in `build/test-artifacts/<module>/<test>/`, and CI
uploads them. QEMU records the interrupt
trace only under TCG, so CI (which runs KVM) re-runs failed scenarios under
TCG before uploading, into a separate root (`LUMEN_ARTIFACTS_DIR`,
`build/test-artifacts-tcg` in CI). If the ktest scenario is re-run, its
`ktest.tap` goes there too, so `build/test-results/ktest.tap` (the first
run's, which the job summary was built from) survives.

## 16. Repository layout

```
kernel/            the kernel (bin crate, x86_64-unknown-none)
  src/arch/        HAL; x86_64/ has gdt, idt, irq, context (thread switch), apic, pic, pit, i8042 and cmos (ports), reset (reset and PM1 registers), serial, stacks (registry), cpu, qemu, paging, percpu (GS base)
  src/acpi.rs      ACPI discovery (MADT, HPET, MCFG, FADT and the DSDT's _S5_) through lumen-acpi
  src/pci/         PCI discovery over ECAM (lumen-pci): pci::devices, the registry (BARs, claims), pci::claim, DeviceResources (mod.rs); MSI-X, DeviceResources::msix (msix.rs)
  src/drivers.rs   the driver table and probe_all
  src/sound.rs     the virtio-sound driver (its thread owns the device), beep, play, volume, sound
  src/virtio.rs    the adapter for virtio-drivers: the Hal (DMA runs, bounce buffers), transport, VirtioTransport
  src/power.rs     reboot (0xCF9, 8042, triple fault) and ACPI S5 power-off
  src/interrupts.rs  interrupt controllers: 8259s masked, local APIC, I/O APICs, routing ISA IRQs
  src/serial_in.rs COM1's input by interrupt (IRQ 4) into the queue the serial shell blocks on
  src/keyboard.rs  the PS/2 keyboard: 8042 init, IRQ 1, decoding (pc-keyboard), lumen.keymap
  src/time.rs      TSC and local APIC timer calibration, the boot origin, now_ns, sleep, ticks
  src/smp.rs       starting the other CPUs, run_on
  src/sched.rs     kernel threads: per-CPU run queues, preemption, sleep, wait queues
  src/boot.rs      Limine requests
  src/cmdline.rs   kernel command line (flags, key=value)
  src/sync.rs      SpinLock (interrupts off while held)
  src/klog.rs      logger (levels, subsystem tags, serial timestamps, the ring dmesg reads)
  src/rtc.rs       the CMOS RTC: date and time (taken as UTC) for `date`
  src/fb.rs        framebuffer: pixels, glyphs, the hello screen (draw_header), the crash screen
  src/desktop.rs   the desktop (#153, #154): the compositor thread, windows (open_window, WindowHandle), desktop, windows, the themes (theme), the wallpaper
  src/font.rs      the desktop's anti-aliased text (lumen-font): the UI font parsed once and shared (font::face), font::Text (a thread's glyph cache, drawing glyph by glyph), the golden sample for `font` and its ktest
  src/screenshot.rs  `screenshot`: the screen as a PNG (lumen-png) in a base64 frame on serial
  src/term.rs      the virtual terminals on the framebuffer (lumen-term): their grids, drawing the shown one's changed cells, log lines, the cursor, the canvas
  src/vt.rs        the keyboard's keys to the shown virtual terminal's shell, Alt+F1 to F4
  src/symbols.rs   kernel symbol table (symbolized crash reports)
  src/crash.rs     crash reporter
  src/ktest.rs     in-kernel test registry and runner
  src/shell/       the shells (lumen-shell): serial and the virtual terminals' consoles, the commands, system commands (sysinfo.rs), deliberate faults (crash.rs)
  src/mm/          memory management: frame allocator (frame.rs), page tables (paging.rs), heap (heap.rs), kernel stacks (stack.rs), window mapping (window.rs)
  linker.ld        layout at -2 GiB, .ktests section
crates/            no_std, host-testable libraries (lumen-elf, lumen-mm, lumen-heap, lumen-gfx, lumen-acpi, lumen-pci, lumen-time, lumen-queue, lumen-term, lumen-shell, lumen-uart, lumen-font, lumen-png, lumen-wm, ...)
assets/fonts/      the desktop's UI font (Atkinson Hyperlegible, SIL OFL 1.1) with its licence and source
assets/wallpaper/  the desktop's wallpaper (original, made by tools/wallpaper.py; README.md)
boot/limine.conf   bootloader config template
tools/             fetch-deps.sh, mkiso.sh, wallpaper.py (makes the wallpaper), harness/ (Python package)
tests/             scenarios/, harness/, golden/, features/*.toml
docs/              ARCHITECTURE.md, ROADMAP.md, STATUS.md (generated)
```
