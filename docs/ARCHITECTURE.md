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
| Kernel shape | **Modular monolith** with message-shaped internal interfaces; Linux x86-64 syscall ABI from M6, wrapped in capability-scoped namespaces in M11 | One address space keeps early milestones fast and debuggable. Speaking the Linux x86-64 ABI lets Lumen run unmodified Alpine Linux (`musl`) compilers and runtimes without maintaining custom `libc`, `ld.so`, or runtime forks; per-process VFS mount namespaces and network policies still enforce Principle 2 around agents and sandboxed processes. | Microkernel (puts IPC overhead in front of every milestone); custom ring-3 syscall ABI + custom `libc` (multiplies porting and maintenance cost by an order of magnitude). |
| Test harness | Python (`uv`, pytest) on the host; TAP from the kernel | Fast to write scenarios, mature tooling, and QMP/PIL are easy to use. | Rust `xtask`: one language, but slower iteration on test scripts. |
| Third-party crates & upstream OS code | Prefer a maintained `no_std` crate over writing our own when it does the job: pinned with `=` in the workspace `Cargo.toml` (`Cargo.lock` committed), a PR adding only the crates its issue names (with their own dependencies), each checked first to build for `x86_64-unknown-none` on the pinned stable toolchain, and `make audit` must pass with it linked (no SSE/AVX outside M6's two named FP save/restore stubs, no red zone, the kernel code model). Any PR that adopts a crate or adapts code from an upstream Rust OS (Lumen is MIT: see [ROADMAP.md](ROADMAP.md#reference-rust-oses-and-license-policy-lumen-is-mit)) must explicitly name the elected license (for multi-licensed projects such as ArceOS/Starry, elect `Apache-2.0`, retain any upstream `NOTICE` and license text, and mark modified files as changed; for BSD-3-Clause code such as Starnix, retain copyright and no-endorsement notices; keep MPL-2.0 code such as Asterinas crates in separate MPL-2.0-marked crates/files; never copy GPL-3.0 code) and preserve upstream copyright notices. Pre-baked Alpine Linux `ext4` test images (containing GPL userspace binaries) live only in the untracked `/build/` output tree (tarball cache in `build/deps/alpine-minirootfs/`, assembled image in `build/alpine-rootfs.ext4`) and are never committed to the MIT repository. Kernel crates so far: `limine`, `rustc-demangle`, `noto-sans-mono-bitmap` and `log` (from M0, before this policy; `log` is still `0.4`, to be pinned in a PR of its own), `pc-keyboard` (scancode decoding and layouts, #58, the first under it), `vte` (terminal escape parsing; brings `arrayvec` and `memchr`) and `unicode-width` (character widths), both #59. `rhai` (scripting, #67; integers only, no modules, no time, closures that can't capture (`no_closure`), `curry` disabled; brings `ahash`, `hashbrown`, `allocator-api2`, `equivalent`, `foldhash`, `smallvec`, `smartstring`, `thin-vec`, `num-traits`, `libm`, `bitflags`, `once_cell`, `portable-atomic`, `zerocopy`, `static_assertions`, `cfg-if`, `no-std-compat` and `core-error`, plus `const-random`, whose compile-time random seed stays linked but unused (the kernel sets a fixed hashing seed before any script runs), and the proc macros `rhai_codegen` and `const-random-macro`, which run only at build time). `virtio-drivers` `=0.13.0` (#95: virtqueues and the virtio device protocols; `default-features = false` with `alloc`; brings `safe-mmio` and `thiserror` (`bitflags` and `zerocopy` were already linked through `rhai`), plus the build-time proc macros `enumn`, `thiserror-impl` and `zerocopy-derive`), under the adapter rules in section 7, and `zerocopy` as a direct dependency, pinned to the lockfile's version (`=0.8.63`; the crate turns on `zerocopy`'s `derive` and `alloc` features for the whole kernel by feature unification, which `make audit` covers). `wasmi` `=2.0.1` (#197: the WASM interpreter for M5's apps; `default-features = false` with `validate` and `portable-dispatch`; brings `wasmi_core`, `wasmi_ir`, `wasmi_collections`, `wasmparser` and `spin`; its `libm` and `bitflags` were already linked through `rhai`). `portable-dispatch` is a stack-safety choice: our dependency line's `default-features = false` drops `auto-dispatch`, and wasmi then dispatches by tail calls, each instruction's handler calling the next and counting on LLVM to make those calls jumps, which its build script expects only without debug assertions; the kernel keeps them on, so a long loop overflowed a 64 KiB thread stack in #197 (`ktest:lumen_kernel::app::wasmi_runs_a_long_loop_in_fuel_slices` runs one on such a stack). The loop dispatch keeps the native stack's depth constant however long a module runs (host calls aside), and wasmi's own call depth (1000 frames, on the heap) bounds a module's recursion. Functions are compiled with the module (`CompilationMode::Eager`, section 8's wasmi configuration): lazily, a body's compilation is charged (7 fuel a byte) to the fuel left in the slice of the first call to reach it, and one costing more ends that call with an error that can't be resumed (though wasmi's message calls it resumable); compiling eagerly isn't metered, and keeps the compiled code, about 13 bytes of heap per body byte, for the module's life. `postcard` `=1.1.3` and `serde` `=1.0.229` (the app interface's wire format, section 8, for the wire types #206 and #207 add to `lumen-app-api`: #199's host calls take none; postcard with `default-features = false` and `alloc`, so without its default `heapless-cas`; serde with `default-features = false`, `derive` and `alloc`; they bring `serde_core` and `cobs` (`thiserror` was already linked through `virtio-drivers`), plus the build-time proc macro `serde_derive` (`proc-macro2`, `quote` and `syn` 3, already built for `thiserror-impl`); two `embedded-io` versions are in `Cargo.lock` only, named weakly by postcard's `alloc` feature, and never built). `ktest:lumen_kernel::app::wire::postcard_round_trips_derived_types` pins their encoding and decoding rules and links them into the ktest kernel, where `make audit`'s binary checks cover them; its IR checks read only the kernel crate's own IR, so once the wire types use them they see the generic code of these crates compiled into it, while their own non-generic code is covered by the binary checks alone, as wasmi's and rhai's is. `linux-raw-sys` `=0.12.1` (#232: Linux x86-64 UAPI syscall numbers, `errno`, `ioctl`, `prctl`, `auxvec`, and ABI structs for M6–M10 native ring-3 syscalls, section 8; multi-licensed `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT`, elected license **MIT**; `default-features = false` with `no_std`, `general`, `errno`, `ioctl`, `system`, `prctl` and `auxvec`; zero transitive dependencies; builds on stable `rustc 1.99.1` for `x86_64-unknown-none`). `kernel/src/linux/mod.rs` re-exports its modules and links its M6 syscall table and `new_utsname` helper into both kernels, and `ktest:lumen_kernel::linux::abi_constants_match_spec` pins all 30 M6 syscall numbers, core `errno` values, terminal `ioctl` / `arch_prctl` / `CLOCK_*` constants, and Linux x86-64 struct sizes with `make audit` passing. `ext4plus` `=0.1.0-rc.2` (#248: pure-Rust `no_std + alloc` writable ext4 driver, section 9; multi-licensed `MIT OR Apache-2.0`, elected license **MIT**; `default-features = false` with `sync` and `multi-threaded`; brings target-linked `crc` `3.4.0` (`crc-catalog` `2.5.0`), `spin` `0.10.1` (`lock_api` `0.4.14`, `scopeguard` `1.2.0`), and `async-lock` `3.5.0` (`event-listener` `5.4.2`, `event-listener-strategy` `0.5.4`, `pin-project-lite` `0.2.17` already in `Cargo.lock`; unused at runtime with `sync` enabled), plus `bitflags` `2.13.2` already linked through `rhai`/`virtio-drivers`, and build-time proc macros `async-trait` `0.1.92` and `maybe-async` `0.2.11` (`proc-macro2`, `quote`, `syn`, `unicode-ident`); builds on stable `rustc 1.99.1` for `x86_64-unknown-none` with zero nightly features). `kernel/src/vfs/mod.rs` links `ext4plus` into both kernels via `log_ext4_crate_summary()`, and `ktest:lumen_kernel::vfs::tests::ext4_crate_smoke` exercises mount, read, create, write, truncate, fast symlink, hard link, `mkdir`/`rmdir`, and `unlink` in memory with `make audit` passing. Since #313 it builds from `vendor/ext4plus`: the release's crate file unpacked byte for byte (its sha256 the checksum `Cargo.lock` had), wired in by `[patch.crates-io]`, kept out of the workspace (Lumen's lint, fmt and test commands leave it alone), its licence files kept, with Lumen's fixes on top in commits of their own, their code changes marked `Lumen (#n)` in the source (section 9, "Writable `ext4` driver"); no new crate comes with it. `shlex` (#62) is only a dev-dependency of `lumen-shell`, the word splitter's test oracle; `noline`, which #62 names, isn't used (no completion hook, escape codes always, and it queries the terminal for the cursor on every prompt). | Reach working, playable features sooner; crates with many users are better tested than a first version of our own. | Writing everything ourselves (more to learn, much slower); unpinned versions (a minor release could break the build or bring in SSE). |
| Firmware for tests | OVMF `edk2-stable202605-r1` from rust-osdev/ovmf-prebuilt, pinned with sha256 | macOS and CI boot byte-identical firmware, so golden screenshots match. | Distro OVMF packages (paths and versions differ per OS). |

## 3. Layers

```
 ┌──────────────────────────────────────────────────────────────────────┐
 │ Linux userspace (Alpine musl: busybox, gcc, rustc, go, python, node, │  ring 3
 │ ruby, git, Claude CLI/agent)  +  WASM apps (in-kernel wasmi, M5)     │
 ├──────────────────────────────────────────────────────────────────────┤
 │ Linux x86-64 syscall ABI (M6–M10)  +  Capability namespaces (M11)    │  boundary
 ├──────────────────────────────────────────────────────────────────────┤
 │ Kernel core: scheduler (SMP migration M8), user VM + VMA tree (M6),  │
 │ CoW/TLB shootdown/futex/signals (M8), epoll/AF_UNIX (M9),            │
 │ trace ring buffer + X-ray, crash reporter, logger                    │  kernel
 │ Kernel subsystems: POSIX VFS + writable ext4 + tmpfs/devfs/procfs    │
 │ (M7), block/page cache, BSD sockets/smoltcp (M9), GUI compositor &   │
 │ automation tree (M4), PTYs (M8), drivers (virtio-*, PS/2, NVMe...)   │
 ├──────────────────────────────────────────────────────────────────────┤
 │ HAL (`kernel/src/arch/`): CPU, GDT/TSS, IDT, syscall/SWAPGS,         │
 │ FXSAVE/XSAVE stubs, paging, timers, ports, IPIs                      │
 ├──────────────────────────────────────────────────────────────────────┤
 │ Limine (UEFI) → kernel ELF at 0xffffffff80000000                     │  boot
 └──────────────────────────────────────────────────────────────────────┘
```

M2–M5 run the Lumen shell, the desktop compositor/toolkit, and sandboxed WASM
apps in ring 0 on kernel threads. From M6 onward, native ring-3 processes enter
through the Linux x86-64 syscall ABI and execute unmodified Alpine Linux
(`musl`) binaries, with Lumen's capability namespaces, snapshots, PTY-backed
desktop terminals, X-ray tracing, and userspace Claude service layered on top
in M11; see [ROADMAP.md](ROADMAP.md#how-this-roadmap-is-ordered).

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
   the test kernel). Its interrupts-off receive lock serializes readers
   on any CPU with IRQ 4's drain, preserving queue/UART ordering. ISA IRQ 4 is routed like the
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
   off under the receive lock. The handler releases it before wake/notify,
   and a reader blocks only while neither holds a byte (`lumen_uart::read_next` and
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
   and the screen; type 'help'`). Lumen's built-in console shell stays in the
   kernel as a rescue/inspection shell; Linux user shells (`busybox ash`,
   `bash`) run in ring 3 from M6 (over PTYs in desktop terminal windows from
   M11).

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
stacks: five per CPU (main, #DF, NMI, #MC, #DB), and the rest for threads
and tests (a build-time assert keeps the five per CPU below it). Each CPU's `smp` slot is empty, starting or online; only online CPUs
get IPIs. The in-kernel tests boot with 4 CPUs online.

**Running something on an AP.** `smp::run_on(cpu, job)` puts a `fn()` in the
AP's mailbox and sends it the `WAKEUP` IPI (vector 0x3f, handled by an
EOI, after `mm::tlb::catch_up`: see "Retiring stack pages" below). The idle loop checks the mailbox (and its run queue) with interrupts
off and only then `sti; hlt` (`arch::irq::wait_unless`), so a wakeup can't
land between the check and the halt; the job runs on the idle thread, in
`ap_main`'s frames, and can't block. The shell's `crash <kind> [cpu]` uses it. The scheduler
sends the same IPI when it queues a thread on another CPU (`smp::wake`).

APs handle no I/O APIC interrupts: every input goes to the boot CPU. An AP
starts its local APIC timer once the boot CPU publishes the count, and the
timer's first tick starts its scheduler. Known gap (in M1–M7; M8's
`TLB_SHOOTDOWN` IPI, designed in #336, covers user address spaces only,
section 5, and the kernel's own unmaps keep the retirement below): there
is no TLB shootdown; `unmap` flushes only the CPU that runs it. Mapping
needs none (x86 caches no not-present entries; an AP may grow the heap, or
map a thread's stack, from a thread or a `run_on` job there). The only
pages unmapped are dropped kernel stacks (test stacks, on the boot CPU; a
dead thread's, on its home CPU), the paging ktests' scratch pages (boot
CPU, other CPUs idle) and the rollback of a failed `map_fresh`, which
undoes pages its own CPU just mapped. A dropped stack's pages may be in
other CPUs' TLBs (a thread's spawner wrote its first frame; a
`sched::scope` thread may have borrowed from it; speculative fills), so
its addresses are reused only after every CPU has flushed them:

**Retiring stack pages (#300).** `mm::tlb` numbers each dropped stack's
unmapping as a generation (`retire`, after the unmap) and keeps, per CPU,
the newest generation it has flushed through: a CPU catches up (`catch_up`:
reads the newest generation, reloads CR3, which drops every kernel-window
translation since the kernel's tables set no global bit, then records it)
at each timer tick and each `WAKEUP` IPI (those go on while `sync` spins
with interrupts on; it catches up its own CPU once, at its start). The stack window hands a free
range out only once every counted CPU has flushed through its generation:
`StackWindow::take` takes a `Flushed`, which only `tlb::flushed_through`
makes (`unsafe fn Flushed::vouch`, like `Claims::new`), and a give-back's
`Generation` (neither `Copy` nor `Clone`) only from `unsafe fn
tlb::retire`, whose caller vouches the pages are unmapped (the private
`stack::give_back`, itself `unsafe`); `wait_for` names the generation to
wait for as a number, never a `Generation`. When the lowest fitting range isn't
flushed through yet, `KernelStack::new` calls `sync(generation)`, which
catches up its own CPU, sends `WAKEUP` to each online CPU behind (again on
the next pass if an IPI couldn't be sent), and spins until they have
caught up. It waits only from a thread with interrupts on and no critical
section open (the M8 shootdown's locking rule: a CPU spinning, interrupts
off, on a lock the waiter held could never answer), once the clocks are
calibrated, at most 2 s (warned about, host-timed); after a timeout no
`sync` waits again until every CPU has got past that generation (a
`sync` meanwhile warns again at most every 10 s, and the first one after
they have logs it). Where it
didn't wait, or gave up, the stack takes other addresses (a higher freed
range already flushed, else the untouched rest, which may map a page
table), counted by reason in `mem` (`no-wait`, `gave-up`, `timed-out`,
`raced`: the lowest fitting range changed while `sync` ran, a stack freed
meanwhile joining it at a newer generation or leaving a new lower one),
with the CPUs still behind. Each spawn that needs a flush costs an IPI to
each CPU behind and a wait for its interrupt (microseconds; none when the
CPUs' ticks have flushed already), and every CPU behind reloads CR3 once
at its next tick or `WAKEUP` (catching up to the newest generation, however
many were retired since). The boot CPU
is counted from the start, an AP from `mm::tlb::join` at the top of
`smp::ap_main`, the first thing it does on the kernel's tables and its own
stack (counted as behind everything, then reading the generation,
flushing and recording it): before that it has run only `ap_entry`'s
switch onto its own main stack, never retired, so it uses no translation
of a retired page, and that flush drops whatever its walker cached. CPUs
past the cap are never counted (they never load the kernel's tables). Tested, with a ktest-only switch stopping the
timer's flushes (`tlb::TickFlushesOff`) so only a `WAKEUP` (`sync`'s)
flushes the other CPUs, by
`every_cpu_flushes_through_a_retired_generation` and
`a_reused_stack_is_flushed_from_every_cpu` (another CPU reading the old
stack, then the reused one, sees the new frame; it can't always tell a
missing flush, as the other CPU may not have kept the translation), and
the no-wait fallback by
`a_stack_that_cant_wait_takes_other_addresses_and_counts_it`.

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
power-off ones (`test_shell::test_shutdown_powers_off` and
`test_fs::test_ext4_unsynced_writes_survive_shutdown`) accept QEMU's exit status 0 only with a QMP `SHUTDOWN` event
whose reason is `guest-shutdown` (a triple fault under `-no-reboot` also
exits with 0, but as `guest-reset`).

Both commands first flush every filesystem (`vfs::sync_all`, #352), on the
shell's thread while it can still sleep: `reboot` before `reboot:
restarting`, `shutdown` before `shutdown: powering off` and only when ACPI
gave a way to power off (`shell/commands.rs`'s `sync_then_announce`). A
failed sync is a warning on serial, `WARN  power: <command>: sync failed:
<error>; <restarting|powering off> anyway` (a terminal window's or virtual
terminal's output may never show before the machine goes), and a line on
the console, `<command>: sync failed: <error> (the log says what failed);
<restarting|powering off> anyway`; then the command goes on. The block
cache writes its dirty blocks back in block order and stops at the first
that fails, before the device flush: the blocks ahead of it reached the
device but were never flushed, and it and the ones after it are lost.
Staying up saves nothing: on ext4 that failure makes the volume read-only
(`errors=remount-ro`, section 9), so no later sync writes them; refusing
would let a disk that keeps failing keep the machine from going down at
all. What the
ext4 rootfs holds unsynced is on the disk after either (tested:
`test_fs::test_ext4_unsynced_writes_survive_reboot` and
`test_fs::test_ext4_unsynced_writes_survive_shutdown`, each checking first
that the image is still untouched; the failure's line and warning by a
ktest over a failing sync; not tested: a failing disk, and the sync being
skipped when ACPI gave no way to power off). Nothing stops other threads
first, so what they write after the sync can still be lost. A device that
stops answering doesn't hang the command (each virtio-blk request gives up
after 5 s, and the sync then fails), but a VFS or ext4 lock another thread
never releases would. `exit` (the
`lumen.qemu_exit` test hook), the ktest kernel's exit and the crash
reporter's don't sync: the first two end QEMU as pulling the plug would (a
scenario that wants its writes kept runs `sync` first), and the crash
reporter must take no lock it could deadlock on (`crash.rs`).

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
capped at 2000 ms; section 11), `lumen.theme=<light|dark>` (the
desktop's theme, said at INFO; a name that isn't a theme is warned about
and ignored; section 11) and, in the test kernel,
`lumen.ktest=<prefix>[,<prefix>...]` (run only the ktests whose names start
with one of them, said in a TAP comment; none selected fails the run).

## 5. Memory layout

### Virtual address space (x86-64, 4-level paging)

| Range | Use | Status |
|---|---|---|
| `0x0000_0000_0000_0000 – 0x0000_0000_0000_ffff` | unmapped null guard (first 64 KiB, never mapped in any address space) | built (empty) |
| `0x0000_0000_0001_0000 – 0x0000_7fff_ffff_efff` | user space, per process, from M6 (`USER_MIN` to `USER_MAX`; top canonical page `0x0000_7fff_ffff_f000` stays unmapped as a boundary guard) | built (M6) |
| `0xffff_8000_0000_0000 – +phys size` | higher-half direct map (HHDM) of RAM: usable, bootloader-reclaimable, kernel-and-modules and framebuffer entries; NX, RW- except kernel-and-modules (R--), framebuffer write-combining | built |
| `0xffff_c000_0000_0000 – 0xffff_c0ff_ffff_ffff` | kernel heap (grows up by mapping frames, to 1 GiB below the top; the top GiB holds a ktest scratch page) | built |
| `0xffff_d000_0000_0000 – 0xffff_d0ff_ffff_ffff` | kernel stacks: 64 KiB (the main stack 128 KiB, script threads 1 MiB), **unmapped guard page below every stack**; the top GiB holds a ktest page | built |
| `0xffff_e000_0000_0000 – 0xffff_e0ff_ffff_ffff` | MMIO mappings (uncached, RW-), `map_mmio` | built |
| `0xffff_ffff_8000_0000 – end of image` | kernel image: `.text` R-X, `.rodata` (and `.got`) R--, `.data/.bss` RW-, page-aligned segments | built |

The kernel is linked at -2 GiB because the **kernel code model** requires it
(all symbols fit in sign-extended 32-bit immediates). `make audit` verifies
this along with the absence of SSE/AVX (outside M6's two named user-thread FP
save/restore assembly stubs, section 6), no red-zone use, and the presence of
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
  gets its own; a guard page's window stays inside its `StackSlot`, so it
  can never be mapped; a window handed on after an unmap is only safe once
  every CPU has flushed the pages, `mm::tlb`), and no
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

### Per-process address spaces and user VM (M6 built, #233/#237; file-backed `mmap` designed in #246, its core built in #255 (the in-kernel API: `AddressSpace::mmap_file`, the fault paths, `invalidate_file_from`), the `mmap(fd)` and `msync` syscalls built in #255 too; `truncate`/`ftruncate` (#254's `meta` cluster) and `openat(O_TRUNC)` (#255) call `invalidate_file_from`; M7; CoW `fork` and shootdowns designed in #336, M8, below)

Each user process owns an `AddressSpace` protected by a **sleeping,
interrupt-enabled `SleepMutex`** (not an IRQ-off `SpinLock`—needed both for
M7 blocking file-backed `#PF` I/O and to prevent M8 `TLB_SHOOTDOWN` IPI
deadlocks, below). It holds a dedicated 4-level page-table root (L4 frame,
loaded into `CR3` when switching to one of its threads) and a VMA interval tree
(pure logic in host-tested `crates/lumen-vm`):

- **Upper-half sharing (L4 entries 256–511):** Each 1 TiB kernel window
  (`WINDOW = 1 << 40`) spans two 512 GiB L4 slots. During `mm::init`, before
  any process is created, the kernel pre-allocates L3 tables for all 6 dynamic
  kernel L4 slots—kernel heap (`384`, `385`, including the ktest scratch slot),
  kernel stacks (`416`, `417`), and MMIO (`448`, `449`)—alongside the static
  HHDM (`256..`) and kernel image (`511`) tables already built by `paging`.
  Creating a new `AddressSpace` copies L4 entries 256–511 from the kernel root
  table verbatim, so subsequent kernel heap, stack, and MMIO mappings become
  visible in every address space automatically without patching other L4
  tables. Kernel entries are never marked `USER`; `lumen-mm::paging` extends
  intermediate table entries and `Attrs` with an explicit `user` bit set **only**
  within `L4[0..256]`, and `arch::x86_64::paging` adds a plain `write_cr3`
  helper (`CR4.PCIDE = 0`; avoiding `load_root`'s `CR4.PGE` toggle on every
  context switch).
- **Lower-half layout (L4 entries 0–255, `USER_MIN..USER_MAX`):**
  - `USER_MIN = 0x0000_0000_0001_0000` (64 KiB null guard below).
  - `USER_MAX = 0x0000_7fff_ffff_f000` (top 4 KiB canonical user page unmapped
    so no valid user slice can wrap past the canonical boundary).
  - Static ELF `PT_LOAD` segments map at their `p_vaddr`; PIE binaries map at
    `PIE_BASE = 0x0000_5555_5555_4000`, and M7's dynamic linker (`ld-musl`)
    maps upward at and above `INTERP_BASE = 0x0000_7f00_0000_0000` (disjoint
    from `[..., MMAP_TOP)` and wholly below the user stack's guard page,
    which the loader enforces).
  - `brk` starts page-aligned immediately above the highest loaded main-binary
    ELF segment and tracks a byte-granular cursor upward up to the next VMA
    (capped away from `mmap`, and, for a main binary above `MMAP_TOP`, below
    the user stack's guard page).
  - Anonymous (M6) and file-backed (M7) `mmap` regions allocate top-down below
    `MMAP_TOP = 0x0000_7f00_0000_0000`.
  - Initial user stack top: `USER_STACK_TOP = 0x0000_7fff_f000_0000`, with a
    demand-paged VMA and an unmapped guard page below it. Each address space
    retains its own `stack_bottom`, also used for `[stack]` in maps and fork.
    `lumen_vm::StackSize::from_rlimit` clamps the reservation to 128 KiB..=256 MiB,
    rounds up to a page, and exposes `bytes()`, `bottom()` and Linux's
    `arg_limit()` (the argument budget uses the original, unrounded rlimit).
    The default remains 8 MiB (`USER_STACK_BOTTOM = 0x0000_7fff_ef80_0000`);
    stored RLIMIT_STACK writes arrive in C12.
- **VMA interval tree (`crates/lumen-vm`, host-tested):** tracks non-overlapping
  page-aligned virtual ranges (`start..end`), protection flags (`R`, `W`, `X`,
  or `PROT_NONE`), and backing:
  - `Backing::Anonymous` (M6): demand-zeroed private or shared anonymous pages.
  - `Backing::File(FileMapping { file_id: u64, offset: u64, shared: bool, may_write: bool })`
    (M7, designed in #246): `file_id` is the inode's `lock_id`
    (`user_vm::file_id_of`: unique across filesystems and fixed for the
    inode's life, unlike `ino`), keying the open reference (a `KeptFile`:
    an `OpenGuard`, counted in `open_count`, cloned from the descriptor's
    open file, and the path it was opened by, which `/proc/<pid>/maps`
    shows) held in `AddressSpace::mapped_files` (retained until **zero** VMAs reference
    `file_id`, so splitting via `mprotect`/partial `munmap` never drops it early
    and closing the `fd` after `mmap` keeps the mapping valid per POSIX);
    `offset` is the page-aligned file byte offset at `Vma::start`; `shared`
    distinguishes `MAP_SHARED` (`true`) from `MAP_PRIVATE` CoW (`false`);
    `may_write` (`VM_MAYWRITE`) is `true` for `MAP_PRIVATE` and, for
    `MAP_SHARED`, whether the descriptor is open `O_RDWR` on a mount that
    is writable when `mmap` runs (a read-only mount can't have opened one:
    `EROFS`; an `ext4` volume gone read-only since, section 9's
    `errors=remount-ro`, can, so `mmap` asks the mount: a shared
    `PROT_WRITE` mapping there is `EROFS`, and a read-only one gets no
    `may_write`). `MappedFile::of`, the one way
    to make what `AddressSpace::mmap_file` maps, records it from the
    description (`Sharing` is `Private` or `Shared`, carrying no right of
    its own), and `mmap_file` keeps it only in the new VMA: the
    `mapped_files` entry, the first mapping's, holds no right. In
    `VmaMap::protect`, requesting `PROT_WRITE` on a file VMA with
    `!fm.may_write` returns `VmError::Access` (mapped to `-EACCES` in
    `UserVmError::to_mprotect_errno`), preventing `mprotect` from upgrading an
    `O_RDONLY` or read-only-mount `MAP_SHARED` mapping to writable. A
    shared mapping that got `may_write` before its `ext4` volume went
    read-only keeps it, so `AddressSpace::mprotect` also asks the page
    cache (`page_cache::mapped_read_only`) and refuses `PROT_WRITE` there
    the same way, and its store faults are bus errors (section 9).
  - **Sub-VMA splitting and coalescing invariants for file VMAs (M7):**
    - Splitting `[start, end)` at page-aligned `mid` preserves `offset`,
      `shared`, and `may_write` on the left piece `[start, mid)` and sets the
      right piece `[mid, end)`'s offset to `offset + (mid - start)` (checked
      against `u64` overflow).
    - Adjacent file VMAs `a` and `b` coalesce **only** when
      `a.end == b.start && a.prot == b.prot && a.file_id == b.file_id && a.shared == b.shared && a.may_write == b.may_write && b.offset == a.offset + (a.end - a.start)`.
  - Supports exact-range lookup, top-down free-gap search, `MAP_FIXED`
    unmap-and-replace (and `MAP_FIXED_NOREPLACE` returning `-EEXIST`),
    partial-range `munmap`/`mprotect` sub-VMA splitting (required from M6 for
    `musl` `mallocng` `PROT_NONE` guard pages and from M7 for `ld-musl`
    `PT_GNU_RELRO` and `.so` segment overlays), and adjacent same-attribute
    merging. Every range validates `start >= USER_MIN`, `end <= USER_MAX`, page
    alignment, and no overflow.
- **Demand paging, `#PF` interrupt protocol, `PROT_NONE`, and M7 file-backed CoW / `MAP_SHARED`:**
  `mmap(MAP_ANONYMOUS)` and `brk` growth record VMAs without eagerly allocating
  physical frames. Because `idt.rs` installs all 256 IDT entries as 64-bit
  interrupt gates (`IF = 0` on entry) while `AddressSpace` uses a sleeping
  lock (which blocks via `WaitQueue::block`, requiring `IF = 1`), user
  (`cs & 3 != 0`) and `.lumen_fixup` `#PF`s follow an explicit 4-step
  interrupt protocol:
  1. Read `CR2` into a local while `IF = 0`; panic immediately if `RSVD`
     (error code bit 3) is set.
  2. Assert `frame.rflags & IF != 0` and `irq::depth() == 0` (guaranteed for
     ring-3 `#PF` and by `copy_from_user`'s lock-ordering precondition), then
     re-enable interrupts via `irq::restore(true)` (respecting
     `irq::disabled_for_good()`).
  3. Acquire `AddressSpace`'s sleeping lock, resolve demand paging / M7 file
     I/O & CoW / M8 shootdown, and release the lock.
  4. Call `irq::disable()` (`IF = 0`, `depth == 0`) **before** returning to
     `lumen_isr_common` (keeping `swapgs; iretq` atomic with respect to IRQs)
     or calling `sched::exit()` on a fatal fault.

  **Software PTE bits (`crates/lumen-mm/src/paging.rs`):**
  - `PTE_PROTNONE` (bit 9, M6): marks a non-present leaf (`PRESENT = 0`) whose
    physical frame is retained across `mprotect(..., PROT_NONE)`.
  - `PTE_SHARED` (bit 10, M7, designed in #246): marks a leaf (present or
    `PTE_PROTNONE`) whose physical frame belongs to a unified `PageCache`
    `Arc<CachePage>` pinned in `AddressSpace::pinned_pages: BTreeMap<u64, Arc<CachePage>>`
    (keyed by page-aligned user VA) rather than being exclusively owned by the
    process. `lumen_mm::paging::Mapping` exposes `shared: bool` and
    `dirty: bool` (hardware bit 6), `Attrs::leaf`/`protect` preserve
    `PTE_SHARED` across `Present <-> PTE_PROTNONE`, and `unmap_user_range`,
    `clear_user_lower_half`, and `destroy_user_root` (`free_user_subtree`)
    **never** call `frame::free` on a `PTE_SHARED` leaf; instead the VM core
    removes `va` from `pinned_pages` (releasing the `Arc<CachePage>` pin)
    once the walk has flushed the TLB. Every PTE removal from a live
    address space goes through one helper (`unmap_range_locked`: `munmap`,
    `MAP_FIXED`, `brk` shrink, the ELF loader's rollback,
    `invalidate_file_from`, a stale or copy-on-write leaf); `teardown` and
    `Drop` sweep the whole lower half themselves, after the switch away
    from its `CR3`. A ktest-only recorder (`user_vm::release_order`) checks
    each page's clear, flush and release happen in that order.

  **File-backed `#PF` and coherence protocol (M7, designed in #246):**
  - **EOF and `SIGBUS`:** for a `Backing::File(fm)` VMA, let
    `file_off = fm.offset + (page_va - vma.start)` and `file_size` be the
    inode's current byte length. If `file_off >= align_up_page(file_size)`, the
    fault returns `FaultResult::BusError`: for a ring-3 fault (`cs & 3 != 0`),
    queues `SIGBUS` with the page fault's trap number, error code and CR2.
    If fatal, `UserExit::BusError { rip, cr2 }` retains the diagnostic, one
    `WARN` is logged, and `linux` prints `exit=135 signal=7`; for a kernel `.lumen_fixup` copy, redirects to
    `fixup_rip` (`-EFAULT`) rather than killing the process. The same goes
    for a page `mmap_page` can't hand out: a failed disk read (`Io`, which
    `ext4` logs naming the device, inode and page), a quota or the `ext4`
    hard limit refusing a new page or an unlinked inode (`NoSpc`, `NoEnt`:
    a `WARN` from `mm` for a ring-3 fault, once per streak of the same file
    and error, and only a `debug` line for a kernel user copy); only `NoMem`
    is `OutOfMemory` instead, which for a ring-3 fault logs one `WARN` from
    `mm` (out of memory resolving a user page fault) before the process
    ends as for any fatal fault. For a partial
    final page (`file_off < file_size < file_off + 4096`), bytes past
    `file_size` are zero-filled; writes past `file_size` never extend the file
    (writeback clamps to `min(4096, file_size - page_off)`, and subsequent file
    extension zeroes `[old_size % 4096 .. 4096)` of the cached old final page
    so past-EOF stores never leak into newly extended regions).
  - **Single `writable_maps` invariant:** `CachePage::writable_maps` equals the
    **exact** count of user PTEs currently having
    `(PRESENT == 1 && WRITABLE == 1 && PTE_SHARED == 1)`.
    - Only `#PF` (mapping or promoting a `MAP_SHARED` writable page to
      `PRESENT | WRITABLE | PTE_SHARED`) counts one, by taking a
      `WritableMap` (`MappedPage::map_writable`, section 9) before the PTE
      becomes writable, kept in `AddressSpace::writable_maps` beside it.
    - **Every** transition of a `PTE_SHARED` leaf out of `(PRESENT && WRITABLE)`
      —whether via `mprotect` (to any new prot, including `RW -> RWX` or
      idempotent `RW -> RW` if `WRITABLE` is cleared), `munmap`, `MAP_FIXED`
      replacement, `ftruncate` invalidation, `teardown`, or `Drop`—follows
      the **unmap order**, in this order: (1) clear the leaf or downgrade it
      to `WRITABLE = 0`, reading hardware `PTE_DIRTY` (bit 6) as it goes,
      and marking the page dirty if it was set (`WritableMap::note_pte_dirty`;
      the guard marks it when made and dropped anyway); (2) flush every TLB
      that may still hold the writable entry (`invlpg` on the one CPU in
      M7, a shootdown in M8); (3) only then drop its `WritableMap`, which
      calls `CachePage::dec_writable_maps()`. Writeback clears `dirty` before
      copying a page (section 9), so a decrement before the flush would let
      a store through a stale TLB entry land on a page that looks clean.
  - **Not-present fault (`error_code & 1 == 0`) on `Backing::File`:**
    - `MAP_PRIVATE` write fault (`!fm.shared && is_write`): read the page via
      the inode's `PageCache` into a freshly allocated private frame
      (`frame::alloc`), zero any bytes past `file_size`, and map it with
      `prot_to_attrs(vma.prot)` (`USER | WRITABLE`, preserving `vma.prot.exec()`,
      `PTE_SHARED = 0`).
    - `MAP_PRIVATE` read/exec fault (`!fm.shared && !is_write`): fetch/pin the
      `Arc<CachePage>` from `PageCache`, store a clone in `pinned_pages[page_va]`,
      and map its frame **read-only** (`WRITABLE = 0`, preserving
      `vma.prot.exec()`, `PTE_SHARED = 1`, even when `vma.prot.write()` is true
      so the first write triggers CoW).
    - `MAP_SHARED` fault (`fm.shared`): fetch/pin the `Arc<CachePage>` from
      `PageCache` and store a clone in `pinned_pages[page_va]`. On a write
      fault to a writable VMA, take a `WritableMap` (dirty and counted),
      and map with `prot_to_attrs(vma.prot)` +
      `PTE_SHARED = 1` (`USER | WRITABLE | PTE_SHARED`, preserving
      `vma.prot.exec()`). On a read/exec fault, map with `WRITABLE = 0`,
      preserving `vma.prot.exec()`, and `PTE_SHARED = 1` so the first store
      faults into dirty tracking.
  - **Present write fault (`is_write` on a leaf already mapped):** the
    fault decides by the leaf it walks under Lock 1, not by the error
    code's present bit (another fault may have mapped the page since, and
    a direct caller may pass either).
    - If `vma.prot.write()` is true and the existing PTE is already
      `PRESENT | WRITABLE` (stale TLB, or concurrent M8 thread CoW), flush the
      local TLB (`invlpg`) and return `FaultResult::Resolved`.
    - If `vma.prot.write()` is true and the existing PTE has `PTE_SHARED = 1`
      and `WRITABLE = 0`:
      - For `MAP_PRIVATE`: **CoW split**—look the page up again
        (`mmap_page`, for the file's size now: `BusError` if it is gone),
        allocate a fresh private frame, copy
        the 4 KiB page into it with `MappedPage::copy_to_frame` (zeroing any
        tail past `file_size`; it takes the inode's `PageCache` Lock 5 while
        copying, which the fault, holding Lock 1, may take: section 9),
        replace the PTE with the private frame (`prot_to_attrs(vma.prot)`,
        preserving `vma.prot.exec()`, `PTE_SHARED = 0`), flush the local TLB
        (`invlpg`), and remove `page_va` from `pinned_pages`.
      - For `MAP_SHARED`: **dirty promotion**—look the page up again; if
        the cache has let go of the pinned one (a shrink that didn't
        invalidate mappings), drop that leaf and map the page the file has
        now. Else take a `WritableMap`, upgrade the PTE to
        `prot_to_attrs(vma.prot)` + `PTE_SHARED = 1` (preserving
        `vma.prot.exec()`), and flush the local TLB (`invlpg`).
    - Any other present fault returns `FaultResult::Invalid`.
  - **`mprotect`, `munmap` and `ftruncate` invalidation with `PTE_SHARED`:**
    - `protect_user_range` operates **per leaf**: for every leaf with
      `PTE_SHARED = 1`, it **always** writes `WRITABLE = 0` (uniformly for both
      `MAP_PRIVATE` and `MAP_SHARED`, preserving CoW on `MAP_PRIVATE` and
      write-fault dirty tracking on `MAP_SHARED` without ever wiping hardware
      `PTE_DIRTY` silently), and whenever the prior leaf was
      `(PRESENT && WRITABLE)` follows the unmap order above (propagate
      `PTE_DIRTY` and flush the TLB per leaf; once the walk is done, drop
      the range's `WritableMap`s, each calling `dec_writable_maps()`).
      When `prot` is non-`PROT_NONE` (including write-only `PROT_WRITE`),
      clearing `WRITABLE = 0` keeps `PRESENT = 1` (never turning a writable
      mapping into `PTE_PROTNONE`). Already CoW-split `PTE_SHARED = 0` leaves
      update `WRITABLE` directly from `vma.prot.write()`.
    - Because `fsync`/`fdatasync`/`sync`/`msync` treat a `CachePage` as dirty
      whenever `writable_maps > 0 || dirty` (tested in that order, section
      9), repeated `MAP_SHARED` stores
      followed by repeated `fsync` calls without intervening `munmap` always
      persist to `ext4` without requiring reverse-PTE walks in M7.
    - **Truncate shrink invalidation (`ftruncate`, `truncate`, `openat(O_TRUNC)`):**
      `AddressSpace::invalidate_file_from(file_id, new_size)` (built in
      #255; `truncate` and `ftruncate` call it, `syscall/meta.rs`, #254;
      `openat(O_TRUNC)` too, `syscall/fd.rs`, once the open's truncate
      has returned, #255);
      once the inode's truncate has returned, having dropped the `PageCache`
      entries `>= new_size.div_ceil(4096)` under Lock 4, the syscall takes
      Lock 1 and unmaps all file-VMA PTEs covering
      `file_off >= align_up_page(new_size)` (in the unmap order above:
      per leaf, propagating `PTE_DIRTY`, issuing `invlpg` and freeing a
      private CoW frame; once the walk is done, dropping the range's
      `WritableMap`s and `pinned_pages` entries). The truncate syscall never
      holds Lock 1 and Lock 4 together (the fault path does, Lock 1 then
      Lock 4, through `mmap_page`); a dropped page stays alive, pinned by its
      mapping, until the unmap. `invalidate_file_from` retires its leaves on
      every CPU holding that address space (`tlb::shoot_down`, B5 below).
      Through `invalidate_file_mappings` (B7, "Copy-on-write `fork` and TLB
      shootdowns", below), `truncate`, `ftruncate` and `openat(O_TRUNC)` call
      `invalidate_file_from` on every address space mapping the file, so
      subsequent accesses in any space fault into `BusError` / `SIGBUS` and
      later file growth can't split-brain against stale mapped frames.
    - `msync(addr, len, flags)` (syscall 26) returns `0` immediately on
      `len == 0`, or a `len` within a page of 2^64 (Linux's rounding wraps
      it to 0), after validating page-aligned `addr` and valid `flags`;
      `-ENOMEM` when `addr + len` rounded up overflows, and for a range
      partly unmapped (with `MS_SYNC`, after writing back the mapped
      parts, a writeback error winning);
      succeeds as a no-op on `MAP_PRIVATE`/anonymous VMAs, and for `MAP_SHARED`
      VMAs with `MS_SYNC` writes back each mapped file's dirty pages
      (`VfsInode::sync(data_only)`: through `ext4` and its block cache's
      flush to the device). It reads no `PTE_DIRTY` bit: a page mapped
      writable is dirty through its `writable_maps` count for as long as it
      is (above), so the writeback writes every store made through the
      mapping. `AddressSpace::msync_files` clones an open reference on each
      such file under Lock 1 and the writeback runs after Lock 1 is
      released (Lock 1 before Lock 4 would be in order, but disk writes
      under it would stall the process's faults; Linux drops `mmap_lock`
      around `vfs_fsync_range` likewise). `MS_ASYNC` and `MS_INVALIDATE`
      write nothing.

  `PROT_NONE` VMAs reserve VA space without demand-paging on fault;
  `mprotect(..., PROT_NONE)` on already-populated pages preserves their
  physical frames (`PTE_PROTNONE`, preserving `PTE_SHARED` as well so teardown
  releases cached pins without freeing them). If no frame is available or the
  access violates VMA rights (or hits an unmapped address), the process is
  terminated (M6–M7) or sent `SIGSEGV`/`SIGBUS` (M8). Note on W^X: ELF
  `PT_LOAD` segments enforce W^X at load; `mmap`/`mprotect` support switching
  pages between `RW-` and `R-X` and permit explicit
  `PROT_READ | PROT_WRITE | PROT_EXEC` when a Linux binary requests it (needed
  by Python `ctypes`/`libffi`, Ruby, and JITs).
- **Teardown:** When a process's last thread exits (`exit_group`, running with
  interrupts ON), it switches `CR3` back to `kernel_root()` (detaching its
  CPU), then gives up the process's user of the space (`drop_user`: the
  last user's drop runs `teardown`), extracts and closes its FD table
  outside every process lock (releasing `flock` and POSIX record locks;
  section 8, Exit step 3), and `teardown` removes all VMAs, `pinned_pages` (sweeping `PTE_DIRTY`
  and decrementing `writable_maps` on `WRITABLE | PTE_SHARED` leaves, then
  dropping `Arc<CachePage>` pins; the earlier switch to `kernel_root()` is
  the unmap order's flush step, since no CPU can still store through the
  process's entries), and user page-table mappings (unmapping counted
  leaves and releasing their references, then freeing empty L1/L2/L3
  tables). Direct `teardown` retires `users` to zero, asserting its old
  value is at most one, so `add_user` cannot revive the space. Removed `mapped_files`
  `OpenGuard` entries (from `teardown()`, `munmap()`, `MAP_FIXED`
  replacement or a `brk` shrink) are extracted and dropped **after** releasing
  `AddressSpace::inner` (decrementing `inode.open_count` atomically via an RAII
  guard) so any deferred reclamation when the last open/mapped reference
  disappears never holds `AddressSpace::inner`. `AddressSpace::drop` (which may
  run inside `sched::finish_switch` with interrupts off once no CPU holds its
  `CR3`) runs lock-free: if `!torn_down` (error rollback or ktest), its
  unmap walk first releases each counted leaf's reference and propagates
  `PTE_DIRTY` from cache leaves. With no CPU holding its `CR3`, translations
  are already retired. After the walk, `destroy_user_root` frees only the
  empty page tables and root, then `writable_maps` and `pinned_pages` are
  dropped. `VfsInode::drop` never takes a sleeping lock.
  Kernel upper-half tables (256–511) are untouched.
- **Lock ordering across VM and VFS (M7–M8):** Because `copy_from_user` and
  `copy_to_user` may fault into `#PF` → `AddressSpace` → `PageCache` →
  `BlockCache` → `BlockDevice`, syscall paths (`read`, `write`, `pread64`,
  `pwrite64`, `getdents64`, `stat*`, `readlink*`, etc.) **never** hold any VFS,
  inode, `PageCache`, `BlockCache`, or `BlockDevice` lock across a user-memory
  copy; they transfer via bounded kernel bounce buffers (`IO_CHUNK`, e.g. 4 KiB;
  `getdents64` packs its records in a heap buffer of at most `GETDENTS_MAX`,
  32 KiB, holding only Lock 2 while it fills it).
  See section 9 ("Global lock ordering") for the complete acyclic hierarchy.
- **Cross-CPU TLB shootdowns (M6–M7 single-CPU invariant; M8 below):** In M6
  and M7, every user process is single-threaded and pinned to its home CPU
  (as all threads are in `sched.rs`), so local `invlpg` is exact and no
  other CPU can hold a stale entry for its `CR3`. M8 shares address spaces
  between threads (`clone(CLONE_VM)`), copies them on write (`fork`) and
  migrates threads between CPUs, so every change that takes a right away
  from a mapping shoots it down on every CPU that may cache it: "Copy-on-write
  `fork` and TLB shootdowns" below.

### Copy-on-write `fork` and TLB shootdowns (designed in #336, M8)

Issues (section 8's table, "M8's issues"): B4 (frame reference counts,
copy-on-write), B5 (the shootdown IPI and its call sites), B7 (truncate
reaching every address space), C5 (faults as signals). M8 keeps 4-level
paging, no PCID, and no global bit on any mapping.

**Frame reference counts (B4).** A private user frame (anonymous memory,
a copy-on-write copy of a file page, the loader's copies of an image) can
be mapped by several address spaces after a `fork`. Its count lives in a
per-frame array beside the bitmap allocator: `frame::REFS`, one
`AtomicU32` per 4 KiB frame of the bitmap's physical span (1 MiB per GiB of
span, 512 KiB for a 512 MiB guest), allocated from usable memory by
`frame::init` and reserved by name (`frame refcounts`), so hard rule 2's
check covers it. B4 adds its logic to `lumen-mm` as a new module,
host-tested (`frame::RefTable`: `set_one`, `inc`, `dec -> Remaining`, `get`;
`cargo:lumen-mm`). The count
is the number of user page-table leaves (present or `PTE_PROTNONE`, never
`PTE_SHARED`) naming the frame plus live `UserFrame` tokens, outside
Lock 1's critical sections. Raw paging-ktest frames remain uncounted:
- **Set to 1** when the fault path (or the loader) maps a frame it just
  allocated: `frame::alloc_user() -> Option<UserFrame>`, a zero-filled,
  move-only frame whose count is 1, or `UserFrame::from_owned` after a
  private copy has been filled in an exclusively owned kernel frame.
- **Incremented** by `fork` for each private leaf it copies (and for each
  frame of a `MAP_SHARED | MAP_ANONYMOUS` mapping, below), under the
  parent's Lock 1. A count that would pass `u32::MAX` refuses the `fork`
  (`-ENOMEM`; unreachable with `MAX_STACKS` processes).
- **Decremented** only after the leaf is gone from every TLB: by
  `unmap_range_locked`, the copy-on-write split, `teardown` and
  `AddressSpace::drop`, each after its shootdown (or, for `teardown` and
  `drop`, after no CPU holds the root, below). The decrement that reaches
  0 frees the frame (`frame::free`, which asserts a count of 0).
- Kernel frames (page tables, heap, stacks, DMA) never have a count;
  page-cache pages keep M7's `Arc<CachePage>` pins in `pinned_pages`.

**Copy-on-write `fork` (B4: `AddressSpace::fork_copy(self: &Arc<Self>) ->
Result<Arc<AddressSpace>, UserVmError>`, taking and returning `Arc`s so it
can register the child's `Weak` as a mapper, below).** Under the parent's
Lock 1, with the child new and unshared:
1. Copy the VMA map, the break, `images` and `mapped_files` (each
   `KeptFile` cloned, the child registered as a mapper of each file, below),
   then read each mapped file's size (after its registration: B7's order).
2. Walk the parent's lower half (a leaf of a file-backed VMA at or past
   the size step 1 read for its file is not copied, whatever its kind: the
   child refaults it, `SIGBUS` past EOF):
   - a private leaf (`PTE_SHARED == 0`) of a private VMA: clear `WRITABLE`
     in the parent's leaf if it was set (a copy-on-write leaf is one that
     isn't writable in a writable VMA), map the same frame read-only in the
     child (`PTE_PROTNONE` stays `PTE_PROTNONE`), `inc` its count;
   - a `PTE_SHARED` leaf (a page-cache page, `MAP_SHARED` or a not-yet-split
     `MAP_PRIVATE` file page): not copied; the child faults it in from the
     page cache like any first touch (Linux skips a whole VMA that holds no
     anonymous page, `vma_needs_copy`; Lumen skips each page-cache leaf,
     which faults back the same page);
   - a leaf of a `MAP_SHARED | MAP_ANONYMOUS` VMA (and `/dev/zero`'s shared
     mappings): copied with its rights, `inc` its count, so both processes
     write the same frame. B4 records the sharing in `lumen_vm::Backing`
     (`Anonymous { shared }`), and first populates the parent's missing
     pages of such a VMA with zeroed frames, so a page neither process had
     touched is still shared after the fork. (Linux backs these with a
     shmem inode; that design, with `memfd_create`, is M10's.) The census's
     workloads never map shared anonymous memory.
3. Shoot down the parent's address space (a full flush, below) before the
   child can run, so no CPU still writes through a writable entry the walk
   took away.

A failure part way leaves the parent's leaves read-only, which only costs
a fault each, and drops the child after the parent's Lock 1 is released
(its `drop` decrements what it counted). Any shared anonymous pages
populated before failure remain as zero-filled pages at count 1, equivalent
to demand-zero; a warning reports how many were retained. In a private VMA
(`Backing::Anonymous { shared: false }` or a `MAP_PRIVATE` file),
`mprotect` keeps a leaf whose count is above 1 read-only when it grants
`PROT_WRITE` (M7 sets `WRITABLE` from the VMA directly; Linux's
`can_change_pte_writable` refuses the same); in a shared anonymous VMA it
sets `WRITABLE` from the VMA as M7 does, since those frames are shared on
purpose.

**The copy-on-write split (B4).** A write fault, under Lock 1, on a
private present leaf (`PTE_SHARED == 0`) that isn't writable in a
writable **private** VMA (`Backing::Anonymous { shared: false }` or a
`MAP_PRIVATE` file; a shared anonymous VMA's leaves are always mapped with
the VMA's rights, so they never take this path; section 5's file-page split
for `PTE_SHARED` leaves is unchanged):
- count 1: the frame is this space's alone; set `WRITABLE`, `invlpg`
  locally, `Resolved` (an upgrade needs no shootdown: another CPU's stale
  read-only entry only faults, and the fault finds the leaf writable,
  M7's "already writable" path);
- count above 1: `frame::alloc_owned()` (`OutOfMemory` if none), copy the 4
  KiB, convert with `UserFrame::from_owned`, clear the leaf, shoot the page
  down in this space (`tlb::shoot_down(space, Shoot::Pages(page))`), map the
  copy (writable), then `dec` the old frame's count. Break-before-make (as
  Linux's `wp_page_copy`) ensures no sibling CPU keeps reading stale data from
  a cached old read-only entry after stores land in the new copy, and the
  shootdown comes before the decrement so a CPU still holding the old entry
  never reads a frame the other holder may reuse writable.

Two spaces splitting one frame at once may both copy; the second
decrement then frees it. Only a space that maps a frame can raise its
count (`fork` under that space's Lock 1), so a count of 1 seen under Lock 1
stays 1.

**TLB shootdown (B5, built).** Vector `0x3e` (section 6's vector map
names `idt::TLB_SHOOTDOWN`, handled lock-free by `mm::tlb::on_shootdown_ipi`),
`kernel/src/mm/tlb.rs` beside M1's stack-retire generations. B5 owns the
one entry point, `tlb::shoot_down(space: &AddressSpace, what: Shoot)`
(`pub(in crate::mm)`) with `enum Shoot { Pages(Range<u64>), Full }`.
- **Who may hold an entry:** each `AddressSpace` has `active: AtomicU64`,
  one bit per CPU. Each CPU records the space whose root it has loaded
  (`PerCpu::loaded_space`, a raw pointer kept alive by the running
  thread's `UserThreadState::address_space`). Whatever loads a new root
  (the context switch, `detach_user_space`, `execve`'s swap,
  `UserSpaceGuard::enter`) sets this CPU's bit in the new space before
  the `CR3` write and clears it in the recorded space after, so the bit
  is cleared on the space the CPU actually had loaded. The switch reloads
  `CR3` only when the root changes (`sched.rs` today), so two threads of
  one space switching on a CPU keep the bit set. A kernel or idle thread
  runs on `kernel_root()`, so a CPU running one holds no user entry. An
  exiting thread detaches (section 8, "Exit", step 2) before it can count
  as gone, so once a process's last thread is out, no CPU of its can set
  the bit again.
- **The board:** `SHOOTDOWN: SleepMutex<Board>`, one request at a time
  kernel-wide (section 9's Lock 1a: taken only under the space's Lock 1,
  with no lock below it held (2 through 10, 9a included)). `Board` holds
  only the writer's generation counter; the published request
  (`REQUEST_GEN`, `REQUEST_ROOT`, `REQUEST_START`, `REQUEST_END`,
  `REQUEST_FULL`) and each CPU's acknowledged generation (`ACKED[cpu]`)
  live in module-level atomics the handler reads without a lock.
- **The request:** with the PTEs already changed under Lock 1: a fast path
  flushes this CPU with interrupts off (`invlpg` per page, or a `CR3`
  reload for `Full` or past `FULL_FLUSH_PAGES` = 32 pages, about Linux's
  `tlb_single_page_flush_ceiling` of 33), issues a `SeqCst` fence, then
  reads `active` less this CPU, returning without taking `SHOOTDOWN` when
  no other CPU has the space loaded. Otherwise it acquires `SHOOTDOWN` and:
  (1) with interrupts off (so the thread can't migrate in between), flushes
  this CPU if it migrated, issues a `SeqCst` fence, then reads `active`
  less this CPU, publishes the request with a new generation and sends
  `0x3e` to each CPU left; (2) with interrupts on again, waits until each
  target's `ACKED` reaches the generation or its bit has left `active` (it
  has loaded another root, which flushes every non-global entry). The wait
  spins with interrupts on (`debug_assert!(irq::enabled() && irq::depth()
  == 0)`), so this CPU keeps taking interrupts, shootdowns and M1's
  `WAKEUP` included; it warns at 2 s (host-timed: B5 adds that first line
  to `klog.py`'s `HOST_TIMED`) and every 2 s thereafter with distinct
  non-host-timed text naming the elapsed time and unsent mask, keeping
  waiting since the frames can't be freed before.
- **The handler:** if this CPU's `CR3` is the request's root, flush the
  range (or reload `CR3`); store the generation in `ACKED[cpu]`; EOI. It
  takes no lock.
- **Deadlock freedom:** the waiter holds only `SleepMutex`es (Lock 1, the
  board, maybe Lock 0) with interrupts on, so a target spinning with
  interrupts off on a `SpinLock` waits for a holder that never waits on
  the board or on a `SleepMutex` (section 9), and answers once it lets go.
  A second requester sleeps on the board.
- **Frames and pins after acknowledgements:** every caller collects the
  frames, `Arc<CachePage>` pins and `WritableMap`s its leaves held and
  drops them only after the wait (M7's unmap order: clear, flush, then
  release, with the flush now a shootdown).

**The call sites.** Every change that removes or narrows a leaf of a space
whose `active` has another CPU in it:
- `unmap_range_locked` (`munmap`, `MAP_FIXED` replacement, `brk` shrink,
  the loader's rollback, a stale or copy-on-write leaf), by its range;
- `protect_user_range_with` (`mprotect`) when a leaf loses `WRITABLE`, gains
  NX or becomes `PTE_PROTNONE`; an upgrade needs none;
- `map_elf_image_locked`'s `PageAction::UpdatePerm` (`ProtectPage`), one page;
- `fork_copy`'s write-protect (a full flush) and the copy-on-write split
  (one page);
- section 5's `MAP_PRIVATE` file-page split (one page): the fault replaces
  the `PTE_SHARED` leaf after `MappedPage::copy_to_frame` has released Lock
  5, shoots the page down, and drops the old pin only after the
  acknowledgements;
- `invalidate_file_from`, in each space it reaches (B7, below);
- not `teardown` or `drop`: `teardown` (run by whoever lowers the space's
  `users` to 0, section 8: the last thread of the last process using it,
  or `execve` letting its old space go), after switching its own CPU off
  the space, waits until the space's `active` is empty (a CPU running a
  thread on the space that isn't a counted process user, such as a sibling
  or ktest thread not yet detached) before it frees anything, and `drop`
  runs only once no CPU holds the root.

Tested by `ktest:lumen_kernel::mm::tlb::shootdown_across_cpus` (B5),
`ktest:lumen_kernel::mm::frame::refcount_and_cow_split` (B4),
`ktest:lumen_kernel::mm::user_vm::mprotect_keeps_a_shared_frame_read_only` (B4),
and (future, C11: cross-CPU `fork` write-protect)
`scenario:test_linux_vm::test_cross_cpu_munmap_and_mprotect_shootdown`
(section 15, "M8's tests", says what each checks).

**Truncate reaching every address space (B7, built).** M7's
`invalidate_file_from` works on one space at a time. In M8 each
mappable inode (`Ext4Inode`, `TmpfsInode`) keeps, beside its `pages:
SleepMutex<PageCache>`, `mappers: Mappers` (an opaque wrapper in
`mm/user_vm.rs` over `SpinLock<Vec<Weak<AddressSpace>>>`, section 9's
level 10c), reached with `pages` through one private `mappable_parts(inode)`
downcast in `vfs/page_cache.rs` (which derives `mappable`,
`page_cache_lock` and `page_cache_mappers`), never under the `pages` lock.
`Mappers` is a multiset: `register` prunes dead `Weak`s and pushes,
`unregister_one` removes one matching entry via `Weak::ptr_eq`, and
`snapshot` clones the list without editing it and upgrades each `Weak`
outside the spinlock. Each `mapped_files` entry is a non-`Clone` RAII
`Mapped { kept: KeptFile, space: Weak<AddressSpace> }`:
`AddressSpace::mmap_file` takes `self: &Arc<Self>` and calls
`Mapped::register` when it adds the file's `mapped_files` entry (under
Lock 1, before any fault can map a page of it), and `Mapped::drop` calls
`unregister_one` whenever the entry leaves (`munmap`, `MAP_FIXED`, `brk`,
`teardown`, `Drop`, or a failed `fork_copy`'s child drop); the multiset
prevents a late `Mapped::drop` outside Lock 1 from erasing a concurrent
re-`mmap` of the same file. `fork_copy` (section 8) builds the child's
`mapped_files` through `Mapped::fork_for` (registering the child for each
mapped file under both Lock 1s) before reading the files' sizes, and
copies only **private** leaves below EOF (page-cache leaves are never
copied); a leaf at or past EOF is left unmapped, so the child's access
refaults against the file (`SIGBUS` past EOF). The `mappers` lock orders
the two: either the registration comes first and the truncate's snapshot
holds the child, or the snapshot comes first, after the truncate set the
size, and the size the fork reads after registering is already the new
one. (Linux serializes the same pair on `i_mmap_rwsem`, which
`dup_mmap`'s VMA link and `unmap_mapping_range` both take.) A shrinking
`truncate`, `ftruncate` or `openat(O_TRUNC)`, once the inode's truncate
has returned and with no lock held, calls `invalidate_file_mappings(inode,
new_size)`, which snapshots the list (each `Weak` upgraded outside the
list's lock) and calls `invalidate_file_from(file_id, new_size)` on each
space in turn, each under its own Lock 1 with its own shootdown, so a page
another process mapped from a stale size snapshot is unmapped as M7's rule
unmaps the caller's (a space registered after the snapshot mapped nothing
past the new size: a fault checks the size, and a fork copies only private
leaves below the size it reads after registering). Tested by
`ktest:lumen_kernel::mm::user_vm::truncate_unmaps_every_address_space`,
`ktest:lumen_kernel::mm::user_vm::fork_racing_a_truncate_copies_nothing_past_eof`,
`ktest:lumen_kernel::mm::user_vm::file_vm_writable_maps_balance_through_mprotect_unmap_and_teardown`,
and
`ktest:lumen_kernel::linux::syscall::meta::tests::meta_truncate_and_open_trunc_unmap_a_second_address_space`
(section 15).

**Faults as signals (C5).** Where M6 and M7 end the process
(`UserExit::Fault`, `UserExit::BusError`, `UserExit::NonCanonicalRip`),
M8 queues a synchronous signal to the faulting thread (section 6's table,
`#PF`'s row: `SIGSEGV`, or `SIGBUS` for a `FaultResult::BusError`, with
`si_addr` = `CR2`). With the default action the process dies by that
signal (status `128 + n` in a shell: `exit=135` for `SIGBUS`, as M7
prints already, and `exit=139` for `SIGSEGV`); the M6 and M7 scenarios
matching the old exit lines move to the signal's in C5's PR (section 15).

### Kernel stacks (built in M1)

Every stack the kernel runs on once `kmain` switches stacks (right after
`mm::init`) comes from the allocator
(`kernel/src/mm/stack.rs`; layout and registry in the host-tested
`lumen_mm::stacks`). `KernelStack::new` takes addresses from the stack
window (`StackWindow` holds its token and hands each stack a `StackSlot`:
the stack's own sub-window, which maps it, and its guard page's, kept
inside the slot so no one can map it), with an unmapped guard page below
each stack (slots tile the window upward, so the page above a stack is the
next slot's guard, or the first page of a free range or of the untouched
rest, which becomes a guard when taken), maps fresh frames there through
its own sub-window (RW, NX, write-back; all or nothing, shared with the
heap in `mm/window.rs`) and registers the stack. Dropping one retracts it
from the registry, then unmaps it, frees its frames and gives its slot
back (#300): the slot joins the window's free ranges (up to
`FREE_RANGES`, 256 (1024 from C3, `2 * MAX_STACKS`); adjacent ones joined,
so there is at most one more
than the slots taken), tagged with the generation its pages were retired
at (section 4, "Retiring stack pages"; the newest of a joined range's
parts, which may come back out of order). `new` takes the lowest free range
that fits, carved from its bottom, once every CPU has flushed through its
generation (waiting for that where it can); otherwise (none fits, or the
lowest one's flush can't be waited for) a higher free range already
flushed, else the untouched rest of the window. It asserts the slot's
guard page is unmapped (a reused range's page can become a smaller
stack's guard). So steady-state spawning and exiting reuses
the same addresses and their page tables: before, every stack's addresses
were new and `unmap` frees no table, so about every 30th spawn mapped an
L1 table for good (a frame each; RAM ran out after about 2M spawns). A
failed `new` (out of frames, or the registry full) gives its slot back
too. Each CPU's
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
(in `sched::finish_switch`) a dead thread's joiners' list (its stack goes
back through the page tables and frame allocator; its record is freed when
`sched::register` prunes its `Weak`, once the last reference is gone). For
a joined thread this happens before `join` returns: `join` waits for
`finish_switch` to be done with it (`FINISHING`, #183). The scheduler
reserves room in its queues so the timer never allocates.

## 6. Interrupts, exceptions and crash reports (built)

- One assembler `.rept` loop generates 256 entry stubs, each 16 bytes apart.
  Stubs push a dummy error code where the CPU does not, then the vector, then
  jump to `lumen_isr_common`, which saves all 15 GPRs into a `TrapFrame` and
  calls `lumen_exception_dispatch`.
- **Vector map** (`arch/x86_64/idt.rs`): 0-31 CPU exceptions; 0x20-0x2f the
  8259 PICs, remapped there and masked (only a spurious IRQ 7 or 15 should
  arrive); 0x30-0x3d unused (ktests raise 0x30 and 0x31); 0x3e the
  `TLB_SHOOTDOWN` IPI (M8, B5), handled lock-free by `mm::tlb::on_shootdown_ipi`; 0x3f the `WAKEUP` IPI, whose handler also
  catches up a retired-stack TLB flush (section 4, "Other CPUs"); 0x40 the local APIC
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
  report text with the user's consent (M11).

### Ring 3 entry, `syscall`/`sysretq`, user CPU state and safe copies (designed in #230, built in M6)

M6 brings native ring-3 user threads into the existing x86-64 HAL without
weakening crash reporting or `make audit`.

- **GDT, TSS, `PerCpu` and context switch order:** `arch/x86_64/gdt.rs`
  already places user segments in the exact order `sysretq` requires: `0x08`
  `KERNEL_CODE`, `0x10` `KERNEL_DATA`, `0x18 | 3` `USER_DATA`, `0x20 | 3`
  `USER_CODE`, `0x28` `TSS`. `PerCpu` (`arch/x86_64/percpu.rs`, pointed to by
  `IA32_GS_BASE` in ring 0) moves from `.rodata` into `.data`/`.bss` (via
  `AtomicU64`/`UnsafeCell`) and gains `kernel_rsp: u64` (top of the current
  thread's kernel stack) and `user_rsp_scratch: u64` (scratch slot for the
  `syscall` entry prologue). Because a freshly spawned thread returns from
  `arch::context::switch` into `thread_entry` rather than back into
  `schedule()`, all per-thread hardware state transitions happen **before**
  `arch::context::switch` (with interrupts off):
  1. If `prev` is a user thread, call `lumen_fpu_save(prev.fpu)`.
  2. Switch `CR3`: load `next`'s `AddressSpace` root for a user thread, or
     switch back to `kernel_root()` when switching to a kernel/idle thread (so
     a dying process's L4 frame is never freed while still active in `CR3`).
  3. If `next` is a user thread, write its kernel stack top to `TSS.rsp0` and
     `PerCpu.kernel_rsp`, write `next.fs_base` to `IA32_FS_BASE`, and call
     `lumen_fpu_restore(next.fpu)`.
- **MSRs and `syscall`/`sysretq`:** On the boot CPU (and on each AP in
  `ap_main`, once on kernel tables and guarded stack), the kernel enables
  `IA32_EFER.SCE` (bit 0, keeping `NXE` bit 11), keeps `CR4.FSGSBASE = 0`, and
  sets:
  - `IA32_STAR = ((0x0010u64 | 3) << 48) | (0x0008u64 << 32)`
    (`0x0013_0008_0000_0000`): `syscall` loads `CS = 0x08` (`KERNEL_CODE`) and
    `SS = 0x10` (`KERNEL_DATA`); 64-bit `sysretq` loads
    `SS = STAR[63:48] + 8 = 0x18 | 3` (`USER_DATA`) and
    `CS = STAR[63:48] + 16 = 0x20 | 3` (`USER_CODE`). Encoding RPL=3 in
    `STAR[49:48]` is required on AMD64 CPUs, where `sysretq` preserves
    `STAR[49:48]` verbatim in `SS.sel` instead of forcing RPL=3.
  - `IA32_LSTAR = lumen_syscall_entry`.
  - `IA32_FMASK = 0x0005_7700` (`TF | IF | DF | IOPL | NT | RF | AC`): clears
    interrupts, direction flag, single-step trap, IOPL, nested task, resume
    flag, and SMAP `AC` atomically on entry.
  - `IA32_KERNEL_GS_BASE = 0` initially (holds user GS base while in ring 0;
    holds `&CPUS[index]` while in ring 3).
- **Syscall entry, RFLAGS sanitization and canonical RIP safety:** Because
  `syscall` does not switch `rsp`, `lumen_syscall_entry` executes with
  interrupts off: `swapgs`, saves user `rsp` to `gs:[user_rsp_scratch]`, loads
  `gs:[kernel_rsp]`, builds a full register frame on the kernel stack (saving
  `rcx` = user RIP, `r11` = user RFLAGS, `user_rsp_scratch`, and all GPRs),
  zeros `rbp` (`xor ebp, ebp`) so kernel backtraces terminate cleanly at the
  syscall boundary, re-enables interrupts via `irq::restore(true)` (respecting
  `irq::disabled_for_good()`), and calls the Rust syscall dispatcher. Before
  returning to ring 3, interrupts are disabled and the return frame is
  validated on **both** return paths (`sysretq` and `iretq`):
  - **RFLAGS sanitization:** `(rflags & 0x0024_0CD5) | 0x0202` — retains only
    user-safe flags (`CF | PF | AF | ZF | SF | DF | OF | AC | ID`), forces
    `IF = 1` and reserved bit 1 = 1, and unconditionally clears `TF`, `IOPL`,
    `NT`, `RF`, `VM`, `VIF`, `VIP`, and all other reserved bits so neither
    `sysretq` nor `iretq` (including `rt_sigreturn` in M8) can leak ring-3
    port-I/O privileges or raise ring-0 `#GP`.
  - **Canonical RIP validation:** both `sysretq` and `iretq` raise `#GP(0)` in
    ring 0 (with user `GS_BASE` already loaded after `swapgs`) if the return
    RIP is non-canonical. The kernel therefore checks `rip < USER_MAX`
    **before** `swapgs` or returning: if non-canonical, it never attempts
    `sysretq` or `iretq` and instead terminates the process (M6) or delivers
    `SIGSEGV` (M8). For a valid canonical `rip < USER_MAX`, ordinary syscalls
    return via `swapgs; sysretq` and rewritten register frames (`execve`,
    `fork` child, `rt_sigreturn`) return via `swapgs; iretq` (with
    `USER_CODE`/`USER_DATA`).
  - **Fork RSP:** the SDM's 64-bit IRET checks the new RIP for
    canonicality, not RSP. Do not reject a noncanonical saved RSP before
    IRET: the scenario's stackless child publishes a marker and exits at
    ring 3 with RSP = 1 << 63, showing the entry itself is safe. A later
    noncanonical stack reference is `#SS(0)` per the SDM (QEMU TCG reports
    `#GP`); that fault at first stack use is not part of this scenario.
- **Conditional `SWAPGS` and IST protection (incl. `#DB` / CVE-2018-8897):**
  `lumen_isr_common` inspects the saved `CS` on the stack (`cs & 3 != 0`): if
  the interrupt or exception came from ring 3, it executes `swapgs` on entry
  and `swapgs` before `iretq`. For IST vectors—`#DF`, `NMI`, `#MC`, plus
  **`#DB` (vector 1, `IST_DEBUG = 4`, guarded IST stack registered as
  `StackKind::Debug` in `lumen-mm::stacks` as each CPU's 5th permanent stack;
  `MAX_STACKS` goes to 512 in M8, "Kernel threads")** so a ring-3
  `MOV SS`/`POP SS` + `SYSCALL` with `TF = 1` (CVE-2018-8897) cannot deliver
  `#DB` onto untrusted user `RSP` at `LSTAR`—an exception can arrive in the
  `syscall` entry or exit `swapgs` window with `CS == KERNEL_CODE`. Those IST
  stubs therefore save `rax`/`rcx`/`rdx` first, clear `RFLAGS.AC` if
  `SMAP_ENABLED`, read `rdmsr(IA32_GS_BASE)`, and execute `swapgs` only if it
  is below the kernel half (`< 0xffff_8000_0000_0000`), recording a stack flag
  to undo `swapgs` on exit. Keeping `CR4.FSGSBASE = 0` and enforcing
  `addr < USER_MAX` on any future `ARCH_SET_GS` guarantees user `GS_BASE` can
  never lie in the kernel half.
- **User exceptions do not crash the kernel:** When `lumen_exception_dispatch`
  handles **any** exception with `cs & 3 != 0` (including `#PF`, `#UD`, `#GP`,
  `#DE`, `#SS`, `#AC`, `#MF`, `#XM`, `#DB`, `#BP`—guarding the kernel ktest
  `#BP` resume branch with `cs & 3 == 0`):
  - `#PF` runs the 4-step interrupt protocol and user VM demand-paging / CoW
    handler (section 5); if the fault is invalid, the process exits with a
    fatal fault status (M6) or receives `SIGSEGV` with `si_addr = CR2` (M8),
    logged once at `WARN` with pid, vector, `rip` and `cr2`—never
    `crash::exception`.
  - Every other ring-3 exception likewise terminates only the offending user
    process (M6) or delivers the corresponding POSIX signal (M8).
  - **Deferred `#DB` at `LSTAR` (CVE-2018-8897 handoff):** never delivers
    `SIGTRAP` or calls `sched::exit()` directly on the shared per-CPU
    `IST_DEBUG` stack (where `IF = 0` and `did_swapgs = 1`). Instead, it clears
    `TF` in the saved `RFLAGS` (and clears `DR6`), sets a per-thread
    pending-debug-trap flag, and returns via `iretq` (undoing `swapgs`) back to
    `LSTAR`; `lumen_syscall_entry` then switches immediately onto the thread's
    normal kernel stack, where the syscall dispatcher checks the flag before
    executing the syscall and terminates the process (M6) or delivers `SIGTRAP`
    (M8). Any other exception from ring 0 outside the `.lumen_fixup` table
    remains fatal and goes to `crash::exception` as before.
- **User FP/SSE/AVX state (`FXSAVE64` baseline + optional `XSAVE64`/`XRSTOR64`) and `audit.py`:**
  - Every x86-64 CPU guarantees SSE2 and `FXSAVE64`/`FXRSTOR64`, whereas the
    `XSAVE` CPUID feature and AVX depend on CPUID (and QEMU's default
    `-cpu qemu64` in `lumen_harness/qemu.py` does not expose `XSAVE`/AVX). On
    the boot CPU and each AP (`ap_main`), the kernel clears `CR0.EM` and
    `CR0.TS`, sets `CR0.MP`, **`CR0.NE`** (bit 5, so unmasked ring-3 x87 errors
    raise `#MF` vector 16 rather than legacy IRQ 13), `CR4.OSFXSR` and
    `CR4.OSXMMEXCPT`. Only when CPUID leaf 1 ECX bit 26 (`XSAVE`) is set does
    it enable `CR4.OSXSAVE`, write `XCR0 = 0x3 | (cpuid(0xD, 0).eax & 0x4)`
    (enabling AVX bit 2 only if hardware actually supports it—avoiding `#GP(0)`
    on XSAVE-without-AVX CPUs), and then query `CPUID(0xD, 0).ebx` for the
    active save-area size.
  - Each user thread owns an aligned FPU save buffer: a 512-byte,
    16-byte-aligned `FxSaveArea` (initialized with `FCW = 0x037F`,
    `MXCSR = 0x1F80`) on baseline CPUs, or a 64-byte-aligned CPUID-sized
    `XSaveArea` when `XSAVE64`/`XRSTOR64` is active (reset to clean initial
    state on `execve`). In M8, `rt_sigreturn` restores only what
    `lumen_signal::frame::restore` validated, so a restore can't raise a
    ring-0 `#GP` (section 6, "Signals", designed in #336): an `MXCSR` with a
    bit outside the CPU's `MXCSR_MASK` is refused (`FpError::Mxcsr`, so the
    thread gets `SIGSEGV`), on the FX-only and the XSAVE path alike. Linux
    gets the same `SIGSEGV` from the `#GP` its 64-bit `FXRSTOR`/`XRSTOR` of
    the user frame raises under an exception fixup
    (`restore_fpregs_from_user`; its explicit `MXCSR` check is the ia32
    path's). The crate also refuses the value when the frame's features
    leave SSE out, stricter than Linux;
    `XSTATE_BV` outside the enabled features or a non-zero `XCOMP_BV` is
    refused too (`FpError::Features`, `FpError::Reserved`). Bad software
    metadata in the frame's `_fpx_sw_bytes` (`magic1`, `xstate_size`,
    `extended_size`, `magic2`) is not an error: the restore falls back to the
    legacy FX area alone (`FpFallback`), with the extended state reset, as
    Linux's `check_xstate_in_sigframe` does. Pure kernel threads carry no
    FPU buffer. Because kernel code uses
    no SIMD/x87, user FPU state is saved/restored only on thread context
    switches and signal delivery/return (M8), not on every syscall.
  - **`make audit` exemption (hard rule 3):** `lumen_fpu_save` and
    `lumen_fpu_restore` must be defined via `global_asm!` or
    `#[unsafe(naked)] #[unsafe(no_mangle)]` so both lower to `module asm` with
    **no LLVM IR `define`** (`audit.py`'s `check_ir` stays 100% strict with no
    IR allowlist, LLVM cannot inline them into `sched`, and un-demangled
    `llvm-objdump -d` sees their exact symbol names). `audit.py` exempts
    **only** `fxsave64`/`xsave64` inside `lumen_fpu_save` and
    `fxrstor64`/`xrstor64` inside `lumen_fpu_restore`, keeping `%xmm`/`%ymm`/
    `%zmm`/`%mm`/`%k` registers banned everywhere (including inside those two
    symbols). `tests/harness/test_audit.py` pins a negative test proving that
    `fxsave64`, `xsave64`, or any `%xmm` instruction in any other symbol still
    fails `audit:flags`.
- **Thread-Local Storage (`IA32_FS_BASE`):** Each user thread stores a
  `fs_base: u64` field (default 0), updated by `arch_prctl(ARCH_SET_FS, addr)`
  (validating `addr < USER_MAX`, allowing `0`) and written to `IA32_FS_BASE`
  (`0xC000_0100`) on context switch.
- **Fault-safe user memory copies (`copy_from_user` / `copy_to_user`):**
  Every kernel read/write of user memory goes through `user::copy_from_user` /
  `copy_to_user` / `copy_cstr_from_user`:
  1. **Lock-ordering precondition:** must never be called with any `SpinLock`
     held or interrupts off (`debug_assert!(irq::enabled() && irq::depth() == 0)`),
     so a demand-paging `#PF` inside a copy can never re-lock a held `SpinLock`
     or invert `heap -> paging -> frame`.
  2. **Bounds check:** returns `Ok(())` immediately if `len == 0` (POSIX allows
     zero-length `read`/`write` with a null pointer); otherwise validates
     `start >= USER_MIN` and
     `start.checked_add(len).is_some_and(|end| end <= USER_MAX)` before
     touching memory (explicitly rejecting `u64` overflow and kernel pointers
     with `-EFAULT`).
  3. **Fixup table:** executes a small assembly copy routine whose faulting
     load/store instruction addresses are registered in a read-only
     `.lumen_fixup` linker section (`"a"` flags, `ALIGN(8)` +
     `KEEP(*(.lumen_fixup))` inside `:rodata` in `kernel/linker.ld` so
     `make audit` verifies it is read-only and inside
     `[__kernel_start, __kernel_end)`).
  4. **Fault resolution:** if `#PF` fires in ring 0 with `frame.rip` matching a
     `.lumen_fixup` entry and `CR2` in `USER_MIN..USER_MAX` (with `RSVD == 0`),
     the handler first runs user demand paging / CoW for `CR2`; if the address
     is unmapped or lacks the required permission, it sets
     `frame.rip = fixup_rip` (and clears `RFLAGS.AC` in `frame.rflags` if SMAP
     is active) and resumes, returning `Err(EFAULT)` instead of panicking.
  5. **Optional SMEP/SMAP:** if `cpuid(0).eax >= 7`, boot/AP setup checks
     CPUID 7.0 EBX bit 7 (`SMEP`) and bit 20 (`SMAP`) and enables them in
     `CR4`. Because `stac`/`clac` raise `#UD` on CPUs without SMAP (including
     QEMU's default `-cpu qemu64`) and hardware interrupt gates do not clear
     `RFLAGS.AC`, SMAP code paths are runtime-gated on `SMAP_ENABLED`: the copy
     routine brackets accesses with `stac`/`clac` only when `SMAP_ENABLED`,
     `lumen_isr_common` clears `AC` on entry when `SMAP_ENABLED` (preventing
     `AC = 1` from leaking into `#PF`/IRQs or across `context::switch`), and
     the fixup path clears `AC` in `frame.rflags`.
  6. **Non-faulting user access (designed in #336, M8; added by B2,
     since `futex`, C4, needs them and no B issue owns the copies):**
     - `user::read_u32_nofault(addr) -> Result<u32, Fault>`: a 4-byte,
       4-byte-aligned load;
     - `user::cmpxchg_u32(addr, expected, new) -> Result<u32, Fault>`: a
       `lock cmpxchg` on user memory, returning the value it found;
     - `user::fault_in_writable(addr) -> Result<(), FaultIn>`: resolves a
       write fault at `addr` under Lock 1, as a store there would (demand
       paging, copy-on-write, dirty promotion), and stores nothing, as
       Linux's `fault_in_user_writeable` does (`fixup_user_fault` with
       `FAULT_FLAG_WRITE`); called with no lock held. `FaultIn::Fault`
       covers invalid/unwritable pages; `FaultIn::OutOfMemory` is logged
       and lets callers return `ENOMEM`.

     The first two may run with a `SpinLock` held (a futex bucket) and never
     fault a page in. They check the range as step 2 does, and register
     their one faulting instruction each in a separate read-only section,
     `.lumen_fixup_nofault` (placed and audited as `.lumen_fixup` is: B2
     extends `audit:flags`'s section check to it,
     `harness:test_audit::test_nofault_fixup_section_is_read_only_and_in_rodata`).
     The `#PF` handler looks a ring-0 fault's `rip` up there at its first
     step, only for `CR2` in the user range, with interrupts still off,
     before the step-2 assertion (`irq::depth() == 0`, which a caller holding a bucket lock
     fails), and before it enables interrupts or takes Lock 1: it sets
     `rip` to the fixup (clearing `AC` as step 4 does) and returns, so the
     call answers `Err(Fault)` (Linux's `pagefault_disable` around
     `get_user`/`futex_atomic_cmpxchg_inatomic`). A kernel address is
     `Err(Fault)` from the range check. Callers first refuse bad or
     misaligned addresses with their ABI's `EFAULT`/`EINVAL`; only page
     state is retried after releasing locks and faulting the page in
     (`copy_from_user` to read it, `fault_in_writable` to write it). Tested by
     `ktest:lumen_kernel::user::nofault_access_answers_fault_without_paging_in`
     (unmapped and not-yet-faulted pages: `Fault` for both accesses;
     read-only and copy-on-write pages: `Fault` for the exchange only,
     each under a held `SpinLock`, nothing mapped; then a mapped page's read
     and exchange; `fault_in_writable` on a not-yet-faulted and a
     copy-on-write page, after which the exchange succeeds and the word
     is unchanged by the fault-in).

### Signals: delivery, frames and `rt_sigreturn` (designed in #336, M8)

M6 stores `rt_sigaction`'s table and `rt_sigprocmask`'s mask and delivers
nothing. M8 delivers (C5), interrupts waits (B2, C6) and turns ring-3
faults into signals. The policy and byte images are `lumen-signal`'s
(#339); the kernel copies them to and from user memory and runs the
FPU instructions. Section 8 has the per-process state (`SignalState`) and
the syscalls.

- **Sending:** under the target process's `signal` lock: drop the signal
  if it is ignored and not blocked by every thread that could take it (a
  stop or continue signal still runs `PendingSignals::prepare_signal`); a
  signal in a thread's `real_blocked` (its mask from before an
  `rt_sigtimedwait`) is never dropped as ignored, as Linux's `sig_ignored`
  keeps it;
  `SIGCONT` resumes a stopped group at once (C6, below); otherwise
  `PendingSignals::enqueue` (`Target::Process` for `kill`, `Target::Thread`
  for `tkill`, `tgkill`, a fault and `SIGEV_THREAD_ID`), then pick the
  thread to tell: the target thread, or `select_target` with the process's
  threads as `ThreadCandidate`s and `SignalState::preferred` as the cursor
  (moved to the answer); a `None` answer (every thread blocks it, is
  exiting or stopped, or already has a signal pending) leaves the signal
  pending for the first thread to look. The chosen thread gets `sigpending`
  and `sched::kick` through its `ThreadSignals::thread` (section 8, "One
  interruptible, killable blocking wait"). `SIGKILL`, and a signal whose
  default action kills when it is the action, the chosen thread doesn't
  block it and no thread has it in `real_blocked`, set `group_exit` there
  (Linux's `complete_signal`: `!sigismember(&t->real_blocked, sig)`),
  clear `group_stop`, `stopped` and every thread's `stop_pending` and
  `stop_dequeued`, and give every thread `sigpending`, `fatal_pending` and
  a kick, as `complete_signal` zeroes `group_stop_count` and, for each
  thread, clears `JOBCTL_PENDING_MASK`, sets `SIGKILL` in its queue and
  calls `signal_wake_up(t, 1)`. The rule doesn't look at `exec_waiter`, as
  `complete_signal` doesn't look at `group_exec_task`: a fatal signal sent
  to the process during an `execve` ends the caller too.
  `rt_sigtimedwait` sets the caller's `real_blocked` to its mask for the
  wait and clears it after: musl's `SIGEV_THREAD` helper waits so for a
  signal it leaves at `SIG_DFL`.
- **Where pending signals are checked:** in the syscall-exit hook B2 adds
  (section 8), run on returns to ring 3 with interrupts on, excluding
  `IST_VECTORS` (debug, NMI, double fault and machine check): at the
  end of each syscall (`SyscallExit::new(orig_rax)` as the syscall
  context), before a fork child's first IRET (`lumen_fork_exit_to_user`,
  context `None`), and when `lumen_isr_common` returns to ring 3 from an
  interrupt or exception (context `None`), the timer's preemption
  included. Interrupt returns skip the hook when neither `sigpending` nor
  a deferred debug trap is set. Syscall returns always check once (also
  resolving restart state), then loop while work remains, each iteration one
  hold of `signal` that starts with the kill test and only then dequeues,
  as Linux's `get_signal` tests `SIGNAL_GROUP_EXIT` and `group_exec_task`
  at the top of every iteration under `siglock`. The kill test: a
  `group_exit`, a `SIGKILL` pending for the thread, or `exec_waiter`
  naming another thread (`de_thread`, section 8) ends the thread (section
  8, "Exit"), before any group-stop park; while `exec_waiter` names
  another thread it ends through exit step 2 alone and records no
  `group_exit`, as Linux's `do_group_exit` gives code 0 and kills no one
  while `group_exec_task` is set. The hook releases `signal` before the
  thread ends (the unwind needs no lock held: section 8, "Exit", step 2).
  Otherwise a group stop the thread is counted in parks it (below);
  otherwise `PendingSignals::dequeue(slot, blocked)` takes the next one,
  and its disposition decides: ignored, dropped; default `Term` or `Core`,
  a group exit with `ExitStatus::Killed` through exit step 1, whose own
  hold re-checks (an `exec_waiter` that `de_thread` set after this
  iteration's hold makes the thread end alone, the signal consumed, as
  `do_group_exit`'s re-check under `siglock` does); default `Stop`, a
  group stop (below); a handler, a frame (below), built after the hold,
  so the next iteration's kill test sees whatever `de_thread` or a group
  exit did meanwhile. The loop runs until
  nothing deliverable is left, then clears `sigpending` under the lock;
  the final check with interrupts off (before `sysretq` or `iretq`) loops
  again if a sender set it meanwhile, as Linux's `exit_to_user_mode_loop`.
- **Stop and continue (C6):** a stop signal's dequeue marks the thread
  `stop_dequeued` (`ThreadSignals`, Linux's `JOBCTL_STOP_DEQUEUED`).
  `SIGSTOP` starts the stop in that same hold. `SIGTSTP`, `SIGTTIN` and
  `SIGTTOU` first release `signal` to ask, under `PROCS`, whether the
  process group is orphaned (if so the signal is dropped, as Linux's
  `get_signal`), and start it in a `PROCS` then `signal` hold. The hold
  that would start it refuses, the signal consumed and nothing stopped,
  unless the thread is still `stop_dequeued` (a `SIGCONT` sent in that gap
  clears every thread's mark) and neither `group_exit` nor `exec_waiter`
  is set, as Linux's `do_signal_stop` returns false on
  `!JOBCTL_STOP_DEQUEUED`, `SIGNAL_GROUP_EXIT` or `group_exec_task`; so a
  stop can't start after `de_thread` cleared one and stop the new image.
  Starting it sets `group_stop` (a new generation, and the count of
  threads still to park), marks every thread `stop_pending` (Linux's
  `JOBCTL_STOP_PENDING`) and kicks the other threads; each
  parks on `Process::stop_wait` in a killable wait whose `ready()` reads
  `Process::stopped`, an atomic mirror of `group_stop` written under
  `signal`. A thread's park, or its exit (section 8, "Exit", step 2, as
  Linux's `exit_signals` calls `task_participate_group_stop`), clears its
  mark and lowers the count, once; the one that takes the count to 0
  makes the transition under `PROCS` then `signal`, re-checking that
  `group_stop` still has the generation it counted for:
  `ProcTree::stop(pid, sig)`. Its `StateChange` is acted on
  after the child's `signal` is released, still under `PROCS`: the
  parent's waits woken and `SIGCHLD` (`CLD_STOPPED`) sent under the
  parent's `signal` unless its disposition is `SIG_IGN` or it set
  `SA_NOCLDSTOP`. `SIGCONT`'s send clears `group_stop`, `stopped` and
  every thread's `stop_pending` and `stop_dequeued` (Linux's
  `task_clear_jobctl_pending`) and wakes `stop_wait`, and
  `ProcTree::cont` and its `StateChange` (`CLD_CONTINUED`) run the same
  way.
- **The frame:** `lumen_fpu_save` into the thread's own buffer first, then
  `lumen_signal::frame::build(FrameInput { registers, fp, info, saved_mask,
  blocked, handler, alt_stack, syscall, user_cs, user_ss })`, with
  `user_cs`/`user_ss` the GDT's `USER_CODE`/`USER_DATA` selectors (section
  6's ring-3 entry): `registers` from the syscall or trap frame, `fp` the
  thread's own `FXSAVE`/`XSAVE` bytes, `saved_mask` the thread's
  `saved_mask` (taken: `rt_sigsuspend`, `ppoll`, `pselect6`) or else its
  mask, `handler` the disposition's `lumen_signal::frame::Handler {
  address, flags, restorer, sa_mask }`, converted from the stored
  `KernelSigaction` (`SA_RESETHAND` applied at the dequeue, before the
  build, as the crate requires). `build` applies the
  restart decision to the saved registers. Its `FrameImage` is copied to
  `new_rsp()` with `copy_to_user`, no lock held. On success the kernel
  installs `blocked_during_handler` and `alt_stack_during_handler` under
  the `signal` lock, resets the FPU to its initial state (Linux's
  `fpu__clear_user_states`), loads `entry()` into the user frame (`rip`
  the handler, `rsp` the frame, `rdi` the signal, `rsi` the `siginfo`,
  `rdx` the `ucontext`) and returns through `iretq`, M6's path for a
  rewritten frame, with its final `RFLAGS` mask and canonical-`rip` check.
  A `build` error or a copy that faults is Linux's `force_sigsegv(sig)`:
  if the failed signal was `SIGSEGV` itself, `SIGSEGV`'s action is reset
  to the default and unblocked, so it kills; otherwise `SIGSEGV` is sent
  as `force_sig` sends it, so an unblocked `SIGSEGV` handler (on the
  alternate stack, say) runs. The restart code still in the registers is
  resolved by that delivery, as the crate's docs say; neither the mask
  nor the alternate stack changes.
- **`rt_sigreturn` (15):** the frame starts 8 bytes below the entry `rsp`
  (the handler's `ret` popped `pretcode`). Copy `FRAME_SIZE` bytes, then
  the FP area in the crate's two phases (`FpSnapshot`: 512 bytes, then
  `xstate_size + 4` when `magic1` matches and the size is in
  576..=the CPU's), each with `copy_from_user`. Call
  `frame::restore(bytes, fp, config, alt_stack, entry_rsp)` with
  `RestoreConfig { current_rflags: the user's r11 at entry, user_cs,
  user_ss, mxcsr_mask, enabled_xfeatures: XCR0, xsave_size, xfeature_layout
  }`: `mxcsr_mask` comes from the thread's saved FP image (0xFFBF when
  zero); the AVX layout comes from CPUID leaf 0Dh on each call; XCR0 and
  the configured size come from the boot-selected `fpu::mode()`. A `Restored`: the
  mask and alternate stack installed under the `signal` lock; its
  `ValidatedFp` bytes copied into the thread's aligned buffer and restored
  with its `FpKind` (with an FX-only fallback on an XSAVE CPU, every
  extended feature initialized; with `Xsave`, the enabled features outside
  its mask initialized, as the crate requires); no FP image, the initial
  state; the registers loaded into the user frame; the restart block
  cleared and no syscall context, so its `rax` is never taken for a
  restart code; then `iretq` through the same final checks. A copy fault
  or a `RestoreError` sends `SIGSEGV` with `force_sig`, as Linux's
  `rt_sigreturn` does through `signal_fault`. The `MXCSR`, `XSTATE_BV` and
  `XCOMP_BV` checks are `restore`'s (section 6's user FP state).
- **`sigaltstack`:** `AltStack::configure` with the caller's `rsp`; a
  thread's alternate stack is cleared by `clone(CLONE_VM)` without
  `CLONE_VFORK` and by `execve`.
- **Faults become signals**, keeping `UserExit::Fault`, `NonCanonicalRip`
  and `UserExit::BusError` as diagnostics if fatal. Each is a
  `SigInfo::fault` queued to the faulting thread after the handler has
  turned interrupts on, with Linux's mapping (`arch/x86/kernel/traps.c`).
  A ring-3 `#DB` arrives on the per-CPU `IST_DEBUG` stack, where nothing
  may block: as M6's deferred `#DB` at `LSTAR` does, its handler clears
  `TF` in the saved `RFLAGS`, records the trap in the thread's
  pending-debug-trap `si_code` together with its original `rip` and
  `User`/`Lstar` origin and returns; for a `#DB` from ring 3 (not the
  `LSTAR` case, which the syscall entry checks) it also sends its own CPU
  the `WAKEUP` IPI. Its eventual return on the thread's own kernel stack
  queues `SIGTRAP` with the original trap address and delivers it; the
  interrupted register context belongs to that later return. If a syscall
  arrives before that IPI, a `User` record queues its original address and the
  syscall executes, keeping its result and normal restart decision. A `Lstar`
  record instead reports the syscall's return RIP, rewinds it by two bytes,
  skips dispatch and drops restart context so the handler re-runs that call.
  `AtomicDebugTrap` packs the origin in bit 7 of its si_code byte.

  | Vector | Signal, `si_code`, `si_addr` |
  |---|---|
  | `#DE` (0) | `SIGFPE`, `FPE_INTDIV`, the faulting `rip` |
  | `#DB` (1), and the deferred `#DB` at `LSTAR` | `SIGTRAP`, `TRAP_TRACE` for a single step, else `TRAP_BRKPT` |
  | `#BP` (3) | `SIGTRAP`, `SI_KERNEL` |
  | `#OF` (4), `#BR` (5), `#GP` (13), a non-canonical return `rip` | `SIGSEGV`, `SI_KERNEL`, 0 |
  | coprocessor segment overrun (9) | `SIGFPE`, `SI_KERNEL`, 0 |
  | `#TS` (10) | `SIGSEGV`, `SI_KERNEL`, 0 |
  | `#UD` (6) | `SIGILL`, `ILL_ILLOPN`, the faulting `rip` |
  | `#NP` (11), `#SS` (12) | `SIGBUS`, `SI_KERNEL`, 0 |
  | `#PF` (14) | `SIGSEGV`, `SEGV_MAPERR` (no VMA) or `SEGV_ACCERR`, `CR2`; `SIGBUS`, `BUS_ADRERR`, `CR2` for `FaultResult::BusError`; `SIGKILL` for `OutOfMemory` (a deviation: Linux runs its out-of-memory killer and retries the fault) |
  | `#MF` (16), `#XM` (19) | `SIGFPE`, the code from the unmasked exception bits (Linux's `fpu__exception_code`), the faulting `rip`; no signal if no unmasked exception bit is set |
  | `#AC` (17) | `SIGBUS`, `BUS_ADRALN`, 0 |

  A fault's signal that is blocked or ignored is unblocked and reset to
  the default first, so it kills, as Linux's `force_sig_info_to_task`
  does. The one `WARN` M6 logs for a fatal ring-3 fault stays, for a
  fault whose signal kills the process.
- Tested by `cargo:lumen-signal`,
  `scenario:test_linux_proc::test_signals_and_sigaltstack`
  and `scenario:test_linux_proc::test_sigchld_and_sigint_interrupt_wait`
  (section 15, "M8's tests", says what each checks).

### Kernel threads (built in M2)

`kernel/src/sched.rs`. A spawned thread has its own 64 KiB kernel stack
(`run_sized` runs a closure in a thread with another size and waits for
it, so the closure may borrow: the script threads' 1 MiB; `scope(|s| ..)`,
#157, the same for several threads: `Scope::spawn_on(cpu, name, f)`, `f`
borrowing from the caller, every thread joined before `scope` returns,
`Scope` invariant in its lifetime so no thread can borrow a local of the
closure)
from `mm::stack` (a guard page below it, hard rule 4; registered as a
`thread` stack), its saved `rsp`, and a state: ready, running, sleeping,
blocked or dead. (`main` and the APs' idle threads run on stacks they don't
own.) Its name is a `TaskName` protected by one leaf `SpinLock`: raw Linux
comm bytes (15 bytes plus NUL, zero-filled to 16) for `prctl`, and a sanitized
`lumen_trace::ThreadName` (#276) for display. Spawn initializes both from its
display name; fork copies the whole `TaskName`. Only the owner changes it:
exec's `set_current_name` replaces both from the path's raw last component, while
`PR_SET_NAME` copies at most 15 user bytes, adds a NUL, then
`TaskName::from_comm` normalizes the terminator and trailing zeros, builds the
sanitized display, and `set_current_comm` replaces both under the
same lock. `PR_GET_NAME` returns the raw bytes, including non-UTF-8 and
control bytes. The crash report's `thread:` line reads the display through
`try_lock`, falling back to `?` if busy, without blocking or allocating.
The displayed name is at most 15 bytes, a
longer name cut to the longest prefix ending on a character, then each control character (C0, DEL,
C1) and U+2028/U+2029 replaced by one `?`. Ordinary spawn functions take any
`Into<ThreadName>` (a `&str`). The cut is for names from outside the source
(`linux /path`, section 9): every name written as a literal at a call
that names a thread fits (`tests/harness/test_thread_names.py`; names
kept in a table or a variable aren't read there, and `From<&str>`, the
spawn functions' conversion, refuses one that wouldn't fit with a debug
assertion), and an app's name is at most 11 bytes (section 8). `sched::spawn(name, f)` starts one on this CPU and `spawn_on(cpu, ..)`
on another; both return a `JoinHandle`, whose `join` blocks until the
thread has finished and its CPU's `finish_switch` is done with it (its
stack and joiners' list freed, its reference dropped), so nothing that CPU
frees for it lands after `join` returns (#183; its record goes when a later
spawn prunes it). `spawn` and `spawn_on` keep kernel threads pinned;
unpinned user threads (roots, and fork children within the inherited mask)
and `spawn_migratable` threads may move between allowed CPUs
(M8 B6, "SMP migration" below). Each CPU has a round-robin run queue.
Only a Ready thread with its full context saved (`on_cpu` clear) moves;
source and destination queue locks serialize placement. A wake from any
CPU queues the thread on its current queue, then redirects it if affinity
requires, after its outgoing save completes. `WAKEUP` IPIs wake halted
CPUs (the IPI is tested; how soon a thread runs depends on the host).

- **Switching** (`arch::context::switch`, naked asm): pushes the
  callee-saved registers (rbx, rbp, r12-r15), saves `rsp`, loads the next
  thread's and pops its registers. Kernel threads carry no SSE/AVX state
  (`make audit` proves kernel code uses none; M6 user threads save/restore
  FP/SSE state around `switch` via the two allowlisted assembly stubs above).
  It happens only with interrupts
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
  thread at nice 0 runs 10 ticks (`SLICE_TICKS`), scaled by its Linux nice
  weight and clamped to 1..=100, before the next ready thread on its
  CPU gets a turn (the idle thread gives way at once when the wakeup IPI
  ends its halt, or at the next tick while it runs a `run_on` job); the
  timer interrupt (after its EOI) switches, and the preempted thread later
  returns through the handler's `iretq`. Slices are counted in the CPU's
  own ticks (a ktest checks two threads' turns 20 ticks apart). Under TCG
  ticks arrive late, so slices are longer in time; tests bound waits by
  progress. The timer never allocates: each CPU's queues have room for
  every live thread, regardless of its placement (checked by ktests and
  asserted on every push). Each spawn reserves ready and sleeper slots
  on all eight CPU slots before constructing or publishing its thread;
  allocation failure returns `SpawnError::NoMemory`, and a reservation
  guard rolls back counts on failure.
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
  The other places that can't block (`run_sized`, `sched::scope`, `sched::sleep`, a
  virtio doorbell's wait) refuse or spin instead, and the stop NMI reaches
  them there. And
  `time::sleep` busy-waits (tested with a lock held, interrupts off, and
  on an idle thread; before the scheduler runs on the CPU, not tested).
  M8 adds `WaitQueue::wait_interruptible` and `wait_killable`, which a
  signal (for `wait_killable`, a fatal one: `fatal_pending`) or the
  thread's group exit also ends, and `sched::kick` (section
  8, "One interruptible, killable blocking wait", designed in #336).
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
  spawner's entry), which then wakes the joiners, frees their list and
  drops its reference; only once it is done (`FINISHING`) does `join`
  return.
- **Observability:** every switch records a `switch` trace event (previous
  and next thread's ids, the previous thread's state), and the serial crash
  report names the running thread (`thread: <name> (id N)`).
- **Accounting:** `top` samples per-thread timer ticks; C12 supplies the
  user/system split and syscall-visible tick counts below.

**SMP migration, priorities and affinity (designed in #336, M8; B6).**
M8's threads share address spaces and its gate runs parallel builds, so
threads move to idle CPUs:

- **What changes:** a thread's home CPU (`Thread::cpu`, an atomic: the run
  queue it is on, or the CPU it last ran on) can change while it is
  `Ready`, and its saved registers are then touched by the CPU that runs
  it next. `Thread::on_cpu` (set from when a CPU switches to the thread
  until that CPU's `finish_switch` after switching away, Linux's
  `task_on_cpu`) keeps a thread whose registers aren't saved yet from
  being taken: only a `Ready` thread with `on_cpu` clear moves. A thread
  that is running, sleeping, blocked or dead never moves. A wake locks its
  current queue, removes its old deadline, and queues it there. If affinity
  excludes that CPU, it moves to the lowest allowed online CPU once its
  save is complete: immediately when `on_cpu` is clear, otherwise in
  `finish_switch`. Timer-expired sleepers are routed after dropping the
  old queue lock (`route_ready`), including deadline waits also on a
  WaitQueue. The destination receives `WAKEUP`. A dead thread's stack
  is still freed by the next thread on the CPU it died on.
- **Work stealing:** an idle CPU, in `idle_loop` before it halts, and
  every CPU every `BALANCE_TICKS` (100) ticks, looks for the online CPU
  with the most queued threads; under both queue locks it compares
  runnable counts (queued plus a running non-idle thread). If the donor
  has at least two more runnable threads, it takes the ready thread
  queued last there that may run on it
  (affinity) and isn't `on_cpu`, holding both run queues' locks in
  ascending CPU order, and records an X-ray `runq` event on both. The
  next donor is tried if the busiest queue has no eligible thread or no
  longer meets the threshold. The timer must not allocate (section 5's heap
  rule), so each spawn reserves a ready slot and a sleeper slot in every CPU's queues, not only its
  home's: neither migration nor a timer tick grows either queue.
  `Cpu::reserved` counts live threads plus pending spawn reservations.
  `try_reserve` fails with `NoMemory` before `Thread::spawned`; the guard
  rolls back all counts acquired so far. Fork maps that error to `ENOMEM`.
- **Priorities:** `Thread::nice` (-20..=19, 0 at spawn, inherited by
  `fork` and `clone`; `setpriority`) scales the slice: `SLICE_TICKS` (10)
  times the thread's weight over nice 0's, from Linux's
  `sched_prio_to_weight` table (1024 at nice 0, each step about 1.25
  times), clamped to 1..=100 ticks. Each CPU remains round-robin:
  a thread of higher priority runs longer when its turn comes, not sooner.
- **Affinity and immutable pins:** `Thread::affinity` is a CPU mask.
  `spawn` and `spawn_on` install a permanent pin and a one-CPU mask, so
  idle threads (never queued), `run_on` jobs, scoped threads, `perf burn`,
  X-ray spin workers and driver threads keep their CPU. `set_affinity`
  refuses all changes to pinned threads (`AffinityError::Pinned`) and
  masks with no online CPU (`NoOnlineCpu`); C12 maps both to `EINVAL`.
  `spawn_migratable` and root `spawn_user_on` threads start with all
  `MAX_CPUS` bits; target selection intersects the stored mask with online
  CPUs. `spawn_user_pinned_on` pins CPU-local test programs and DR0 probes
  at construction, before they can run. `spawn_fork_child` inherits the
  parent's mask and nice with interrupts off, along with its FPU snapshot:
  it starts on the caller's CPU if allowed, otherwise the lowest allowed
  online CPU. A running thread excluded by a new mask leaves at its next
  switch, forced by the next timer tick even if its slice remains; a blocked
  or sleeping thread moves only after waking.
- **Serial-console readers:** roots and children may read on any CPU.
  `serial_in::RECEIVE`, an interrupts-off lock, serializes queue consumers
  and their UART fallback with IRQ 4's drain. The handler releases it
  before waking waiters or notifying poll subscribers; readiness callbacks
  can take it without reversing the wait-queue lock order. Serial roots
  therefore need no CPU-0 affinity restriction.
- **Per-thread CPU state on migration:** a user thread's `fs_base`, FPU
  state, `TSS.rsp0` and `PerCpu.kernel_rsp` are already loaded at every
  switch to it from its own fields (the switch order above), and saved at
  every switch away, so moving needs only that the save on the old CPU
  finishes before the restore on the new one, which `on_cpu` ensures.
  `CR3` is loaded when the root changes, with the space's `active` mask
  (section 5). Code that reads `percpu::index()` twice and needs one
  answer runs pinned or with interrupts off, as before.
- **Tick counts (C12):** section 8's model.
- Tested by `ktest:lumen_kernel::sched::threads_migrate_to_idle_cpus`,
  `ktest:lumen_kernel::sched::balancing_skips_pinned_donor_and_moves_user_roots`
  (a pinned busiest donor cannot hide movable user roots on another CPU),
  `ktest:lumen_kernel::sched::pinned_threads_never_migrate`,
  `ktest:lumen_kernel::sched::nice_scales_slices`,
  `ktest:lumen_kernel::sched::migration_keeps_user_state`,
  `ktest:lumen_kernel::sched::automatic_balancer_moves_runnable_threads`,
  `ktest:lumen_kernel::sched::sleeping_deadline_follows_affinity`,
  `ktest:lumen_kernel::sched::wait_deadline_follows_affinity`,
  `ktest:lumen_kernel::sched::affinity_refuses_immutable_pins`,
  `ktest:lumen_kernel::sched::queue_reservations_fail_and_roll_back`,
  `ktest:lumen_kernel::linux::process::fork_inherits_affinity_and_nice`,
  `ktest:lumen_kernel::serial_in::reads_queued_input_on_non_boot_cpu`,
  and `scenario:test_smp::test_multithreaded_workload_uses_all_cpus` (C11:
  threads pinned to each CPU, confirmed by `getcpu`, each make progress;
  section 15, "M8's tests", says what each checks).

**`MAX_STACKS` (C3).** The stack registry's 128 slots hold five
permanent stacks per CPU (40 at `MAX_CPUS` = 8) and every thread's
stack. Each user thread has a kernel stack, and the gates run tens of
processes and threads at once (make, the compiler driver and its
subprocesses, git, libc-test's thread tests), so M8 raises `MAX_STACKS` to
512 (472 threads past the permanent stacks, at most 30 MiB of 64 KiB
stacks); the stack window's free-range list already holds `2 *
MAX_STACKS`. User threads may take all but `KERNEL_STACK_RESERVE` (32) of
the slots: `spawn_user_on` refuses past that, so a `fork` or `clone` that
can't have a stack is `-EAGAIN` while kernel threads (drivers, the
timers thread, the tty input threads) can still start. `smp.rs`'s
build-time check (`5 * MAX_CPUS < MAX_STACKS`) stays.

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
  1280x800; the shadow and the diff are `lumen_term::render::Renderer`'s,
  shared with the terminal windows, #155): under the screen's lock, the
  cursor is taken off, a pending scroll moves the pixels
  (`Framebuffer::move_rows`) and the shadow (a copy of every cell drawn)
  together, and in each marked row only the cells that
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
pipelines, `pipe.rs`; the deliberate faults, `crash.rs`; `trace runq` and
`trace xray`, `xray.rs`).

- **Five shells, one per console,** each with its own line and history
  (and one more per terminal window on the desktop, #155: `term-shell`,
  section 11):
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
  `shell-crash`), `trace [n]` (this CPU's main ring), `trace runq [park <n> | spin <n> | spin stop]`
  and `trace xray <cpu> [since]` (section 14, X-ray rings), `heap`, `ticks`, `kbd`, `term [text [window]]`
  (a terminal window's rows with its id, #155), `terminal` (section 11),
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
    leaves it idle and checks both), and the threads currently placed there
    (`sched::threads`, a registry of weak references); then CPUs stuck
    starting, and in the header those halted past `MAX_CPUS` (neither
    tested). An idle boot CPU reads nearly 100% busy only where the serial
    shell polls serial: without COM1's interrupt (`serial_in`, #120).
  - `mem`: frames used of those the allocator owns, the heap's counters,
    the kernel stacks registered, by kind (sizes without their guard
    pages), and a `reuse` line: the stacks that took other addresses than
    the lowest freed range that fits, by reason (`no-wait`, `gave-up`,
    `timed-out`, `raced`; section 4, "Retiring stack pages"), and the CPUs
    still behind the TLB flushes (the pipeline table leaves it out).
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
    Ctrl-C (or after `frames` frames). The scenario checks frames at least
    0.9 s apart, each CPU ticking, and the CPUs' ticks a second together
    within 3 of the timers' interrupts a second (each interrupt is a tick;
    the timers programmed for 1 kHz are two ktests'). With escape codes each
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
  wide as the widest value, a left-aligned last one unpadded; a text cell
  that wouldn't read back as one word is double-quoted: section 11,
  "Records quote"). Alone, a
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
  records, redirection, variables, and running Linux shells (`busybox ash`,
  `bash`) in ring 3 (M6–M8; over PTYs in desktop terminal windows in M11).
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
  <name>` runs one, until M7 brings files. A value other than `()` is
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
- **Not yet:** scripts from files (M7), floats, capturing closures,
  a script running in the background.

## 7. Driver interface (M3 built: PCI, virtio adapter, MSI-X, virtio-rng, tablet, sound; `InputDevice` M4 built; `BlockDevice` designed in #246, M7)

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
  IOMMU a device can write any physical memory. Moving a driver out of the
  kernel later would require explicit MMIO/IRQ/DMA isolation and an IOMMU.

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
    HHDM) can come with networking (M9) if profiling asks for it; rng's
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

### Device classes (`InputDevice` built in M4; `BlockDevice` designed in #246, M7; `NetDevice` M9)

`BlockDevice` (M7, designed in #246), `NetDevice` (send/receive frames, M9),
`InputDevice` (event stream: the keyboard and the tablet, designed in #146 and
built in #151), `Display` (framebuffer + flush rectangles), `Console`: traits
whose methods are the subsystem boundaries. `Display` stays later: M4 has one
display (the GOP framebuffer Limine hands over) and one consumer at a time (the
terminals or the compositor, section 11), so it becomes a trait with a second
display (virtio-gpu, or real hardware in M12).

**`BlockDevice` and `virtio-blk` (designed in #246, M7).** Defined in the
host-tested `crates/lumen-bcache` crate so both host block-cache tests
(`MemBlockDevice`, fault/crash injection) and the kernel's `virtio-blk` driver
implement the exact same synchronous, blocking interface (callers always run on
preemptible kernel threads with interrupts enabled, never holding a `SpinLock`):

```rust
pub const SECTOR_SIZE: usize = 512;
pub const BLOCK_SIZE: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockInfo {
    pub sector_size: u32,       // 512 for virtio-blk baseline
    pub sector_count: u64,      // capacity in 512-byte sectors (> 0)
    pub read_only: bool,        // VIRTIO_BLK_F_RO
    pub flush_supported: bool,  // VIRTIO_BLK_F_FLUSH
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockError {
    OutOfRange,     // lba + sectors > sector_count or overflow
    Unaligned,      // buf.len() == 0 or !buf.len().is_multiple_of(sector_size)
    ReadOnly,       // write to a read-only device
    CacheFull,      // all BlockCache slots are pinned (no evictable slot)
    Io,             // device returned VIRTIO_BLK_S_IOERR / UNSUPP / short used_len
    Timeout,        // bounded MSI-X wait expired (device marked failed_after_bind)
    DeviceFailed,   // device was already retired after a prior fatal error
}

pub trait BlockDevice: Send + Sync {
    fn info(&self) -> BlockInfo;
    fn read_sectors(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError>;
    fn write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError>;
    fn flush(&self) -> Result<(), BlockError>;
}
```

- **`virtio-blk` driver (`kernel/src/blk/virtio_blk.rs`, added to `DRIVERS`):**
  - **Bounded multi-device slots (`MAX_BLK_DEVICES = 4`):** because
    `virtio::transport(res, handler: fn())` takes a zero-capture `fn()`, the
    driver maintains a fixed pool of `MAX_BLK_DEVICES = 4` slots with static
    `[Doorbell; MAX_BLK_DEVICES]` and const-generic `fn on_irq<const I: usize>()`
    trampolines. Probe claims a free slot before calling `virtio::transport`
    (releasing the slot on failure) and declines devices past `MAX_BLK_DEVICES`
    with `ProbeError::NotMine` before calling `virtio::transport` so no MSI-X
    vector is leaked.
  - Matches PCI virtio device type `2` (`d.virtio_type() == Some(2)`), brings
    up its transport via `virtio::transport` (MSI-X vector required), negotiates
    `Feature::VERSION_1`, `VIRTIO_BLK_F_RO` (bit 5), `VIRTIO_BLK_F_BLK_SIZE`
    (bit 6), and `VIRTIO_BLK_F_FLUSH` (bit 9). Reads 64-bit `capacity` via
    `transport.read_consistent(...)` (virtio 1.2 §2.4.1), reads `blk_size`
    (config offset 20) **only** when `VIRTIO_BLK_F_BLK_SIZE` was negotiated
    (defaulting to `512` otherwise), rejects `capacity == 0` or unsupported
    `sector_size != 512`, checks `max_queue_size(0) >= QUEUE_SIZE`
    (`QUEUE_SIZE = 16`, `indirect = false`), and calls `virtio::check_device`
    before publishing the device.
  - **Bounded interrupt-driven I/O (no unbounded crate spins, 1-frame chunking):**
    `virtio-drivers` `0.13.0`'s synchronous `VirtIOBlk::read_blocks`/
    `write_blocks`/`flush` spin unboundedly on `can_pop()`. Following `rng.rs`
    (#97), the driver drives its own `VirtQueue` directly (serialized per device
    behind a `SleepMutex`) for `VIRTIO_BLK_T_IN` (0), `VIRTIO_BLK_T_OUT` (1),
    and `VIRTIO_BLK_T_FLUSH` (4: 2-descriptor chain `[req_hdr { sector: 0,
    reserved: 0 }]`, `[status]` with no data buffer; no-op `Ok(())` on
    `read_only` devices or when `VIRTIO_BLK_F_FLUSH` was not negotiated),
    waiting on the slot's MSI-X `Doorbell` via `StatusHandle::wait_for` with a
    bounded deadline (`BLK_REQUEST_TIMEOUT_NS = 5_000_000_000`, 5 s for loaded
    TCG hosts). Multi-sector transfers are chunked at `<= 4 KiB` (`BLOCK_SIZE`,
    1 physical frame per data descriptor—preventing contiguous-frame
    `VirtioHal::share` OOM panics under fragmentation and respecting any
    `VIRTIO_BLK_F_SIZE_MAX >= 4096`). Each bound device uses **5 runs** in
    `VirtioHal::TABLE` (2 persistent queue `Dma` runs + up to 3 transient
    `Bounce` runs; the `virtio-blk` PR updates the budget comment in
    `kernel/src/virtio.rs`).
  - **Completion validation (`used_len` check):** because `VirtioHal::share`
    zero-fills `DeviceToDriver` bounce buffers and `VIRTIO_BLK_S_OK == 0`, an
    unwritten status byte would otherwise look like `OK`. Every `queue.pop_used`
    must verify both `token` match and `used_len` (`used_len as usize ==
    data.len() + 1` for `IN`, `>= 1` for `OUT`/`FLUSH`), treating a short
    `used_len` as `BlockError::Io`.
  - **Drop order & retirement:** the transport field precedes the `VirtQueue`
    field so device reset always runs before queue DMA teardown. If a request
    times out or the device sets `DEVICE_NEEDS_RESET`/`FAILED`, the driver
    retires the device by **dropping `VirtioTransport` first** (resetting the
    device and releasing `DeviceResources`, matching `rng.rs`), logs once with
    any leaked bounce run count, calls `pci::failed_after_bind`, and returns
    `BlockError::Timeout`/`DeviceFailed` on all subsequent calls.
  - **Probe interaction with existing M3 tests:** `virtio-blk` joins `DRIVERS`
    in `kernel/src/drivers.rs`. The PR that adds it updates
    `tests/scenarios/test_pci.py` (expecting `virtio-blk: bound` and 4 bound
    devices when `Devices.every()` runs), `tests/scenarios/test_ktest.py`
    (driver count when `blk_mib=1` is attached, plus attaching an **additional**
    unclaimed virtio device—such as `virtio-gpu-pci` or a 6th `virtio-input`—
    since every existing ktest virtio device is already claimed),
    `kernel/src/virtio.rs::transport_brings_up_a_virtio_device_through_the_hal`
    (switching from `ktest_device(2)` / `DeviceType::Block` to that new
    unclaimed device), and `tests/features/dev.toml`.

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
  (#155) from a `KeyEvent` press's text, code and modifiers, so they need
  no second decoder.
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

**virtio first** (rng, input, sound in M3; blk in M7; net in M9) under
QEMU; real-hardware drivers (NVMe, AHCI, xHCI HID, a common NIC) in M12
behind the same traits.

## 8. Processes, syscalls and capabilities (WASM apps M5: built, #195; Linux x86-64 ABI M6 built #230/#232–#238, M7 built (#246 design), M8 designed in #336; capability namespaces M11)

Lumen runs two kinds of user code:

1. **Sandboxed WASM apps (M5, built):** run by `wasmi` on kernel threads with
   explicit capability handles (`Log`, `Time`, `Desktop`, `Window`, `Trace`),
   described in "Apps: WASM modules in the kernel" below.
2. **Native ring-3 Linux x86-64 processes (M6 built, M7 built, M8
   designed in #336, M9–M10 planned):**
   unmodified Linux `x86_64` ELF binaries (targeting Alpine Linux `musl`)
   entering through the standard Linux `syscall` ABI. Starting in M11, each
   Linux process/sandbox can be confined by capability-scoped VFS mount
   namespaces and network allowlists, preserving Principle 2 (no ambient
   authority for agents) without requiring custom `libc` or runtime forks.

### Linux x86-64 syscall ABI (M6 built #232–#238; M7 built (#246 design); M8 designed in #336)

- **Register convention:** identical to Linux x86-64. Syscall number in `rax`;
  up to 6 arguments in `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9` (`r10` replaces
  `rcx`, which `syscall` overwrites with user RIP). Return value in `rax`:
  `0..=usize::MAX - 4096` on success, or negated Linux `errno` (`-4095..=-1`)
  on error. **Every GPR except `rax`, `rcx`, and `r11`** (including `rdi`,
  `rsi`, `rdx`, `r8`, `r9`, `r10`, `rbx`, `rbp`, `r12`–`r15`, and user `rsp`)
  is preserved across `syscall`, matching `musl`'s `syscall_arch.h`.
- **ABI types:** sourced from the pinned `no_std` crate `linux-raw-sys` `=0.12.1`
  (adopted in #232, re-exported by `kernel/src/linux/mod.rs`) so syscall numbers,
  `errno` constants, `utsname`, `timespec`, `iovec`, `stat`, `termios`, and
  `sigaction` layouts match Linux x86-64 verbatim.
- **Unknown syscalls:** return `-ENOSYS` (`-38`) and log once per syscall
  number per boot at `debug` level (never panic or kill the process).
- **Handlers (`kernel/src/linux/syscall/`, #254):** `dispatch` has each
  number in one arm (its constant as the pattern; a number past `u32::MAX`
  is `-ENOSYS`, though recent Linux reads only `eax`), passing the
  registers of the syscall's Linux signature, in order, to a handler in its
  cluster's module (`fd`, `stat`, `ns`, `dir`, `meta`, `io`, `lock`; M6's
  `mem` and `proc`). An M7 syscall not yet implemented is a stub logging
  like an unknown one (with its name) and returning `-ENOSYS`. Wave 3's
  handlers follow three rules:
  1. **Decode first.** An M7 arm decodes each register with
     `syscall/at.rs`'s `reg` into the handler's parameter type, as Linux
     reads it: an `int`, `mode_t` or flags argument is the register's low
     32 bits (`Fd`, `Dirfd`, where `AT_FDCWD` sign- or zero-extended is the
     same `Dirfd::Cwd`, `Mode`, lumen-vfs's `AtFlags`/`OpenFlags`/
     `RenameFlags`/`AccessMask`, `u32`, `i32`); a size, offset or pointer
     is a whole `u64` or `UserPtr`. A handler never compares a raw register.
  2. **One dirfd resolution.** Turning a `Dirfd` into the directory a path
     starts from is `at::start_dir(dirfd, &UserPath) -> Result<Start, u32>`
     (built in #254b); every `*at` handler (and each legacy shim, as
     `Dirfd::Cwd`) calls it once, after `UserPath::copy_in`, and reaches
     lumen-vfs's path operations only through `Start`'s wrappers
     (`open` and `mkdir`, which take `Mode` and apply the process's umask
     themselves, `resolve`, `unlink`, `readlink`, `symlink`, and the
     associated `Start::link` and `Start::rename` across two starts).
     A handler that only needs the inode a path names calls
     `at::lookup(dirfd, &UserPath, AtFlags)`: `start_dir`, then `resolve`,
     with `AT_SYMLINK_NOFOLLOW` and `AT_EMPTY_PATH` read from its flags
     (not for `linkat`, whose `AT_SYMLINK_FOLLOW` runs the other way).
     A `NULL` path is `-EFAULT` (`copy_in`) for every handler but two:
     `newfstatat` and `statx` with `AT_EMPTY_PATH` read it as `""`
     (`UserPath::empty()`), whatever the dirfd and their other flags
     (Linux 6.11 does so only for an fd dirfd with exactly that flag, 6.13
     generally); `faccessat2` and the rest keep `-EFAULT`.
     `Start`'s directory is private, so no handler can hand lumen-vfs a raw
     `start_dir` (where `None` means the global root, never the cwd). Its
     contract, the traps included: an absolute path starts at the global
     root and the dirfd is never looked at, so a closed, negative or
     non-directory one is no error; for a relative path, `Dirfd::Cwd` is the
     cwd's inode, a concrete `Arc`, and `Dirfd::Fd` the descriptor's inode
     (`-EBADF` if it isn't open, `-ENOTDIR` if it isn't a directory; an
     `O_PATH` directory is fine); an empty path (which `copy_in` lets
     through only with `AT_EMPTY_PATH`, or for `readlinkat`, which takes
     one without it) names the descriptor's file itself,
     a directory or not (or the cwd). `Fd::file()` is every handler's
     descriptor lookup (`-EBADF`). `Start::display_path` joins a relative
     path to the start's own (the cwd's, or the dirfd's open file's) for
     `/proc/<pid>/fd`; it is lexical and doesn't follow a later rename.
  3. **Positive errnos inside, negated once.** Helpers return
     `Result<_, u32>` holding a positive errno (`errno::ENOENT`, as
     `VfsError::to_errno` gives); the handler applies `neg_errno` once, on
     its way out (a private `fn inner(..) -> Result<u64, u32>` wrapped once,
     as `read_into_user` is).

  `at.rs` also copies a path argument in (`UserPath`: `PATH_MAX` with its
  NUL, else `-ENAMETOOLONG`; empty `-ENOENT` unless `AT_EMPTY_PATH` (or for `readlinkat`); a bad
  or `NULL` pointer `-EFAULT`, `UserPath::empty()` aside; any bytes but
  NUL, UTF-8 or not, #268), fills `struct stat` and `struct statx` from VFS
  `Metadata` and writes them to user memory (`write_stat`, `write_statx`:
  `-EFAULT` for a bad buffer). Each cluster's tests check its arms take
  their registers in order, with distinct values in each argument (the
  M7 stubs, and their ktests, went with the last of them, `msync`).

#### M6 initial syscall table (30 syscalls: static `musl` & `busybox ash` built-ins)

| # | Syscall | M6 behaviour |
|---|---|---|
| 0 | `read(fd, buf, count)` | fd 0 reads from the console/serial input queue (blocking until bytes arrive or EOF); other fds `-EBADF` |
| 1 | `write(fd, buf, count)` | fd 1/2 write to the console/serial output via `copy_from_user`; other fds `-EBADF` |
| 3 | `close(fd)` | closes an open entry in the process's FD table; `-EBADF` if absent |
| 5 | `fstat(fd, statbuf)` | minimal character-device `stat` for fd 0/1/2 (so `musl` stdio and `busybox ash` detect a terminal); `-EBADF` otherwise |
| 9 | `mmap(addr, len, prot, flags, fd, off)` | `MAP_ANONYMOUS` with `MAP_PRIVATE` or `MAP_SHARED` (rejecting `MAP_ANONYMOUS` without either, and rejecting `len == 0`, with `-EINVAL`; ignoring `fd` and `off`); supports `MAP_FIXED`, `MAP_FIXED_NOREPLACE` (`-EEXIST`), and `PROT_NONE` guard VMAs; demand-paged via `lumen-vm` (section 5); file-backed `mmap` arrives in M7 |
| 10 | `mprotect(addr, len, prot)` | updates VMA rights (splitting sub-VMAs when needed) and mapped PTEs (preserving `PROT_NONE` frames, section 5); returns `0` on `len == 0` (page-aligned `addr`), `-ENOMEM` on any unmapped hole; local `invlpg` in M6–M7, cross-CPU shootdown in M8 |
| 11 | `munmap(addr, len)` | unmaps pages, frees frames, splits/removes VMAs; ignores unmapped holes, returns `-EINVAL` on `len == 0` or misaligned `addr` |
| 12 | `brk(addr)` | tracks a byte-granular heap cursor (backing VMAs page-aligned); returns current `brk` on `addr == 0`, exact requested `addr` on success, and unchanged current `brk` on any failure (`addr < initial_brk`, VMA overlap, OOM)—never `-ENOMEM` |
| 13 | `rt_sigaction(sig, act, oldact, sigsetsize)` | requires `sigsetsize == 8` (`-EINVAL` otherwise); rejects non-null `act` for `SIGKILL`/`SIGSTOP` (`-EINVAL`); stores a per-process 64-entry `sigaction` table so `oldact` preserves prior handlers/`SIG_IGN`; real signal dispatch arrives in M8 |
| 14 | `rt_sigprocmask(how, set, oldset, sigsetsize)` | requires `sigsetsize == 8`; maintains per-thread signal mask (silently dropping `SIGKILL`/`SIGSTOP`); real delivery arrives in M8 |
| 16 | `ioctl(fd, req, arg)` | returns `-EBADF` for unopened/closed fds; on fd 0/1/2: `TCGETS` (`0x5401`) fills a default `struct termios`, `TCSETS`/`TCSETSW`/`TCSETSF` (`0x5402`–`0x5404`) succeed as no-ops (0), and `TIOCGWINSZ` (`0x5413`) fills `struct winsize` with console rows/cols; all other requests `-ENOTTY` |
| 19 | `readv(fd, iov, iovcnt)` | validates `0 <= iovcnt <= 1024`, copies `iovec` array via `copy_from_user`, skips zero-length spans without dereferencing `iov_base`, reads from fd 0 (needed by `musl` `__stdio_read` for buffered stdin) |
| 20 | `writev(fd, iov, iovcnt)` | validates `0 <= iovcnt <= 1024`, copies `iovec` array via `copy_from_user`, skips zero-length spans without dereferencing `iov_base`, writes each span to fd 1/2 |
| 39 | `getpid()` | returns process PID (`>= 1`) |
| 60 | `exit(status)` | terminates the calling thread (and single-threaded M6 process), recording exit status |
| 63 | `uname(buf)` | fills `utsname` (`sysname = "Linux"`, `release = "6.6.0-lumen"`, `machine = "x86_64"`) so `musl`/Alpine binaries accept the kernel |
| 79 | `getcwd(buf, size)` | copies `"/"\0` and returns `2` (byte length including the NUL terminator), or `-ERANGE` if `size < 2` (real cwd from VFS in M7) |
| 102 | `getuid()` | returns `0` in M6 |
| 104 | `getgid()` | returns `0` in M6 |
| 107 | `geteuid()` | returns `0` in M6 |
| 108 | `getegid()` | returns `0` in M6 |
| 110 | `getppid()` | returns parent PID (`0` for kernel-spawned root process) |
| 111 | `getpgrp()` | returns process PID in M6 (real process groups in M8) |
| 158 | `arch_prctl(code, addr)` | `ARCH_SET_FS` (`0x1002`, validates `addr < USER_MAX`, `0` allowed); `ARCH_GET_FS` (`0x1003`, writes `fs_base` to `*addr` via `copy_to_user` and returns `0`); other codes `-EINVAL` |
| 186 | `gettid()` | returns thread TID |
| 218 | `set_tid_address(tidptr)` | stores `clear_child_tid` pointer and returns current TID |
| 228 | `clock_gettime(clockid, tp)` | `CLOCK_REALTIME` (0, from RTC/boot offset), `CLOCK_MONOTONIC` (1), `CLOCK_MONOTONIC_RAW` (4), `CLOCK_REALTIME_COARSE` (5), `CLOCK_MONOTONIC_COARSE` (6), `CLOCK_BOOTTIME` (7) |
| 231 | `exit_group(status)` | terminates the process and frees its `AddressSpace` and FD table |
| 302 | `prlimit64(pid, res, new, old)` | validates `pid == 0 \|\| pid == current_pid` (`-ESRCH` otherwise) and `res < RLIM_NLIMITS` (16, `-EINVAL` otherwise); M6 read-only defaults for `RLIMIT_STACK` (8 MiB), `RLIMIT_NOFILE` (1024), and `RLIM_INFINITY` for other valid resources when `old` is non-null; writable per-process limit enforcement arrives in M10 |
| 318 | `getrandom(buf, len, flags)` | rejects unknown flag bits (`-EINVAL`), fills user buffer from `crate::rng` (section 7) via `copy_to_user`, and returns bytes written |

In addition to the 30 table entries above, M6 includes console `poll(2)` (syscall 7)
to satisfy Rust `std`'s `sanitize_standard_fds` startup check (`poll([0, 1, 2], 3, 0)`)
and `busybox` `lineedit` stdin readiness waits; full pipe/socket/PTY `poll`/`ppoll`/`select`
arrives in M8.

#### M7 POSIX VFS, file-backed `mmap`, and dynamic-linking syscalls (designed in #246)

In M7, the per-process FD table grows from 64 to `MAX_FDS = 1024` (matching
`RLIMIT_NOFILE`), storing `Option<FdEntry { file: Arc<OpenFile>, cloexec: bool }>`.

**Built in #254b** (`kernel/src/linux/process.rs`, `syscall/fd.rs`,
`syscall/dir.rs`; split into M8's `Process` and `Files` by B1, below): the
process's `Files` holds `table: SpinLock<FdTable<FileRef>>` (`FileRef` =
`Arc<OpenFile<KernelVfsLock<u64>>>`)
(private: a slot leaves it only through `close_current_fd`, a displacing
`insert_current_fd`/`dup_current_fd` or the takedown's `Files::leave`
(behind `take_current` and the test seams' `RegisteredProcess`), which
release its locks; a fresh `OpenFile` gets its reserved slot through
`ReservedFd::install`, and `insert_current_fd` takes one by value for an
exact slot; `install_current_fd` is a ktest-only reserve-and-install helper;
`dup_current_fd` (`dup2`/`dup3`) and
`dup_current_fd_from` (`F_DUPFD*`), each with a close-on-exec flag, count
the slot they share),
`cwd: Cwd` (an `OpenGuard` on a directory and its path, computed from the
inode when `chdir`/`fchdir` set it, which a relative open's
`/proc/<pid>/fd` path is joined to) and
`umask` (`0o022`). A process starts with `Stdio`, opened by the caller
before the process's thread is spawned: fd 0 on `/dev/console` `O_RDONLY`
and fds 1 and 2 `O_WRONLY` (three open file descriptions, each through
`open_inode_guarded`, so M6's `-EBADF` for `read(1)` and `write(0)` stays,
and `FIONBIO` on one doesn't reach the others) and `/` as its cwd. If any
can't be opened the run is refused (an `error!` line): a process never
starts with an empty stdio slot. A descriptor
on a terminal (`VfsInode::is_tty`: devfs's `tty` and `console`) reads and
writes the process's `ConsoleIo` exactly as M6 did; any other reads and
writes its inode at `OpenFile::offset` through a 4 KiB bounce buffer on the
stack, the offset's lock (Lock 2) held across each chunk's `read_at`/
`write_at` but never across the copy (a chunk the copy out refuses is
given back to the offset). On procfs the offset's lock isn't held across
`read_at` (`/proc/<pid>/maps` takes Lock 1 there): the chunk is read at a
snapshot of the offset, which moves after only if nothing moved it
meanwhile. An `O_APPEND` write goes through `VfsInode::append`, one chunk
at a time (a write over 4 KiB can have another appender's data land
between its chunks). `close` takes the entry out under the table's lock
and, after it, releases every current table member's POSIX record locks
on the inode (any close) and, on the description's last `dec_fd_ref`, its
`flock` locks. `dup2`/`dup3` displacement uses the same `release_fd` path.
Exit separately sweeps the departing pid's locks before `Files::leave`;
the last member drains FD references and flock. Both paths share
`release_locks`, which wakes blocked lockers after releasing inode/table
locks. `openat` reserves a slot before it opens and keeps it until installation (a full table is `-EMFILE` before anything is created), and
`getcwd` computes the path from the cwd's inode (`compute_cwd_path`). `umask` sets the mask
(`syscall/meta.rs`). `chdir` and `fchdir` set the cwd (its kept path
computed from the inode too, so relative `chdir`s never grow it), and
`getdents64` lists a directory from its open file's `dir_cursor`, the
filesystem's own `readdir` cursor (`d_off`: the cursor past each entry),
which stays stable as entries come and go (section 9, with ext4's two
exceptions there), so a model of busybox `rm -r`'s loop, reading on while
it unlinks, empties a directory; the other M7 numbers not yet implemented
are stubs (wave 3).

The design #246 set out for this, and the behaviour of each rerouted call:
Console FDs 0/1/2 become `OpenFile` entries backed by the process's `ConsoleIo`
device node (preserving all M6 serial, framebuffer VT, and `usertest` capture
semantics), and M6's `read` (0), `write` (1), `close` (3), `fstat` (5), `poll`
(7), `ioctl` (16), `readv` (19), `writev` (20), `mmap` (9), and `getcwd` (79)
route through the VFS and real process working directory:
- **`ioctl` (16):** tty requests (`TCGETS`, `TCSETS*`, `TIOCGWINSZ`) succeed
  **only** on console/tty descriptors and return **`-ENOTTY`** on regular
  files, directories, `/dev/null`, `/proc/*`, etc. (so `isatty()` works for
  `musl` stdio buffering and `busybox`); `FIOCLEX`/`FIONCLEX`/`FIONBIO`
  update per-FD `FD_CLOEXEC` / `O_NONBLOCK` on any open non-`O_PATH` FD.
- **`readv` (19):** loops across all non-empty `iov` spans for regular files
  (stopping early only on EOF, an error, or a short span: the page cache's
  short count for an error past a span's first page, #321), while keeping
  single-read early return on console/tty FDs.
- **`poll` (7):** regular files, directories, `/proc/*`, and always-ready
  character devices (`/dev/null`, `/dev/zero`, `/dev/random`, `/dev/urandom`)
  report `POLLIN | POLLOUT` immediately (matching Linux), while console FDs
  0/1/2, `/dev/tty`, and `/dev/console` preserve M6's readiness-driven polling
  (`POLLOUT` immediately on a writable one; `POLLIN` on a readable one, gated on `stdin_ready(io)` with
  timeout-aware blocking wait).
- **`mmap` (9):** M7 widens `sys_mmap` dispatch from M6's 4 arguments to all 6
  Linux arguments `(rdi, rsi, rdx, r10, r8 = fd, r9 = off)`.

Because `musl` on `x86_64` invokes legacy non-`*at` / non-`*2` syscalls
directly (for example `open(2)` in `ldso/dynlink.c`), M7 wires **both** the
`*at` and legacy entry points (every legacy syscall is a thin shim over the
corresponding `*at(AT_FDCWD, ...)` handler):

| # | Syscall | M7 behaviour (designed in #246) |
|---|---|---|
| 2 | `open(path, flags, mode)` | shim to `openat(AT_FDCWD, path, flags, mode)` |
| 4 | `stat(path, buf)` | shim to `newfstatat(AT_FDCWD, path, buf, 0)` |
| 6 | `lstat(path, buf)` | shim to `newfstatat(AT_FDCWD, path, buf, AT_SYMLINK_NOFOLLOW)` |
| 8 | `lseek(fd, off, whence)` | `SEEK_SET` (0), `SEEK_CUR` (1), `SEEK_END` (2) on regular files, `/proc`, and `/dev/{null,zero,random,urandom}` (returning 0); on directories updates `OpenFile::dir_cursor` (`SEEK_SET` for `musl` `rewinddir`/`seekdir`); `-ESPIPE` on console/tty; `-EBADF` on `O_PATH`; rejects resulting negative offset with `-EINVAL` |
| 9 | `mmap(addr, len, prot, flags, fd, off)` | extends M6 anonymous `mmap` with file-backed `MAP_PRIVATE` (CoW) and `MAP_SHARED` (zero-copy unified `PageCache`, section 5); masks `flags & MAP_TYPE` (`0x0f`) to select `MAP_SHARED` (1), `MAP_PRIVATE` (2), or `MAP_SHARED_VALIDATE` (3, rejecting unknown flags with `-EOPNOTSUPP`; any other `MAP_TYPE` → `-EINVAL`); `MAP_SYNC` and `MAP_FIXED_NOREPLACE` with `MAP_SHARED_VALIDATE` → `-EOPNOTSUPP`, as Linux's `LEGACY_MAP_MASK`; ignores advisory `MAP_DENYWRITE`/`MAP_EXECUTABLE`/`MAP_POPULATE`/`MAP_NORESERVE`/`MAP_NONBLOCK`/`MAP_STACK`/`MAP_LOCKED`/`MAP_UNINITIALIZED` and `MAP_32BIT`/`MAP_ABOVE4G`; `MAP_HUGETLB` → `-EINVAL`, and `MAP_GROWSDOWN` on a file → `-EINVAL`; validates `off & 0xfff == 0`, `len != 0` (`-EINVAL`), `len`'s page rounding (`-ENOMEM`), `off + aligned_len` at most `i64::MAX` for a regular file, below 2^64 otherwise (Linux's `ULONG_MAX` for a character device) (`-EOVERFLOW`), `fd` open & readable (`-EBADF` if closed or `O_PATH`, `-EACCES` on `O_WRONLY` or `MAP_SHARED \| PROT_WRITE` without `O_RDWR`), `-ENODEV` for non-mappable nodes (except `/dev/zero`, mapped as anonymous zero pages); records `may_write` on `FileMapping` |
| 17 | `pread64(fd, buf, count, off)` | positional read at `off >= 0` without reading/modifying `OpenFile::offset`; `count` capped at 1 MiB (`MAX_SINGLE_IO`), and a fault or filesystem error after the first 4 KiB chunk returns the bytes moved (as does a filesystem error past the first page inside that chunk: the page cache's short count, #321). Errors in this order: `-EINVAL` if `off < 0`; `-EBADF` if not open or `O_PATH`; `-ESPIPE` on console/tty and FIFO/socket nodes; `-EBADF` if not readable; `-EFAULT` if the buffer isn't wholly in user space; `-EINVAL` if `off + count` passes `i64::MAX`; `-EISDIR` on directories (even for 0 bytes) |
| 18 | `pwrite64(fd, buf, count, off)` | positional write at `off >= 0` without reading/modifying `OpenFile::offset` (when `O_APPEND` is set, appends to EOF per Linux `pwrite(2)` semantics); same cap and the same errors in the same order as `pread64` (`-EBADF` if not writable; no `-EISDIR`, as a directory can't be open for writing) |
| 21 | `access(path, mode)` | shim to `faccessat2(AT_FDCWD, path, mode, 0)` |
| 26 | `msync(addr, len, flags)` | validates page-aligned `addr`, `flags` subset of `MS_ASYNC \| MS_SYNC \| MS_INVALIDATE` (`MS_ASYNC \| MS_SYNC` rejected with `-EINVAL`), returns `0` immediately on `len == 0` (or a `len` within a page of 2^64, which Linux's rounding wraps to 0) and succeeds as a no-op over `MAP_PRIVATE`/anonymous VMAs, `-ENOMEM` when `addr + len` rounded up overflows and on any unmapped hole (with `MS_SYNC`, after writing back the mapped parts; a writeback error such as `-EIO` wins); for `MS_SYNC`, writes back each `MAP_SHARED` file's dirty pages (a writable mapping keeps its pages dirty, section 5) through its filesystem and block cache to the device, with the address space's lock released |
| 72 | `fcntl(fd, cmd, arg)` | `F_DUPFD` (0) / `F_DUPFD_CLOEXEC` (1030) (`-EINVAL` if `arg < 0 \|\| arg >= MAX_FDS`, `-EMFILE` if no free slot in `[arg, MAX_FDS)`), `F_GETFD` (1), `F_SETFD` (2, `FD_CLOEXEC`), `F_GETFL` (3), `F_SETFL` (4, mutating `O_APPEND \| O_NONBLOCK`; the access mode and creation flags ignored, as on Linux, and `O_ASYNC`/`O_DIRECT`/`O_NOATIME` too, which Linux would change and Lumen doesn't track), and POSIX byte-range advisory locks `F_GETLK` (5), `F_SETLK` (6), `F_SETLKW` (7) using `struct flock` (process-scoped; a bad `l_type`/`l_whence` or negative start `-EINVAL`, a range past `OFFSET_MAX` `-EOVERFLOW`, then the open mode `F_WRLCK` → writable / `F_RDLCK` → readable else `-EBADF`; normalizing `l_len < 0`; `F_SETLKW` is `-EDEADLK` where its wait would close a cycle of waiting processes, section 9); on an `O_PATH` fd every `cmd` but `F_DUPFD*`/`F_GETFD`/`F_SETFD`/`F_GETFL` is `-EBADF`, unknown ones included; on any other fd an unknown `cmd` is `-EINVAL` |
| 73 | `flock(fd, op)` | open-file-description advisory lock (`LOCK_SH`, `LOCK_EX`, `LOCK_UN`, optional `LOCK_NB`; independent of `fcntl` POSIX locks); as Linux 6.0+, `LOCK_MAND` is ignored (returns 0; reported once a boot, at info) and `op` is checked (`-EINVAL`) before `fd` (`-EBADF`, `O_PATH` too); atomic upgrade/downgrade on same `OpenFileId` when nothing conflicts (a blocking request that conflicts gives the held lock up first, as Linux; section 9), `-EWOULDBLOCK` on conflict with `LOCK_NB` (the held lock kept: a deviation, Linux drops it), released when last FD to the `OpenFile` closes |
| 74 | `fsync(fd)` | flushes all dirty `PageCache` pages (incl. `writable_maps > 0`) and inode metadata to the filesystem, writes dirty `BlockCache` blocks, and issues `BlockDevice::flush()`; returns `0` on regular files/dirs (and detached unlinked inodes as a no-op), `-EBADF` on `O_PATH`, and `-EINVAL` on console/tty, `/dev/*`, character-device, FIFO and socket nodes and anything on procfs, as Linux (no `->fsync`); also on block-device nodes, an M7 deviation (Linux's `blkdev_fsync` syncs the device, which M7 can't open) |
| 75 | `fdatasync(fd)` | flushes file data pages and size-changing metadata (ext4: all dirty metadata) through `BlockCache` + `BlockDevice::flush()` (same `-EBADF`/`-EINVAL` rules as `fsync`) |
| 76 | `truncate(path, len)` | validates `len >= 0` (`-EINVAL`) before resolving `path` (following symlinks); a directory `-EISDIR`, any other non-regular file `-EINVAL`, a read-only mount `-EROFS`; then resizes as `ftruncate` does |
| 77 | `ftruncate(fd, len)` | validates `len >= 0` and `fd` open for writing on a regular file (`-EINVAL` for `len < 0` first, `-EBADF` if closed or `O_PATH`, `-EINVAL` if not open for writing or not regular, `-EROFS` on a read-only mount, `-EFBIG` past the filesystem's largest file); the inode's truncate zeroes the partial final page's tail when `len % 4096 != 0` on shrink (or the old tail on grow), drops cached pages past `len.div_ceil(4096)` and updates `st_size` and `mtime`/`ctime`; then `invalidate_file_mappings` runs `invalidate_file_from` on every address space mapping the file to unmap PTEs past `align_up_page(len)` (section 5, B7) |
| 79 | `getcwd(buf, size)` | returns canonical NUL-terminated path of the process's current working directory (computed/maintained across ancestor `rename`, length including NUL, `-ERANGE` if `size` too small, `-ENOENT` if cwd was unlinked / `st_nlink == 0`) |
| 80 | `chdir(path)` | resolves `path` (following symlinks), requires directory (`-ENOTDIR`), updates process `cwd` inode and path (extracting and dropping the old `Arc<VfsInode>` outside the process's locks) |
| 81 | `fchdir(fd)` | validates `fd` refers to an open directory (`-ENOTDIR`/`-EBADF`; `O_PATH` dirs allowed), updates process `cwd` |
| 82 | `rename(old, new)` | shim to `renameat2(AT_FDCWD, old, AT_FDCWD, new, 0)` |
| 83 | `mkdir(path, mode)` | shim to `mkdirat(AT_FDCWD, path, mode)` |
| 84 | `rmdir(path)` | shim to `unlinkat(AT_FDCWD, path, AT_REMOVEDIR)` |
| 86 | `link(old, new)` | shim to `linkat(AT_FDCWD, old, AT_FDCWD, new, 0)` |
| 87 | `unlink(path)` | shim to `unlinkat(AT_FDCWD, path, 0)` |
| 88 | `symlink(target, linkpath)` | shim to `symlinkat(target, AT_FDCWD, linkpath)` |
| 89 | `readlink(path, buf, bufsiz)` | shim to `readlinkat(AT_FDCWD, path, buf, bufsiz)` |
| 90 | `chmod(path, mode)` | shim to `fchmodat(AT_FDCWD, path, mode)` |
| 91 | `fchmod(fd, mode)` | updates `mode & 0o7777` and `ctime` on open `fd`'s inode (`-EBADF` on `O_PATH`, `-EROFS` on a read-only mount; devfs: see below) |
| 92 | `chown(path, uid, gid)` | shim to `fchownat(AT_FDCWD, path, uid, gid, 0)` |
| 93 | `fchown(fd, uid, gid)` | updates `uid`/`gid` (`u32::MAX` = leave unchanged) and `ctime` on open `fd`'s inode, dropping setuid (and setgid with group-execute) on a non-directory in the same `set_owner` (`-EBADF` on `O_PATH`, `-EROFS` on read-only fs; devfs: see below) |
| 94 | `lchown(path, uid, gid)` | shim to `fchownat(AT_FDCWD, path, uid, gid, AT_SYMLINK_NOFOLLOW)` |
| — | *`chmod`, `chown` and `utimensat` families on `devfs`* | devfs's mounts are writable, as every Linux devtmpfs mount (#301), so these reach its setters (#293). Its nodes' setters keep the new mode, owner and times in memory until reboot, and `stat` shows them, on any devfs instance. `/dev` itself stays `-EROFS`, its setters refusing (a divergence: Linux's devtmpfs takes these on `/dev` too), while `access("/dev", W_OK)` passes for root, as on Linux. A special file on a read-only mount is `-EROFS`, as Linux's `mnt_want_write` |
| 95 | `umask(mask)` | atomically sets per-process `umask = mask & 0o777` (default `0o022`) and returns previous mask |
| 162 | `sync()` | snapshots mounted filesystems and cached inodes (never holding `MountTable`/`InodeCache` across I/O), flushes every dirty `PageCache` page and `BlockCache` block followed by `BlockDevice::flush()`; always returns `0` |
| 217 | `getdents64(fd, dirp, count)` | validates open directory (`-ENOTDIR`, `-EBADF` on `O_PATH`), emits 8-byte-aligned `struct linux_dirent64` records (`d_ino`, `d_off`, `d_reclen`, `d_type`, NUL-terminated `d_name`) advancing `OpenFile::dir_cursor` (the filesystem's `readdir` cursor, stable as entries come and go, with ext4's exceptions in section 9; `d_off` the cursor past each record); returns bytes written (`0` at EOF, `-EINVAL` if `count` cannot fit the next entry) |
| 257 | `openat(dirfd, path, flags, mode)` | resolves path relative to `dirfd`/`AT_FDCWD`, honouring `O_RDONLY`/`O_WRONLY`/`O_RDWR` (a write open of a special file is never `-EROFS`, #291), `O_CREAT` (`mode & !umask`; `-EISDIR` on trailing `/`; `-EEXIST` with `O_EXCL` even on dangling symlink), `O_TRUNC` (ignored on special files, `-EISDIR` on dirs, `-EROFS` on regular files on rofs, runs `ftruncate(0)` path on regular files), `O_APPEND`, `O_DIRECTORY` (`-ENOTDIR`), `O_NOFOLLOW` (`-ELOOP` on final symlink without trailing `/` unless `O_PATH` is also set; trailing `/` forces directory traversal even with `O_NOFOLLOW`, matching Linux), `O_PATH`, `O_CLOEXEC`, `O_NONBLOCK`; returns `-EOPNOTSUPP` on `O_TMPFILE`; allocates lowest free FD `< MAX_FDS` (`-EMFILE` if full) |
| 258 | `mkdirat(dirfd, path, mode)` | creates directory with `(mode & 0o1777) & !umask` (setuid/setgid dropped, sticky kept, as Linux's `vfs_mkdir`), stripping trailing slashes; `-EEXIST` if present, `-ENOENT`/`-ENOTDIR` for missing/non-dir parent |
| 260 | `fchownat(dirfd, path, uid, gid, flags)` | supports `AT_SYMLINK_NOFOLLOW` and `AT_EMPTY_PATH`; rejects unknown flags (`-EINVAL`); drops setuid (and setgid with group-execute) on a non-directory, as `fchown` |
| 262 | `newfstatat(dirfd, path, buf, flags)` | supports `AT_SYMLINK_NOFOLLOW` and `AT_EMPTY_PATH` (a `NULL` path with it is `""`, whatever the dirfd and other flags: Linux 6.11 for an fd dirfd with exactly that flag, 6.13 generally), and accepts and ignores `AT_NO_AUTOMOUNT` and the `AT_STATX_SYNC_TYPE` bits (as Linux's `vfs_statx`); writes 144-byte Linux x86-64 `struct stat` with unique `(st_dev, st_ino)` per filesystem/inode |
| 263 | `unlinkat(dirfd, path, flags)` | `flags == 0`: unlinks non-directory (`-EISDIR` on dir and on `/`, `.` or `..`, `-ENOTDIR` on trailing `/`, a symlink to a directory included), handling open-unlinked inodes cleanly (section 9); `flags == AT_REMOVEDIR`: removes empty directory (`-ENOTEMPTY`, also for `..`, `-EINVAL` for `.`, `-EBUSY` for `/` and a mount point); unknown flags `-EINVAL`, checked before the path |
| 264 | `renameat(olddirfd, old, newdirfd, new)` | shim to `renameat2(olddirfd, old, newdirfd, new, 0)` |
| 265 | `linkat(olddirfd, old, newdirfd, new, flags)` | creates hard link (`AT_SYMLINK_FOLLOW` / `AT_EMPTY_PATH`); rejects existing target (`-EEXIST`, a trailing `/` too; `-ENOENT` for a trailing `/` on a free name), then a read-only filesystem (`-EROFS`), then cross-mount (`-EXDEV`), then directories (`-EPERM`), Linux's order, incrementing `st_nlink` |
| 266 | `symlinkat(target, newdirfd, linkpath)` | creates symlink with verbatim non-empty `target` (`-ENOENT` if `target` is empty, `-EEXIST` if `linkpath` exists, `-ENOENT`/`-EEXIST` on trailing `/`; the target's bytes stored as given, UTF-8 or not, #268) |
| 267 | `readlinkat(dirfd, path, buf, bufsiz)` | does not follow final symlink (`-EINVAL` if target is not a symlink or `bufsiz <= 0`); accepts empty `path == ""` on an `O_PATH` symlink `dirfd` (`-ENOENT` if that file is not a symlink, as Linux); copies `min(target.len(), bufsiz)` bytes **without** trailing NUL and returns bytes copied |
| 268 | `fchmodat(dirfd, path, mode)` | **3-argument Linux x86-64 syscall** (never reads `r10`, which `musl` leaves uninitialized); follows symlinks and updates mode bits and `ctime` (`-EROFS` on read-only fs) |
| 269 | `faccessat(dirfd, path, mode)` | **3-argument Linux x86-64 syscall** (never reads `r10`); shim to `faccessat2(dirfd, path, mode, 0)` |
| 280 | `utimensat(dirfd, path, times, flags)` | supports `path == NULL` (operates on `dirfd` for `musl` `futimens`: any flag `-EINVAL`, an `O_PATH` or closed fd `-EBADF`; with `AT_FDCWD`, `-EFAULT`), `AT_SYMLINK_NOFOLLOW`, `AT_EMPTY_PATH`, `times == NULL` (both set to current realtime), `UTIME_NOW` (`0x3fff_ffff`), `UTIME_OMIT` (`0x3fff_fffe`; both omitted returns `0` before the path or fd is looked at); an invalid `tv_nsec` (negative, or `>= 1_000_000_000`, and not a sentinel) is `-EINVAL`, checked after the lookup and before `-EROFS` |
| 316 | `renameat2(olddirfd, old, newdirfd, new, flags)` | atomic rename supporting `0`, `RENAME_NOREPLACE` (1 → `-EEXIST`), and `RENAME_EXCHANGE` (2 → atomic swap, `-ENOENT` if either missing); same-inode `rename(a, a)` is a no-op (`0`, or `-EEXIST` with `RENAME_NOREPLACE`); handles open-unlinked file and empty-dir target overwrite via the same detach/orphan path as `unlink`/`rmdir` (section 9); rejects unknown flags, `RENAME_WHITEOUT` or both together (`-EINVAL`, before the paths), cross-mount (`-EXDEV`, ahead of the `-EBUSY` below), `/`, `.` or `..` as either name (`-EBUSY`; `-EEXIST` on the new side with `RENAME_NOREPLACE`), mountpoint overwrite (`-EBUSY`), dir-over-non-dir, or a trailing `/` on either name when the source isn't a directory (`-ENOTDIR`, the slash checked before the types), non-dir-over-dir (`-EISDIR`), a new name that is an ancestor of the source's directory (`-ENOTEMPTY`); the checks in `do_renameat2`'s order (section 9), non-empty target dir (`-ENOTEMPTY`), and ancestor-into-descendant (`-EINVAL`) |
| 332 | `statx(dirfd, path, flags, mask, buf)` | supports `AT_SYMLINK_NOFOLLOW`, `AT_EMPTY_PATH` (a `NULL` path with it is `""`, as for `newfstatat`), `AT_NO_AUTOMOUNT`, and `AT_STATX_SYNC_TYPE` (`AS_STAT`/`FORCE_SYNC`/`DONT_SYNC`, ignored); `-EINVAL` for both sync bits together or for `STATX__RESERVED` in `mask`; fills `struct statx` (`stx_blksize`, `stx_nlink`, `stx_uid`, `stx_gid`, `stx_mode`, `stx_ino`, `stx_size`, `stx_blocks`, `stx_atime`/`stx_mtime`/`stx_ctime`, `stx_rdev_*`, `stx_dev_*`; no `stx_btime`), `stx_mask` always `STATX_BASIC_STATS` whatever `mask` asks |
| 439 | `faccessat2(dirfd, path, mode, flags)` | supports `AT_SYMLINK_NOFOLLOW`, `AT_EACCESS` and `AT_EMPTY_PATH` (the three flags it has taken since Linux 5.8 added it; a `NULL` path is `-EFAULT`, even with `AT_EMPTY_PATH`); checks existence (`F_OK`) and permission bits as root (`uid == 0`): `R_OK` always; `W_OK` too, except `-EROFS` on a read-only mount (special files exempt: `InodeType::is_special`, as `openat` agrees; #291); `X_OK` if any execute bit is set, or always on a directory; an invalid mode or flag is `-EINVAL` before the path is looked at |
| 452 | `fchmodat2(dirfd, path, mode, flags)` | 4-argument Linux 6.6+ variant accepting `AT_SYMLINK_NOFOLLOW` and `AT_EMPTY_PATH` (unknown flags → `-EINVAL`); returns `-EOPNOTSUPP` **only when the resolved target is a symlink**, otherwise updates mode bits and `ctime` normally (`-EROFS` on read-only fs) |

#### M8 processes, threads, signals, `futex`, pipes and PTYs (designed in #336)

The table is pinned from #337's census of the M8 gate workloads (ash,
gcc, g++ with a `.so`, make `-j1`/`-j2`, git, sqlite3, vim scripted and
over a pty, pthreads, signals; `tests/linux/m8-census.toml`, read by
`PYTHONPATH=tools/harness uv run python -m lumen_harness.syscalls report`),
plus the rest of each family the gate needs and what musl's `libc-test`
core (D4) calls. Every handler follows section 8's three rules.
"Interrupted" below means B2's wait returned `Interrupted`, and names the
restart code the handler returns.

**M8's issues.** The labels every M8 section uses, filed from this design
once it merges (epic #335). The Phase A crates (A3–A8), the census (A2),
the developer image (A9) and libc-test (A10) are merged, so no row
depends on them.

| Label | Scope | Depends on | Owns (interfaces; tests) | Sections |
|---|---|---|---|---|
| B0 | the skeleton | M7 done | `M8_SYSCALLS`, the stub modules below, `syscalls.py`'s `PLANNED` changes and `ENOSYS_BY_DECISION`, empty scenario files, `tools/linux-m8/`; `m8_abi_constants_match_spec`, `test_m8_table_covers_the_census` | 8, 15 |
| B1 | the process/thread split, behaviour unchanged | M7 done | `Process`, `UserThread`, `UserThreadState::linux`, `PROCS`, `Files::users`, tids (`ProcTree::add_thread`/`remove_thread`), pid 1 (`linux::init`), the `ps` shell command; `procs_lookup_and_lifetime`, `procs_leave_no_heap_behind`, `identity_syscalls_name_the_process_and_thread`, `sigaction_and_sigprocmask_round_trip`, `cargo:lumen-proc`, all M6/M7 scenarios unchanged but for `test_mm`'s boot heap ceiling (pid 1's allocations) | 8 |
| B2 | the blocking wait and the syscall-exit hook | B1 | `wait_interruptible`/`wait_killable`, `kick`, `Thread::fatal_pending`, the hook, `restart_syscall`, `raise_for_test`, `user::read_u32_nofault`/`cmpxchg_u32`/`fault_in_writable`; the wait, restart and nofault ktests, `harness:test_audit::test_nofault_fixup_section_is_read_only_and_in_rodata` | 6, 8, 15 |
| B3 | the readiness interface | B0, B2 | `PollMask`, `PollWaker`, `Pollable`, `PollHead`, `VfsInode::pollable`; `poll`/`ppoll`/`select`/`pselect6` over M7's file kinds; `poll_masks_of_each_file_kind`, `poll_rechecks_after_a_wake`, `poll_resume_honours_the_saved_deadline`, `ppoll_installs_its_mask_and_writes_back_when_interrupted`, `test_poll_and_select_on_console` | 10 |
| B4 | frame reference counts and copy-on-write | M7 done | `frame::REFS`, `frame::RefTable` (lumen-mm), `frame::alloc_user`, `AddressSpace::users`, `fork_copy`, `Backing::Anonymous { shared }`; `refcount_and_cow_split`, `mprotect_keeps_a_shared_frame_read_only`, `named_reservations_are_complete` (extended), `fork_copy_out_of_frames_rolls_back`, `fork_copy_preserves_files_images_and_private_copies` | 5, 8 |
| B5 | the TLB shootdown IPI | B2 | vector `0x3e` (`idt::TLB_SHOOTDOWN`), `AddressSpace::active`, `PerCpu::loaded_space`, `tlb::shoot_down`, the shootdown warning's `HOST_TIMED` entry; `mm::tlb::shootdown_across_cpus` (its probes use B2's `read_u32_nofault`) | 5, 9 |
| B6 | SMP migration, priorities, affinity | B5, C1 | `on_cpu`, `spawn_migratable`, `spawn_fork_child` migratable (C1 gives it the `UserThread`), `slice_ticks`; the migration and slice ktests | 6 |
| B7 | truncate reaching every address space | B1, B4, B5 | the inode's `Mappers`, the registering `Mapped` entry, `invalidate_file_mappings`, `mmap_file(self: &Arc<Self>)`; `truncate_unmaps_every_address_space`, `fork_racing_a_truncate_copies_nothing_past_eof`, `mappers_multiset_keeps_registration_across_late_drop`, `meta_truncate_and_open_trunc_unmap_a_second_address_space`, `file_vm_writable_maps_balance_through_mprotect_unmap_and_teardown`'s mapper counts | 5 |
| B8 | the five M8 hazards | B1 | `OpenFile::pos`, `FdTable::reserve`, `PosixWaitGraph`'s key; the hazard ktests | 8, 9 |
| C1 | `fork`, `vfork`, exit, zombies, `wait4`/`waitid`, reparenting | B0, B1, B2, B4 | `spawn_fork_child` taking the `UserThread`, pinned to the caller's CPU (B6 makes it migratable), `Files::users` across `fork`; `test_fork_wait_and_exit_status` | 8 |
| C2 | `execve`, single-threaded, `#!`, the streaming loader | B0, C1 | lumen-elf's header split, `ImageSource`, the stack-size parameter; `test_fork_cow_and_exec_shebang` (its single-threaded checks) | 5, 8 |
| C3 | threads. This issue's PR models every de_thread/timer interleaving listed here against Linux's code before implementation, as a host-tested state machine where practical. | B0, B5, B6, C1, C2, C4, C5, C6 | `clone`'s thread flags, `de_thread`, `process::EXEC_DELAY`, the tid exchange, `MAX_STACKS`; `test_pthread_create_and_join`, `clone_racing_a_group_exit_never_escapes`, `de_thread_refuses_a_racing_group_stop`, `exit_group_during_de_thread_ends_only_itself`, `exit_retargets_signals_and_completes_a_stop` | 6, 8 |
| C4 | `futex`, robust lists, sleeps | B0, B1, B2, C1, C2 | `FutexKey`, the buckets, the `clear_child_tid` wake; `test_futex_timeouts_and_nanosleep` | 8 |
| C5 | signal delivery and faults as signals | B0, B1, B2 | frames, `rt_sigreturn`, `sigaltstack`, `kill`/`tkill`/`tgkill`, `rt_sigqueueinfo`; `test_signals_and_sigaltstack` (its `EPERM` check runs two `linux` programs at once) | 5, 6, 8 |
| C6 | restarts, signal waits, stop and continue | B0, B3, C1, C4, C5 | `pause`, `rt_sigsuspend`/`rt_sigtimedwait`/`rt_sigpending`, `SIGCHLD`, stops; `test_sigchld_and_sigint_interrupt_wait` | 6, 8 |
| C7 | pipes, FIFOs, `dup*`, `SIGPIPE` | B0, B3, B8, C1, C2, C5 | `PipeObject`, the FIFO slot, `KernelFile::end_refs`, `IoFile` (`io_refs` in ktests), `mknod_at`; `test_pipes_and_poll`, `poll_wakes_on_a_pipe_write`, `pipe_end_outlives_its_close_while_a_write_is_in_flight`, `fifo_reopen_during_a_blocked_read_reaches_that_reader` | 8, 9, 10 |
| C8 | sessions and process groups | B0, C1, C3, C5 | `test_sessions_and_process_groups` | 8 |
| C9 | PTYs, controlling terminals, job control | B0, C6, C7, C8 | devpts, `Tty`; `test_pty_termios_and_job_control` | 8, 9, 10 |
| C10 | the console as a tty | C2, C9 | the console's `Tty` and input thread; `test_console_ash_job_control` | 10 |
| C11 | multithreaded and cross-CPU scenarios | C3, C5, C12 | `test_cross_cpu_munmap_and_mprotect_shootdown`, `test_futex_and_pthread_mutex_stress`, `test_multithreaded_workload_uses_all_cpus` | 5, 6, 15 |
| C12 | the small syscalls and the census's leftovers | B0, B1, B6, C1, C2 | `FileSystem::statfs`, `RLIMIT_STACK` writes, the per-thread tick counts; `test_small_syscalls` | 6, 8 |
| C13 | `setitimer`/`alarm`/`getitimer`, POSIX timers (new: not in the epic's first list). This issue's PR models every de_thread/timer interleaving listed here against Linux's code before implementation, as a host-tested state machine where practical. | C2, C3, C5, C6 | `TIMERS` (keyed by instance), the timers thread, `PendingSignals::discard_if` and `PendingSignals::any`; `test_interval_and_posix_timers` | 8 |
| D1 | gate: ash scripts and pipelines | C1, C2, C6, C7 | `test_ash_scripts_and_pipelines` | 15 |
| D2 | gate: gcc/g++ and make | C1–C7, C12 | `mem`'s low-water mark; `low_water_mark_tracks_the_minimum`, `test_gcc_gpp_compile_cpp_and_shared_lib` | 15 |
| D3 | gate: git, sqlite3, vim over a PTY | C1–C9, C12, C13 | `test_local_git_and_sqlite3`, `test_vim_edits_a_file_over_pty` | 15 |
| D4 | gate: musl's libc-test | C1–C7, C12 | the harness's timeout class and `flaky` kind; `harness:test_libctest::test_a_timed_out_test_is_run_once_more`, `harness:test_libctest::test_a_flaky_entry_covers_a_fail_or_timeout_never_an_error`, `test_musl_libc_test_core` | 15 |

**Where the code lives.** `Process`, `UserThread` and `PROCS` in
`kernel/src/linux/process.rs` (B1; C1 and B8 edit it too); the futex
table in `kernel/src/linux/futex.rs` (C4); `TIMERS` and the timers thread
in `kernel/src/linux/timer.rs` (C13); `PipeObject` in
`kernel/src/linux/pipe.rs` (C7); `Tty` and devpts in `kernel/src/tty.rs`
and `kernel/src/vfs/devpts.rs` (C9, C10). The files several issues edit:
`sched.rs` (B1, B2, B6), the syscall-exit hook (B2, C5), `vfs/devfs.rs`
(C9, C10), `linux/process.rs` (B1, B8, C1).

**B0's stubs.** A row whose syscall M6 or M7 already dispatches keeps its
handler where it is (or B0 moves it, unchanged, into the cluster below);
only the new numbers get typed `-ENOSYS` stubs, each issue in the last
column replacing its own. The clusters: `syscall/task.rs` (`clone`,
`fork`, `vfork`, `execve`, `wait4`, `waitid`, the session and group calls;
`exit`, `exit_group`, `getpid`, `gettid`, `getppid`, `set_tid_address`
moved there), `syscall/signal.rs` (`rt_sigreturn`, the new `rt_sig*`
calls, `sigaltstack`, `kill`, `tkill`, `tgkill`, `pause`,
`restart_syscall`; M6's `rt_sigaction` and `rt_sigprocmask` moved there),
`syscall/futex.rs` (`futex`, the robust-list calls, `nanosleep`,
`clock_nanosleep`), `syscall/poll.rs` (`ppoll`, `select`, `pselect6`;
M6's `poll` moved there), `syscall/pipe.rs` (`pipe`, `pipe2`, `dup`,
`dup2`, `dup3`, `mknod`, `mknodat`), `syscall/timer.rs` (`alarm`,
`getitimer`, `setitimer`, the `timer_*` calls) and `syscall/sys.rs`
(`sched_*`, priorities, `getrusage`, `sysinfo`, `prctl`, `statfs`,
`fstatfs`, `clock_getres`, `getcpu`, `setfsuid`, `setfsgid`). B0 appends
`M8_SYSCALLS` to `kernel/src/linux/mod.rs` like `M7_SYSCALLS`, and
`ktest:lumen_kernel::linux::m8_abi_constants_match_spec` pins every
number, flag, errno and struct layout below from `linux-raw-sys`. In
`syscalls.py`, `PLANNED["M8"]` gains `sysinfo` (from M10), `pause`,
`alarm`, `getitimer`, `setitimer`, the five `timer_*` calls, `mknod`,
`mknodat`, `setfsuid`, `setfsgid`, `getcpu`, `set_robust_list`,
`get_robust_list`, `rt_sigqueueinfo`, `rt_tgsigqueueinfo` and
`restart_syscall`; `PLANNED["M10"]` loses `sysinfo`; and a new
`ENOSYS_BY_DECISION` holds the "Stays `-ENOSYS`" list below.

| # | Syscall | M8 behaviour | Issue |
|---|---|---|---|
| 7 | `poll(fds, nfds, timeout_ms)` | every descriptor through the readiness interface (section 10); `nfds` is the low 32 bits, past `RLIMIT_NOFILE` (1024) `-EINVAL`; a bad `fds` `-EFAULT`; `timeout_ms < 0` waits forever; per entry: `fd < 0` gives `revents` 0, a closed or `O_PATH` fd `POLLNVAL`, else the mask masked by `events \| POLLERR \| POLLHUP`; only each entry's `revents` is written back, including after an interrupt; an interrupted `poll` with an unwritable `fds` is `-EFAULT` with no restart block; returns how many entries have `revents`, 0 on timeout; interrupted `-ERESTART_RESTARTBLOCK` (the deadline in the restart block, stored and returned through `Err(linux::signal::interrupted_with(block))`), as Linux's `poll` | B3 |
| 13 | `rt_sigaction(sig, act, oldact, sigsetsize)` | `sigsetsize != 8` is `-EINVAL`; copy `act` first (`-EFAULT` precedes invalid `sig`); `sig` outside 1..=64 is `-EINVAL`; a non-null `act` for `SIGKILL`/`SIGSTOP` is `-EINVAL`; atomically swap the action under `signal`, then copy `oldact` (its `-EFAULT` does not undo the install); unknown `sa_flags` bits are cleared before storing (and in `oldact`), as Linux's `do_sigaction` does; `SIG_IGN`, or `SIG_DFL` on a default-ignored signal, discards that signal's pending entries (`PendingSignals::disposition_changed`); `SA_RESTORER` is needed to deliver a handler (`frame::build`'s `MissingRestorer` forces `SIGSEGV`; musl always sets it); `SA_NOCLDWAIT` or `SIG_IGN` on `SIGCHLD` makes `ProcTree::exit` reap the children (`autoreap`) | C5 |
| 14 | `rt_sigprocmask(how, set, oldset, sigsetsize)` | `sigsetsize != 8` is `-EINVAL`; copy `set` first (`-EFAULT` precedes bad `how`); `Blocked::apply` (a bad `how` is `-EINVAL` when `set` is non-null), then copy `oldset` (its `-EFAULT` does not undo the mask); unblocking a pending signal sets `sigpending`, so it is delivered before the call returns | C5 |
| 15 | `rt_sigreturn()` | section 6, "Signals"; returns the restored `rax`; a frame `restore` refuses forces `SIGSEGV` | C5 |
| 16 | `ioctl(fd, req, arg)` | M7's, plus `FIONREAD` on a pipe (`Pipe::fionread`) and the tty requests on PTYs and on the console as a tty (section 10, including the census's `TCFLSH`) | C7, C9, C10 |
| 22 | `pipe(fds)` | `pipe2(fds, 0)` | C7 |
| 23 | `select(n, readfds, writefds, exceptfds, tv)` | `n < 0` `-EINVAL`, `n` capped at `MAX_FDS`; a set naming a closed fd `-EBADF`; a negative `tv` field `-EINVAL`; bad pointers `-EFAULT`; an `O_PATH` fd is open (`max_select_fd` uses `open_fds`) and answers `POLLNVAL` (`select_poll_one`), which sits in all three sets; any other descriptor is readable for `POLLIN_SET`, writable for `POLLOUT_SET`, exceptional for `POLLEX_SET` (`fs/select.c`'s sets: `POLLEX_SET` is `POLLPRI \| POLLNVAL`, and `POLLNVAL` is in all three; `POLLPRI` is never set on a pipe, PTY or the console, so the census's vim exception set on its pty is simply never ready); writes the time left back to `tv` (not for a zero timeout; a failed write-back keeps a ready count and turns `-ERESTARTNOHAND` into `-EINTR`, as Linux's `poll_select_finish`); interrupted `-ERESTARTNOHAND` | B3 |
| 24 | `sched_yield()` | `sched::yield_now`; 0 | C12 |
| 32 | `dup(oldfd)` | the lowest free fd, `FD_CLOEXEC` clear, looked up and installed in one hold of the FD table (section 9's fourth hazard); `-EBADF`, `-EMFILE` | C7 |
| 33 | `dup2(oldfd, newfd)` | `oldfd == newfd`: `newfd` if open, else `-EBADF`; `newfd >= MAX_FDS` `-EBADF`; a `newfd` reserved by an open still in progress `-EBUSY`, as Linux's `do_dup2`; a displaced descriptor closed through `process::release_fd` after the table's lock is released | C7 |
| 34 | `pause()` | waits, interruptible, for a signal; interrupted `-ERESTARTNOHAND` (musl's `pause` calls it on x86-64) | C6 |
| 35 | `nanosleep(req, rem)` | `-EFAULT`; `tv_sec < 0` or `tv_nsec` outside 0..1e9 `-EINVAL`; sleeps at least `req` on `CLOCK_MONOTONIC`; interrupted `-ERESTART_RESTARTBLOCK`, with the time left written to `rem` (if not `NULL`); if no time remains after interruption, returns 0 | C4 |
| 36 | `getitimer(which, value)` | `ITIMER_REAL`: the time left and the interval; `ITIMER_VIRTUAL`, `ITIMER_PROF` and anything else `-EINVAL` (Lumen has no CPU-time timers: a deviation); `-EFAULT` | C13 |
| 37 | `alarm(seconds)` | `setitimer(ITIMER_REAL)` for `seconds`, returning the old timer's seconds rounded as Linux's `alarm_setitimer` | C13 |
| 38 | `setitimer(which, new, old)` | `ITIMER_REAL` only (others `-EINVAL`, as `getitimer`); a `timeval` field out of range `-EINVAL`; `new` `NULL` disarms (Linux still accepts it); arms the process's `ITIMER_REAL`, which sends `SIGALRM` (`SI_KERNEL`) to the process at expiry and re-arms by its interval (musl's `alarm` is this call: the census's signals workload) | C13 |
| 39 | `getpid()` | the process's pid (the thread group id) | B1 |
| 40 | `sendfile(out, in, off, count)` | **`-ENOSYS`**: its one census call is busybox `cat`, which got `-EINVAL` on Linux too, and busybox falls back to `read`/`write` on any error (`libbb/copyfd.c`) | — |
| 56 | `clone(flags, newsp, parent_tid, child_tid, tls)` | section 8, "`clone`, `fork` and `vfork`" | C1, C3 |
| 57 | `fork()` | the same, `SIGCHLD` | C1 |
| 58 | `vfork()` | the same, `CLONE_VM \| CLONE_VFORK \| SIGCHLD` | C1 |
| 59 | `execve(path, argv, envp)` | section 8, "`execve`" | C2 |
| 60 | `exit(status)` | ends the calling thread only; the last thread's exit ends the process (section 8, "Exit") | C1, C3 |
| 61 | `wait4(pid, wstatus, options, rusage)` | section 8, "`wait4` and `waitid`" | C1, C6 |
| 62 | `kill(pid, sig)` | `pid > 0` the process, 0 the caller's group, -1 every process but pid 1 and the caller, `< -1` the group `-pid`; nothing found `-ESRCH` before checking `sig` outside 0..=64 (`-EINVAL`); `sig` 0 only checks; `SI_USER` with the caller's pid (`SenderClass::SiUser`) | C5, C8 |
| 72 | `fcntl(fd, cmd, arg)` | M7's, plus `F_SETPIPE_SZ` (1031: `Pipe::set_capacity` with `DEFAULT_MAX_CAPACITY`, 1 MiB, as the bound for everyone, `-EPERM` past it, a deviation for root, whom Linux lets past `pipe-max-size`; fewer bytes than are queued `-EBUSY`) and `F_GETPIPE_SZ` (1032), both `-EBADF` on anything but a pipe; `F_SETFL`'s `O_NONBLOCK` on pipes and ttys; `F_SETLKW` interrupted `-ERESTARTSYS` | B2, C7 |
| 73 | `flock(fd, op)` | M7's; a blocking request interrupted `-ERESTARTSYS` | B2 |
| 76, 77 | `truncate`, `ftruncate` | M7's (and `openat(O_TRUNC)`), invalidating the mappings of every address space that maps the file via `invalidate_file_mappings` (section 5) | B7 |
| 98 | `getrusage(who, usage)` | `RUSAGE_SELF` (0) the process's ticks, `RUSAGE_CHILDREN` (-1) its reaped children's, `RUSAGE_THREAD` (1) the caller's; any other `-EINVAL`; `ru_utime`/`ru_stime` from the timer interrupt's charge by the interrupted `CS` (1 ms ticks), every other field 0; `-EFAULT` | C12 |
| 99 | `sysinfo(info)` | **moved from M10** (gcc, g++, make and vim call it: 35 census calls): `uptime` from `time::now_ns`, the same clock as `CLOCK_BOOTTIME`, rounded up to whole seconds, `loads` 0, `totalram`/`freeram` from the frame allocator, `bufferram` the page cache's bytes, `sharedram`/`totalswap`/`freeswap`/`totalhigh`/`freehigh` 0, `procs` the live processes, `mem_unit` 1; `-EFAULT` | C12 |
| 109 | `setpgid(pid, pgid)` | `ProcTree::setpgid` | C8 |
| 110 | `getppid()` | the parent's pid; 1 for an orphan; 0 for a root started by `linux` | C1 |
| 111 | `getpgrp()` | the caller's process group | C8 |
| 112 | `setsid()` | `ProcTree::setsid`, clearing the controlling terminal | C8 |
| 121 | `getpgid(pid)` | `ProcTree::getpgid` | C8 |
| 122, 123 | `setfsuid(uid)`, `setfsgid(gid)` | return 0, the previous id (every process runs as root), and change nothing until M11's credentials (vim over the pty calls them: the census) | C12 |
| 124 | `getsid(pid)` | `ProcTree::getsid` | C8 |
| 127 | `rt_sigpending(set, sigsetsize)` | `sigsetsize > 8` `-EINVAL`; the pending signals (the thread's and the shared queue's) that the thread blocks, as Linux's `do_sigpending` | C6 |
| 128 | `rt_sigtimedwait(set, info, timeout, sigsetsize)` | `sigsetsize != 8` or a bad timeout `-EINVAL`; takes a wanted signal (`PendingSignals::dequeue_wanted`), else unblocks the wanted set for the wait (Linux's `real_blocked`) and waits, interruptible: a wanted signal returns its number and `siginfo`, the timeout `-EAGAIN`, another signal's handler `-EINTR` (vim's timer thread waits here: the census) | C6 |
| 129 | `rt_sigqueueinfo(tgid, sig, info)` | `SigInfo::from_user`; an `si_code >= 0` or `SI_TKILL` is `-EPERM` unless `tgid` is the caller's own tid, as Linux compares `task_pid_vnr(current)`; `SenderClass::Other` (`-EAGAIN` when the queue is full) | C5 |
| 130 | `rt_sigsuspend(mask, sigsetsize)` | `sigsetsize != 8` `-EINVAL`; keeps the mask in `saved_mask`, installs `mask`, waits, interruptible; always `-ERESTARTNOHAND` (the handler's frame saves `saved_mask`, so `rt_sigreturn` restores it) | C6 |
| 131 | `sigaltstack(ss, old)` | `AltStack::configure` and `status` (`StackError::errno`: `-EPERM` on the stack, `-ENOMEM` below `MINSIGSTKSZ`, `-EINVAL`); `-EFAULT` | C5 |
| 133 | `mknod(path, mode, dev)` | `mknodat(AT_FDCWD, path, mode, dev)` | C7 |
| 137 | `statfs(path, buf)` | resolves `path` as `stat` does, then the filesystem's `FileSystem::statfs` (a lumen-vfs interface addition, C12): `f_type` (`EXT4_SUPER_MAGIC` 0xEF53, `TMPFS_MAGIC` 0x01021994 for tmpfs and devfs, `PROC_SUPER_MAGIC` 0x9FA0), `f_bsize` and `f_frsize` 4096, block and inode counts (ext4's superblock; a tmpfs mount's quota), `f_fsid` from the dev id, `f_namelen` 255, `f_flags` `ST_VALID` with the mount's flags (`ST_RDONLY` on a read-only mount), as Linux's `calculate_f_flags`; `-EFAULT` | C12 |
| 138 | `fstatfs(fd, buf)` | the same for a descriptor's file (`O_PATH` too) | C12 |
| 140 | `getpriority(which, who)` | `PRIO_PROCESS`: one thread (0: the caller; else the thread `who` names, a pid naming its leader), as Linux's `find_task_by_vpid`; `PRIO_PGRP` and `PRIO_USER` (uid 0: every process): every thread of the matching processes; an exited leader retains its nice until reap (including zombie reads and writes); returns `20 - nice` of the highest-priority match, as the Linux syscall does (libc turns it back); `-ESRCH` if none, a bad `which` `-EINVAL` | C12 |
| 141 | `setpriority(which, who, prio)` | the same targets (`PRIO_PROCESS` one thread, the others every matching thread); `prio` clamped to -20..=19 into B6's `nice`; lowering it is allowed (root) | C12 |
| 157 | `prctl(option, ...)` | `PR_SET_NAME` (15): reads at most 15 bytes, stopping at NUL, then stores a zero-filled 16-byte raw comm and its sanitized display together in `TaskName`; fork inherits both; `PR_GET_NAME` (16): the 16 raw bytes; every other option `-EINVAL` | C12 |
| 186 | `gettid()` | the thread's tid | B1 |
| 200 | `tkill(tid, sig)` | `tid <= 0` `-EINVAL`; no such thread `-ESRCH`; `SI_TKILL` (`SenderClass::Other`); musl's `raise` uses it (the census) | C5 |
| 202 | `futex(...)` | section 8, "`futex`" | C4 |
| 203 | `sched_setaffinity(tid, len, mask)` | `tid` 0: the caller; any thread; copies `min(len, 8)` bytes; a mask with no online CPU `-EINVAL`; `-ESRCH`, `-EFAULT`; the thread moves at its next switch (B6) | C12 |
| 204 | `sched_getaffinity(tid, len, mask)` | `len` below 8 or not a multiple of 8 `-EINVAL`, as Linux's checks for 8 CPUs; writes the mask ∩ online CPUs and returns 8 (git calls it: the census) | C12 |
| 218 | `set_tid_address(tidptr)` | M6's; the pointer is the thread's `clear_child_tid` | B1 |
| 219 | `restart_syscall()` | resumes the thread's restart block (B2); with none, `-EINTR` | B2 |
| 222 | `timer_create(clockid, sevp, timerid)` | `CLOCK_REALTIME`, `CLOCK_MONOTONIC`, `CLOCK_BOOTTIME` (the CPU-time and alarm clocks `-EINVAL`, a deviation); `sevp` `NULL` means `SIGEV_SIGNAL` with `SIGALRM`; `SIGEV_SIGNAL`, `SIGEV_NONE`, `SIGEV_THREAD_ID` (its tid a thread of the caller's process, else `-EINVAL`); a raw `SIGEV_THREAD` is taken as `SIGEV_SIGNAL`, as Linux's `good_sigevent` takes it (libc builds the thread on `SIGEV_THREAD_ID`, as musl's `timer_create` does); a signal outside 1..=64 `-EINVAL`; at most 32 timers a process (`-EAGAIN`, Lumen's cap); ids from 0 | C13 |
| 223 | `timer_settime(id, flags, new, old)` | `TIMER_ABSTIME` (1) read from `flags`, other bits ignored, as Linux's `do_timer_settime`; an unknown id or a bad `itimerspec` `-EINVAL`; arms (a zero `it_value` disarms); `old` gets the previous setting | C13 |
| 224 | `timer_gettime(id, cur)` | the time left and the interval; `-EINVAL`, `-EFAULT` | C13 |
| 225 | `timer_getoverrun(id)` | expiries while the timer's signal was still pending, at the last delivery (capped at `i32::MAX`) | C13 |
| 226 | `timer_delete(id)` | disarms and frees it, discarding its pending signal; `-EINVAL` | C13 |
| 229 | `clock_getres(clockid, res)` | the six clocks `clock_gettime` takes, each 1 ns (each read from the TSC; Linux reports its tick for the two coarse clocks, a deviation); `res` may be `NULL`; another clock `-EINVAL` | C12 |
| 230 | `clock_nanosleep(clockid, flags, req, rem)` | `CLOCK_REALTIME`, `CLOCK_MONOTONIC`, `CLOCK_BOOTTIME` (`CLOCK_THREAD_CPUTIME_ID`, `CLOCK_MONOTONIC_RAW` and the `*_COARSE` clocks `-EOPNOTSUPP`; `CLOCK_TAI`, `CLOCK_PROCESS_CPUTIME_ID`, `CLOCK_REALTIME_ALARM` (8) and `CLOCK_BOOTTIME_ALARM` (9) `-EINVAL`, unsupported deviations); `TIMER_ABSTIME`: until that time, interrupted `-ERESTARTNOHAND` and `rem` untouched; relative: as `nanosleep`, interrupted `-ERESTART_RESTARTBLOCK` (Linux's `hrtimer_nanosleep`) | C4 |
| 231 | `exit_group(status)` | ends every thread of the process (section 8, "Exit") | C1, C3 |
| 234 | `tgkill(tgid, tid, sig)` | `tgid <= 0` or `tid <= 0` `-EINVAL`; `tid` not in `tgid` `-ESRCH`; as `tkill` | C5 |
| 247 | `waitid(idtype, id, infop, options, rusage)` | section 8, "`wait4` and `waitid`" | C1, C6 |
| 259 | `mknodat(dirfd, path, mode, dev)` | the type checked first, before the path is looked up, as Linux's `may_mknod`: `S_IFDIR` `-EPERM`, a type that isn't `S_IFREG`, `S_IFCHR`, `S_IFBLK`, `S_IFIFO`, `S_IFSOCK` or 0 `-EINVAL`; then `S_IFIFO` (a FIFO, section 10) and `S_IFREG` or 0 (an empty regular file) are made with `mode & !umask`, and `S_IFCHR`, `S_IFBLK` and `S_IFSOCK` are `-EPERM` (a deviation: Linux allows any user to make socket nodes with directory access, and requires `CAP_MKNOD` only for character/block devices); a name in use `-EEXIST`; a read-only mount, devfs or procfs `-EROFS` (GNU make 4.4's `-j2` jobserver FIFO: the census); through `Start` (section 8's rule 2) and a new lumen-vfs operation, `mknod_at`, over a new `VfsInode::mknod` method (ext4 and tmpfs implement it; `fixed_namespace_ops!` grows from seven operations to eight, devfs and procfs answering `-EROFS`) | C7 |
| 270 | `pselect6(n, readfds, writefds, exceptfds, tsp, sig)` | as `select`, with a `timespec` (written back) and `sig`, a pointer to `{ const sigset_t *ss; size_t ss_len }`: with `ss` set, `ss_len != 8` `-EINVAL` and the mask swapped for the wait in one step (`saved_mask`), restored on return unless a handler runs, as Linux's `restore_saved_sigmask_unless`; interrupted `-ERESTARTNOHAND` (make waits here with `SIGCHLD` unblocked only for the wait: the census) | B3, C6 |
| 271 | `ppoll(fds, nfds, tsp, sigmask, sigsetsize)` | as `poll`, with `pselect6`'s mask rule and a `timespec`, the time left written back (Linux's `poll_select_finish`); interrupted `-ERESTARTNOHAND` | B3, C6 |
| 273, 274 | `set_robust_list`, `get_robust_list` | section 8, "`futex`" | C4 |
| 292 | `dup3(oldfd, newfd, flags)` | `flags` other than `O_CLOEXEC` `-EINVAL`; `oldfd == newfd` `-EINVAL`; else `dup2` with `FD_CLOEXEC` from `flags` | C7 |
| 293 | `pipe2(fds, flags)` | `O_CLOEXEC` and `O_NONBLOCK` (each end's own description); `O_DIRECT` (packet mode: a deviation) and any other flag `-EINVAL`; two slots reserved first (`-EMFILE`); a bad `fds` `-EFAULT`, both reservations dropped without installing either descriptor; section 10 | C7 |
| 297 | `rt_tgsigqueueinfo(tgid, tid, sig, info)` | copy `info` first (`-EFAULT`); then `tgid`/`tid <= 0` gives `-EINVAL`; otherwise as `rt_sigqueueinfo`, to one thread | C5 |
| 302 | `prlimit64(pid, res, new, old)` | M6's checks; a write to `RLIMIT_STACK` (`rlim_cur > rlim_max` `-EINVAL`) is kept and used by the next `execve` (its stack and argument budget, section 8); every other resource's write is still checked and ignored until M10. The census: gcc and g++ raise the stack to 64 MiB and `cc1`/`cc1plus` read it back | C12 |
| 309 | `getcpu(cpu, node, cache)` | the current CPU's index and node 0 (either pointer may be `NULL`); with no vDSO, musl's `sched_getcpu` makes this syscall | C12 |
| 324 | `membarrier(cmd, flags, cpu_id)` | **`-ENOSYS`**: musl registers `MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED` when it starts a thread, ignores the result, and its barrier falls back to signalling each thread (the census: git, pthread, vim over the pty) | — |

**Changed M6 and M7 rows.** `read`, `write`, `readv`, `writev` (0, 1, 19,
20) reach pipes, FIFOs and ttys (section 10): a blocking one interrupted
is `-ERESTARTSYS`, an `O_NONBLOCK` one that would block `-EAGAIN`; on a
regular file or directory they hold the open file description's position
lock (section 9's hazards). `close` (3) of a pipe end or tty runs its
release. `mmap` (9): `MAP_SHARED | MAP_ANONYMOUS` and `/dev/zero`'s shared
mappings stay shared across `fork` (section 5; M7 maps them as private).
`mprotect`, `munmap`, `brk` (10, 11, 12) shoot down other CPUs' TLB
entries. `rt_sigaction`, `rt_sigprocmask`, `ioctl`, `getpid`, `exit`,
`getppid`, `getpgrp`, `gettid`, `set_tid_address`, `fcntl`, `flock`,
`truncate`, `ftruncate`, `exit_group` and `prlimit64` as in the table.
`openat`'s `O_TRUNC` (257)
invalidates every address space's mappings, as `ftruncate`. `openat` and
`dup*` reserve their descriptor slot before they open (section 9).

**Stays `-ENOSYS` in M8, and why:** `sendfile` (40) and `membarrier` (324)
(the table); `clone3` (435) and `execveat` (322) (section 8); `futex_waitv`
(449) and `futex`'s `WAKE_OP` and PI ops; the scheduler-policy calls
`sched_setparam`, `sched_getparam`, `sched_setscheduler`,
`sched_getscheduler`, `sched_get_priority_max`, `sched_get_priority_min`
and `sched_rr_get_interval` (142–148: no real-time policies, no census
call; musl's pthread scheduling attributes then fail); `signalfd` and
`signalfd4` (282, 289: no census call, and no milestone plans them yet);
`times` (100) and the CPU-time clocks (no census call). This list is
B0's `ENOSYS_BY_DECISION`, and its census coverage check
(`harness:test_syscalls::test_m8_table_covers_the_census`) fails if a
census syscall is in neither `M6_SYSCALLS`, `M7_SYSCALLS`, `M8_SYSCALLS`
nor that list.

- **M9 (Networking & async I/O):** BSD sockets including both `accept` (43)
  and `accept4` (288), `AF_UNIX` (`socketpair`, `SCM_RIGHTS`), `epoll_*`
  (on section 10's readiness interface), `eventfd2`, and `timerfd_*`.
- **M10 (JIT VM & Tier-1 runtimes):** `mremap`, `madvise` (`MADV_DONTNEED`,
  `MADV_FREE`), `memfd_create`, `inotify_*`, writable `prlimit64` (every
  resource), and full `SIGURG`/`SIGSEGV` `ucontext_t` delivery (`sysinfo`
  moved to M8 with #337's census).

### ELF64 loader and initial stack (static M6 built #234/#237; dynamic `PT_INTERP` loader built #255, designed in #246; `dlopen` through ld-musl's file `mmap` built #255, M7)

- **Validation (`crates/lumen-elf`, host-tested):**
  checks ELF64 little-endian `ET_EXEC` (or `ET_DYN` PIE), `EM_X86_64`, no
  overlapping `PT_LOAD` byte ranges (`p_vaddr..p_vaddr + p_memsz`),
  `p_filesz <= p_memsz`, every mapped range within `USER_MIN..USER_MAX`, and no
  single `PT_LOAD` segment both writable and executable (W^X). Because real
  Alpine ELFs often place adjacent `R--` and `RW-` segments (RELRO) on a shared
  4 KiB boundary page, overlap is checked on byte ranges and a shared boundary
  page receives the union of both segments' permissions without zeroing or
  overwriting bytes already copied from the prior segment. Malformed binaries
  are rejected before any process slot is created, leaving kernel heap and
  frames unchanged.
- **Loading (M6):** creates an `AddressSpace`, maps each `PT_LOAD` segment with
  its `PF_R`/`PF_W`/`PF_X` rights, copies `p_filesz` bytes and zero-fills the
  `p_memsz - p_filesz` `.bss` tail, records the initial `brk` page-aligned
  above the highest segment, and maps the initial user stack VMA below
  `USER_STACK_TOP`.
- **Initial stack and register hygiene (`musl` System V ABI):** `%rsp` is
  16-byte-aligned at `_start` (`argc` at `[rsp]`, inserting an 8-byte pad above
  `auxv` whenever `1 + argc + 1 + envc + 1` is odd), and **every other GPR is
  zeroed** (especially `%rdx = 0`, which SysV AMD64 `_start` treats as an
  `atexit` function pointer). Stack contents top-down: `argv`/`envp` strings,
  `execfn` string, 16 random bytes for `AT_RANDOM` (from `crate::rng`), the
  auxiliary vector (`AT_PHDR` = mapped user VA `load_bias + PT_PHDR.p_vaddr` or
  `load_bias + seg.p_vaddr + (e_phoff - seg.p_offset)` in the `PT_LOAD`
  covering `e_phoff`, `AT_PHENT`, `AT_PHNUM`, `AT_PAGESZ = 4096`, `AT_ENTRY`,
  `AT_UID`/`AT_EUID`/`AT_GID`/`AT_EGID`, `AT_SECURE = 0`, `AT_CLKTCK = 100`,
  `AT_EXECFN`, `AT_RANDOM`, `AT_NULL`; `AT_BASE` is omitted or 0 for static
  PIE), `envp` pointers + null, `argv` pointers + null, and `argc` at `[rsp]`.
- **M6 scope for `busybox ash`:** M6 runs embedded/test static `musl` binaries
  (including `busybox ash -s` exercising built-in commands such as `echo`,
  `pwd`, `export`, arithmetic, loops, and `exit` over serial without job
  control). Interactive `lineedit`, pipes, and external subprocesses require
  `fork`/`pipe`/`dup2`/`wait4`/PTYs and arrive in M8.
- **Dynamic linking (`PT_INTERP` → `/lib/ld-musl-x86_64.so.1`; designed in #246, built in #255 (the loader copies segment bytes; the file-backed part is ld-musl's `mmap(fd)` of libraries); `dlopen` of `libm7.so` checked by `test_linux_user::test_dynamic_musl_binary_and_dlopen`, M7):**
  - **`crates/lumen-elf` parser extension:** `LoadedElf::parse_headers_with_pie_base(ehdr, phdrs, file_size, pie_base)`
    accepts an optional `PT_INTERP` header (at most one; rejects duplicate
    `PT_INTERP`, `p_filesz <= 1` (empty or NUL-only), `p_filesz > PATH_MAX`,
    out-of-file ranges), stores its `(offset, len)` inside lifetime-free
    `LoadedElf`, and exposes `interp_range()`. Separately read path bytes
    must pass `validate_interp` (trailing NUL, no interior NUL).
    Main dynamic executables may be `ET_DYN` (PIE, mapped at `PIE_BASE =
    0x0000_5555_5555_4000`, which Alpine ships by default) or `ET_EXEC`
    (`load_bias = 0`), and dynamic ELFs must have a mapped `AT_PHDR`
    (`phdr_vaddr.is_some()`, required by `ld-musl` to locate `PT_DYNAMIC`).
  - **Interpreter loading:** when the separately read `interp_range` bytes
    pass `validate_interp` (e.g.
    `"/lib/ld-musl-x86_64.so.1"`), the kernel resolves and reads the interpreter
    file from VFS (returning `-ENOENT` if absent, `-EACCES` if it is not a
    regular file with an execute bit, `-ELIBBAD` if invalid) and
    parses it with `ET_DYN` base `INTERP_BASE = 0x0000_7f00_0000_0000` (computing
    `load_bias = base.checked_sub(align_down(min_vaddr))`, matching Linux
    `load_elf_interp`; equals `INTERP_BASE` when `min_vaddr == 0`, as in
    `ld-musl`, placing it at and above `MMAP_TOP` and strictly below the
    user stack's guard page, which the loader enforces). The interpreter must
    be `ET_DYN` and must **not** itself carry `PT_INTERP` (rejected with
    `NestedInterp`). Both the main binary's and the interpreter's mapped page
    ranges are verified to be pairwise disjoint and disjoint from the user
    stack + guard page before committing any frames (`LayoutClash`,
    `-EEXIST`, as Linux's `MAP_FIXED_NOREPLACE` ELF mappings).
  - **M6 `test_bad_elf_rejected` compatibility:** static-only
    `AddressSpace::from_elf` continues to reject `PT_INTERP` (returning
    `ElfLoadError::DynamicInterpreterUnsupported`) so M6's `usertest bad_elf` /
    `test_bad_elf_rejected` passes unchanged, while `linux::exec::Exec` (#255)
    holds both sources open, parses bounded headers, and streams 4 KiB
    chunks into private frames through `AddressSpace::from_images_with_stack`;
    M7 adds tests
    for malformed/duplicate/empty/nested `PT_INTERP`.
  - **Auxiliary vector and entry point for dynamic binaries:**
    - `AT_PHDR`, `AT_PHENT`, `AT_PHNUM`, and `AT_ENTRY` describe the **main
      binary** (never the interpreter).
    - `AT_BASE` is set to `Some(interp_load_bias)` so `ld-musl` relocates
      itself before loading `DT_NEEDED` libraries.
    - Initial `brk` is page-aligned above the **main binary's** highest
      `PT_LOAD` segment.
    - Initial ring-3 `%rip` is set to the **interpreter's** mapped entry point
      (`interp_load_bias + interp.e_entry`), while direct invocation of
      `/lib/ld-musl-x86_64.so.1 <prog>` (an `ET_DYN` without `PT_INTERP`)
      continues to load at `PIE_BASE` with `AT_BASE` omitted.
  - **`ld-musl` & `dlopen`/`dlsym` invariants:** `ld-musl` deduplicates shared
    libraries by `(st_dev, st_ino)` from `fstat(2)`, reserves contiguous VA
    ranges via `mmap(..., PROT_NONE, MAP_PRIVATE, fd, 0)` followed by
    `MAP_FIXED` file-backed segment overlays and `MAP_ANONYMOUS | MAP_FIXED`
    `.bss` tails, protects `PT_GNU_RELRO` pages via sub-VMA `mprotect`, and
    reads `/proc/self/exe` for `$ORIGIN`. All of these invariants are satisfied
    directly by section 5 (`lumen-vm` + `PTE_SHARED` CoW) and section 9 (VFS
    unique `(st_dev, st_ino)` + `/proc/self/exe`).
  - **M7 scope for Alpine minirootfs:** M7 executes unmodified dynamic Alpine
    `/bin/busybox` directly (`ls`, `cp`, `mv`, `grep`, `sed`, `awk`,
    uncompressed `tar -cf/-xf`, and built-ins-only `ash -s`) plus dynamic
    `dlopen`/`dlsym` test binaries via `linux /path/to/bin [args...]`, one
    applet per `linux` line (`test_alpine::test_alpine_minirootfs_coreutils`;
    `tools/linux-m7/m7_test.rs` with `libm7.so` for `dlopen` and file `mmap`
    from ring 3). `ash` scripts running external commands, shell
    redirections (`dup`/`dup2`), and pipelines arrive in M8.

### Processes and threads (designed in #336, M8)

M6 and M7 ran one ring-3 thread per process, pinned to the CPU it started
on, with its Linux state in `ACTIVE_PROCS` (`kernel/src/linux/process.rs`:
a `SpinLock<BTreeMap<u64, ProcessState>>` keyed by the kernel thread's id;
B1 replaced it with the model below, one thread per process still).
M8 splits that state into processes and threads, keeps the process tree in
`lumen_proc::ProcTree` (#341), makes every blocking syscall interruptible,
and adds `fork`, `vfork`, threads, `execve`, `wait4`/`waitid`, `futex`,
timers, sessions and process groups. The issue labels are section 8's
table, "M8's issues". Pids and tids are `u32`s, as in lumen-proc; signals
are `lumen_signal::Signal`s. Every lock named here has its place in section
9's order, which this section doesn't restate.

#### The model (B1)

A `Process` is a Linux thread group: one pid and what its threads share. A
`UserThread` is one ring-3 thread's Linux identity; the scheduler's
existing per-thread user state, `UserThreadState`, holds it.

```rust
// kernel/src/linux/process.rs
pub struct Process {
    pid: AtomicU32,                          // set once, in fork's PROCS hold
    mm: SpinLock<Option<Arc<AddressSpace>>>, // the process's space (procfs, fork)
    files: SpinLock<Option<Arc<Files>>>,     // CLONE_FILES shares it
    files_access: FilesAccess,               // fd operations/reservations vs table replacement
    fs: SpinLock<Option<Arc<FsState>>>,      // cwd and umask; CLONE_FS shares it
    signal: SpinLock<SignalState>,
    live_threads: AtomicUsize,               // threads not yet through exit step 2
    exec_wait: WaitQueue,                    // de_thread's wait
    stopped: AtomicBool,                     // mirrors group_stop (section 6)
    stop_wait: WaitQueue,                    // the stop park (section 6)
    child_events: WaitQueue,                 // wait4/waitid sleep here
    child_seq: AtomicU64,                    // bumped on each child state change
    vfork_done: SpinLock<Option<Arc<VforkDone>>>, // a CLONE_VFORK child's
    exe_path: SpinLock<Option<String>>,      // /proc/<pid>/exe
    usage_slot: u64,                         // its RusageSlot's value
    children_ticks: [AtomicU64; 2],          // reaped children's user, system
    console: SpinLock<ConsoleState>,         // M7's console state, until C10
    allow_test_syscalls: bool,               // usertest's test syscalls (M6)
    exit: SpinLock<Option<UserExit>>,        // M7's end, first wins, until group_exit (C3)
}
pub struct Files {
    table: SpinLock<Shared>,                // slots and members, one leaf
    close_access: FilesAccess,              // shared close versus exclusive leave
}
struct Shared {
    fds: FdTable<FileRef>,                   // FileRef = Arc<KernelFile>
    members: Vec<u32>,                      // pid, or 0 before linking
}
pub struct FsState { inner: SpinLock<FsInner> }  // FsInner { cwd: Cwd, umask: u16 }
enum GroupEnd { Exit, Signal(Signal) }
pub struct SignalState {
    actions: [KernelSigaction; 64],
    pending: PendingSignals,                 // shared queue and one per thread
    threads: BTreeMap<u32, ThreadSignals>,   // by tid
    preferred: Option<ThreadSlot>,           // select_target's cursor
    group_exit: Option<GroupEnd>,            // first-wins exit/signal decision
    exec_waiter: Option<u32>,                // the tid in de_thread
    group_stop: Option<GroupStop>,           // signal, threads to park, generation
    timers: Timers,                          // ITIMER_REAL and POSIX timers (C13)
    ctty: Option<Arc<Tty>>,                  // controlling terminal (C9/C10)
    rlimit_stack: StackLimit,                 // private soft <= hard; only soft sizes the next execve
}
pub struct ThreadSignals {
    slot: ThreadSlot,                        // lumen_signal::pending's
    thread: Arc<SpinLock<Weak<sched::Thread>>>, // shared notification handle
    blocked: Blocked,
    real_blocked: Option<Blocked>,           // the mask before rt_sigtimedwait
    saved_mask: Option<Blocked>,             // rt_sigsuspend/ppoll/pselect6's
    alt_stack: AltStack,
    fault: Option<UserFault>,               // last synchronous diagnostic
    frame_failure: Option<FrameFailure>,    // failed delivery/sigreturn origin
    stop_pending: bool,                      // in group_stop's count, not parked
    stop_dequeued: bool,                     // a stop signal dequeued; SIGCONT clears
}
pub struct UserThread {
    tid: AtomicU32,                          // set in PROCS; execve may swap it
    process: Arc<Process>,
    clear_child_tid: AtomicU64,              // set_tid_address, CHILD_CLEARTID
    robust_list: AtomicU64,                  // set_robust_list's head (C4)
    kernel_thread: AtomicU64,                // its sched::Thread's id (ps, ktests)
    kernel_handle: Arc<SpinLock<Weak<sched::Thread>>>, // bound before runnable
}
```

B1 builds the fields its behaviour needs (`pid`, `mm`, `files`, `fs`,
`signal` with `actions` and each thread's `blocked`, `exe_path`,
`usage_slot`, `console`, `allow_test_syscalls`, `exit`; `UserThread`
but `robust_list`); each later issue adds its own.

- **One per-thread state.** `sched::Thread::user` is already a `UserSlot`
  holding `UserThreadState { address_space, fpu, fs_base, kernel_rsp,
  user_exit_rsp, pending_debug_trap }` (`kernel/src/sched.rs`): the
  context switch loads `CR3` from its `address_space`, and the `#PF` path
  reads it lock-free (`current_address_space()`). B1 adds to it
  `linux: Option<Arc<UserThread>>` (`None` for a ktest's `UserSpaceGuard`;
  set by root threads at registration; fork children get it from C1's
  `spawn_fork_child` at spawn, cleared by its takedown before
  `detach_user_space`; read lock-free) and the thread's restart block (B2: an
  `UnsafeCell<Option<RestartBlock>>` read and written only by its own
  thread with interrupts off, as `set_current_fs_base` writes `fs_base`),
  and nothing else: `fs_base`, the FPU area and the pending-`#DB`
  flag stay where they are. `Process::mm` is the process's reference to
  the same `Arc` every one of its threads holds in `address_space` (a
  thread `clone` hands the new thread the caller's); `execve`, which runs
  with one thread left, replaces both (below). A syscall finds its process
  through `UserThreadState::linux`, without a table lookup.
- **`UserSlot`'s `Sync`** rests on exclusive ownership: only the owning
  thread, with interrupts off, or the CPU that has claimed it through `on_cpu`
  (section 6's migration) touches the slot, and `Drop` runs when no other
  reference exists.
- **`spawn_user_on`** starts migratable roots. **`spawn_fork_child`** takes
  the `Arc<UserThread>`, address space and TLS base, inheriting the caller's
  FPU state, affinity and nice. Fork children may migrate within that mask. A thread spawned before its pid or
  tid exists (`fork`, `clone` step 1) gets them through `Process::pid` and
  `UserThread::tid`, atomics stored once in the `PROCS` hold that
  allocates them, before the thread's start gate opens; nothing reads them
  before.
- `sched::Thread` also gains `sigpending: AtomicBool` and `fatal_pending:
  AtomicBool` (B2: Linux's `TIF_SIGPENDING` and `__fatal_signal_pending`,
  section 8's blocking wait), `affinity` and
  `nice` (B6), and two tick counts, user and system, charged by the timer
  interrupt by the interrupted `CS` (C12). The ticks are counted per
  thread; a process's own totals (`RUSAGE_SELF`) are the sum over its live
  threads plus what its dead threads added at their exit, and its
  children's (`RUSAGE_CHILDREN`) are `children_ticks`.
- Lumen's `Process` holds what Linux splits between `signal_struct` and
  `sighand_struct` under one lock: `CLONE_SIGHAND` without `CLONE_THREAD`
  is refused (`-EINVAL`, a deviation, below), so the dispositions are
  always one thread group's.
- **A `Process` drops safely anywhere** (even in `finish_switch`, with
  interrupts off, when the scheduler lets go of its last thread):
  everything whose release takes a `SleepMutex` is taken out of `mm`,
  `files` and `fs` in exit step 3, in thread context, and a debug
  assertion in its `Drop` checks all three are `None` (pid 1 aside). The
  rest, an `Arc<Tty>` included (a PTY pair is released by its
  descriptions' closes, section 10), frees memory only.
- **`AddressSpace::users` (B4)** counts the processes using a space, per
  process, unlike Linux's per-task `mm_users`, apart from the `Arc` count
  (procfs readers, B7's mappers). Only `clone(CLONE_VM)` without
  `CLONE_THREAD` (`vfork`, `posix_spawn`) raises it; a process letting its
  space go (exit step 3, `execve`) lowers it, and the one that brings it
  to 0 runs `teardown` there, in process context (section 5).
- **`Files::users`** is the member vector's length, under the same leaf
  spinlock as descriptor slots. `new` and `share` reserve a 0 member;
  `Process::set_pid` calls `enrol` to replace one 0 without allocating,
  taking the files slot and table locks separately. `fork_copy(member)`
  starts with that member (0 for fork, the caller's pid for unshare).
  `leave(pid)` removes exactly one member. A departing process first
  sweeps its own POSIX locks on open inodes; the last member drains slots
  and releases FD references/flock outside the spinlock. The last `Arc`
  (including a procfs reader's) only frees memory and asserts no members
  remain. Unshare copies with the caller's pid then leaves the old table;
  it does not perform exit's pid-lock sweep. A table-wide `close_access`
  lease spans descriptor removal through inode-lock cleanup; `leave` takes
  it exclusively after the exit sweep. Thus a close either still scans the
  departing pid, or removes the descriptor after the exit sweep saw it.
  This reuses `FilesAccess` independently of the process's table-swap lease.
  Close takes process-read then table-read; leave holds neither a shared
  lease nor an inode lock on entry, and unshare drops its process-write
  lease before calling leave. The last-user drain does not reacquire read.

**The tables.** One kernel-wide table replaces `ACTIVE_PROCS`:

```rust
static PROCS: SpinLock<ProcTable>;
struct ProcTable {
    tree: lumen_proc::ProcTree,              // ProcTree::new(PID_MAX), PID_MAX = 32767
    procs: BTreeMap<u32, Arc<Process>>,      // live processes and zombies, by pid
    threads: BTreeMap<u32, Arc<UserThread>>, // live user threads, by tid
}
```

- `ProcTree::fork`, `setsid` and `setpgid` allocate infallibly under
  `PROCS` (B-tree inserts); an insert that can't allocate panics the
  kernel, as an insert under `ACTIVE_PROCS` can in M7. The heap grows by
  mapping frames, so this needs the frames to run out first; a fallible
  tree is an open question, not M8 work.
- A zombie keeps its `procs` entry until its `RusageSlot` comes back
  (`Report::Reaped`, `ExitAction::Released`, or `ExitAction::Notify` with
  `released`); the kernel finds the process by the pid each of those
  names. A slot's value is any unique number (`NEXT_USAGE_SLOT`, a
  counter): `fork` and `spawn_root` take the slot before the pid exists.
- `PID_MAX` is 32767, so pids run 2..=32767 as Linux's default `pid_max`
  hands out 1..32767 (lumen-proc wraps to 2, Linux to 300: lumen-proc's
  documented choice).

**Tids from the pid space.** A thread's tid is a number from `ProcTree`'s
allocator, so no tid is ever a live pid, pgid or sid, and the leader's tid
is the pid. lumen-proc didn't model threads before B1, which added, host-tested
in `cargo:lumen-proc`:
- `ProcTree::add_thread(pid) -> Result<u32, Error>`: a number as `fork`
  hands one out (`EAGAIN` when none is free; `ENOMEM` when room in the
  process's thread list can't be reserved, as `fork`'s children list),
  counted by `is_busy` while the thread lives; a pid that isn't live is a
  `Bug`;
- `ProcTree::remove_thread(pid, tid) -> Result<(), Error>`: a new `Bug`
  variant, `Bug::NotThread { pid, tid }`, for a tid the process doesn't
  have or for the leader's tid (the pid, which the process keeps until it
  is reaped);
- `ProcTree::exit` accepts a process with non-leader tids still
  registered and frees them with it, after its fallible steps (the kernel
  removes each as its thread exits, so only a bookkeeping bug leaves
  one); `ProcTree::exec` debug-asserts none is registered.

Because a live tid counts in `is_busy`, the pgid and sid calls see it as a
number in use: `tcsetpgrp(tid)` gives `EPERM` (the number isn't a group of
the caller's session) where it gave `ESRCH`, and `getpgid`, `setpgid` and
`kill` with a non-leader tid give `ESRCH` as before (lumen-proc looks
processes up by pid).

**Init and orphans.** At boot, `linux::init` (a function B1 adds in
`kernel/src/linux/init.rs`, re-exported by `linux`) calls `ProcTree::spawn_init`: pid 1 is a
kernel-owned process with no threads. It has a `PROCS.procs` entry, an
ordinary `Process` with no `UserThread` (its `mm` a placeholder empty
`AddressSpace` that is never loaded), whose `SIGCHLD` disposition is
`SIG_IGN`, so `ProcTree::exit`'s `autoreap(1)` is true. A `Notify` to
parent 1 is the ordinary case: its `released` is `Some`, so the child's
zombie and `procs` entry go at once, and the `child_seq` bump and
`child_events` wake reach no waiter. Every exit names pid 1 as the reaper,
so an orphan's `getppid` is 1, as on Linux. The `linux` shell command
starts its process with `ProcTree::spawn_root`: a root with no parent
(`getppid` 0, M6's row), whose end is an `ExitAction::Released` carrying
the exit status the command prints. A root's children outlive the `linux`
command, still running under pid 1, and the command doesn't wait for them
(section 15 says how the scenarios' leak counts treat them). Pid 1 never
exits and has no handlers: `kill(1, sig)` returns 0 and does nothing, as
Linux ignores a signal its init has no handler for (`SIGNAL_UNKILLABLE`),
and `kill(-1, sig)` skips it.

#### Exit (C1, C3)

`exit` (60) ends the calling thread only (M7 ended the whole process in
that arm); `exit_group` (231) and a fatal signal end the group:

1. The first end decision records `group_exit: Option<GroupEnd>` under
   `signal`: `Exit` for an ordinary exit, `Signal(sig)` for a fatal signal.
   `record_exit` inserts `Exit` only if unset, refines a prior `Signal(sig)`
   to that signal's matching fault diagnostic (otherwise `Signaled(sig)`),
   and inserts `Process::exit` only if unset. `take_exit` only takes that
   recorded outcome; it never merges later signal state. Once ending,
   sends succeed without queueing (including sends to zombies), after the
   target's existence check; a later SIGKILL cannot replace `Exited(n)`.
   C3 extends group teardown in a hold that
   re-checks first, as Linux's `do_group_exit` re-checks under `siglock`:
   a `group_exit` already recorded wins (this thread takes its status and
   goes to step 2); an `exec_waiter` naming another thread (`de_thread`,
   below) sends this thread to step 2 alone, recording nothing and kicking
   no one (Linux's code 0), even when a fatal signal dequeued before
   `de_thread`'s hold brought it here: that signal is consumed, Linux's
   code-0 path, not a gap. Otherwise, in the same hold, it records
   `group_exit`, clears `group_stop`, `stopped` (waking `stop_wait`) and
   every thread's `stop_pending` and `stop_dequeued`, its own included,
   and gives every other thread in `threads` `sigpending`, `fatal_pending`
   and `sched::kick` (out of an interruptible or killable wait, B2), as
   `zap_other_threads`. Each leaves at
   its next return-to-user check or interrupted wait, through step 2. A
   thread `clone` and a process `fork` check `group_exit` in the hold that
   links the new thread (below), so none escapes.
2. A thread's exit, in this order, as Linux's `exit_mm`
   (`mm_exit_exec_release`) comes before `exit_notify`:
   - walks its robust list (C4; below);
   - if `clear_child_tid` is set and another thread of the process is
     live or the space's `users` is above 1, writes 0 there (a fault is
     ignored) and wakes one waiter with a shared futex wake, as Linux's
     `mm_release` does only while `mm_users > 1` (C1 writes the 0; C4 adds
     the wake);
   - takes its `Arc<UserThread>` (and with it its `Arc<Process>`) out of
     `UserThreadState::linux` (`set_current_user_thread(None)`, leaving it
     `None`) for the steps after, then detaches from its
     address space (`sched::detach_user_space()`, which takes the whole
     `UserThreadState`: `kernel_root()` loaded, its `active` bit cleared,
     its `Arc<AddressSpace>` dropped), so a later `teardown`'s "`active`
     empty" is final. `detach_user_space` zeroes the CPU's
     `PerCpu::user_exit_rsp` (`kernel/src/sched.rs`), and
     `unwind_to_kernel` asserts it non-zero and `irq::depth()` 0
     (`kernel/src/arch/x86_64/syscall.rs`): a thread ending from ring 3
     (`exit`, `exit_group`, the hook's kill test, a fatal fault) first
     unwinds to `enter_ring3`'s caller with no lock held, and step 2 runs
     there, as `user.rs`'s runner already detaches only after
     `enter_ring3` returns; never before the unwind;
   - adds its tick counts to the process;
   - under `PROCS` then `signal`, in one hold: moves the `preferred`
     cursor off its slot, removes its `ThreadSignals` and `ThreadSlot`
     (`PendingSignals::remove_thread`), its `threads` entry and its tid
     (`remove_thread`, except the leader's), and decrements
     `live_threads`; the thread that takes it to 0 is the last out, a
     decision made in that same hold. Unless `group_exit` is set, the
     same hold does what Linux's `exit_signals` does then: a thread marked
     `stop_pending` clears the mark and lowers the group stop's count
     (`task_participate_group_stop`), and if that takes it to 0 this exit
     completes the stop (`ProcTree::stop` and its `StateChange`, acted on
     as section 6's last parker does, after `signal` is released, still
     under `PROCS`); and the shared queue's pending signals this thread
     didn't block are retargeted: each other live thread that doesn't
     block one of them gets `sigpending` and a non-fatal `kick`, until
     every such signal has a thread (`retarget_shared_pending`), so a
     process-directed signal `select_target` aimed at a thread that
     `de_thread` or `pthread_exit` ends isn't stranded. A caller in
     `de_thread`'s killable wait isn't woken by that kick and takes the
     signal at its first return to ring 3, in the new image. If
     `exec_waiter` is set and one thread is left, it wakes `exec_wait`
     (`de_thread`, below).
3. The last thread out ends the process, in thread context:
   - First, with no lock held, a session leader with a controlling
     terminal hangs up its session (C9/C10: section 10's PTYs, "Hangup").
   - It deletes its timers (C13): under `signal`, each armed one's
     `TIMERS` entry is removed, before `ProcTree::exit` can free its pid
     (the entries' keys name it, section 8's timers).
   - It takes `mm`, `files` and `fs` out of the `Process` (each `take()`
     under its leaf lock), lowers the space's `users` (tearing it down at
     0) and the table's `users` (draining it at 0, above), and drops the
     `FsState`.
   - Then, under `PROCS`, `ProcTree::exit(pid, status, Some(INIT_PID),
     autoreap)` and its `ExitOutcome`'s actions in the same hold: `Notify`
     bumps the parent's `child_seq`, wakes its `child_events`, sends it
     `SIGCHLD` (`si_code` from `WaitStatus::siginfo`, `si_pid`,
     `si_status`; under the parent's `signal`, which nests under `PROCS`
     with no child's `signal` held) unless its disposition is `SIG_IGN`,
     and with `released` drops the zombie's `procs` entry; `Released`
     drops the `procs` entry
     (and, for a root, hands the status to the `linux` command);
     `HangupThenContinue` sends `SIGHUP` then `SIGCONT` to `pgrp_members`.
   - `ExitError::NoMem` leaves the tree unchanged: the thread, past its
     point of no return, sleeps 1 ms (`time::sleep`, a busy wait where it
     can't block) and retries, with a WARN at most once a second (an INFO
     once it goes through), and never returns to user mode.
4. The status: `group_exit`'s if set, else the leader's own `exit` code,
   as Linux's `wait_task_zombie` reads `group_exit_code` only with
   `SIGNAL_GROUP_EXIT` set. A fatal signal's status is
   `ExitStatus::Killed { core: false }`: Lumen writes no core dump, and
   Linux sets the core bit only when one was written.

Tested by `scenario:test_linux_proc::test_fork_wait_and_exit_status`
(section 15).

Every sender of a signal runs in thread context with interrupts on
(syscalls, the fault path once it has turned interrupts on, the tty input
thread, the timers thread, exit); no interrupt handler sends one.

#### `clone`, `fork` and `vfork` (C1, C3)

`clone(flags, newsp, parent_tid, child_tid, tls)` (the x86-64 argument
order: `kernel/fork.c`'s default `SYSCALL_DEFINE5(clone, ...)`), `fork()`
(`SIGCHLD`) and `vfork()` (`CLONE_VM | CLONE_VFORK | SIGCHLD`) share one
path. Legacy `clone` reads only `lower_32_bits(clone_flags)`, as Linux's
`SYSCALL_DEFINE5(clone)` does; high bits are ignored. The low byte is the
exit signal (`CSIGNAL`). Checks, in
this order, each `-EINVAL`:
1. a flag outside the accepted set below;
2. `CLONE_THREAD` without `CLONE_SIGHAND`, then `CLONE_SIGHAND` without
   `CLONE_VM` (Linux's `copy_process` checks, in its order);
3. `CLONE_SIGHAND` without `CLONE_THREAD` (Lumen's deviation, above);
4. without `CLONE_THREAD`, an exit signal other than `SIGCHLD`
   (lumen-proc models `SIGCHLD` children only: a deviation; Linux accepts
   any signal or none and gives `__WCLONE` its meaning). With
   `CLONE_THREAD` the exit signal is ignored, as Linux does.

| Flag | M8 |
|---|---|
| `CLONE_VM` | share the `AddressSpace` (without `CLONE_THREAD`, raising its `users`), else copy it on write (B4) |
| `CLONE_FS` | share `FsState`, else copy cwd and umask |
| `CLONE_FILES` | share `Files` (without `CLONE_THREAD`, raising its `users`), else copy the table: each entry's `FileRef` cloned with its `cloexec`, and `inc_fd_ref` called on each description once per copied descriptor (`FdTable`'s hand-written `Clone` copies open entries, dropping reservations, but doesn't increment these counts), so a child's `close` of a pipe end or a `flock` holder drops only its own count; reserved slots are skipped |
| `CLONE_SIGHAND` | with `CLONE_THREAD` only |
| `CLONE_THREAD` | a new thread of the caller's process (C3); without both `CLONE_FILES` and `CLONE_FS` it is `-EINVAL`, a deviation (`files` and `fs` live on `Process`, so a thread can't have its own; musl always passes both, as do the census's flags) |
| `CLONE_VFORK` | the parent sleeps, killable, until the child's `execve` or exit (as Linux's `wait_for_vfork_done`, `TASK_KILLABLE`) |
| `CLONE_SETTLS` | the child's `fs_base` = `tls`; `tls >= USER_MAX` is `-EPERM`, as Linux's `set_new_tls` → `do_arch_prctl_64` (M6's `arch_prctl(ARCH_SET_FS)` answers that `-EINVAL`: an existing deviation) |
| `CLONE_PARENT_SETTID` | the child's tid written to `parent_tid` in the parent's memory before the child can run (it is spawned behind a start gate, released after the write), with no lock held; a fault is ignored, as Linux's `kernel_clone` writes it before `wake_up_new_task` and ignores `put_user`'s result (musl's `pthread_create` relies on the order) |
| `CLONE_CHILD_SETTID` | the child's tid written to `child_tid` in the child's memory by the child itself, with no lock held, before it first returns to user mode (a fault ignored, as Linux's `schedule_tail`) |
| `CLONE_CHILD_CLEARTID` | `clear_child_tid = child_tid` |
| `CLONE_SYSVSEM`, `CLONE_DETACHED`, `CLONE_IO`, `CLONE_PTRACE`, `CLONE_UNTRACED` | accepted, no effect (no System V semaphores, no ptrace; `CLONE_DETACHED` is ignored by Linux's `clone` too) |
| `CLONE_PARENT`, `CLONE_PIDFD`, `CLONE_NEW*`, `CLONE_NEWTIME` | `-EINVAL` (not modelled by lumen-proc; no pidfds; namespaces are M11's) |
| `CLONE_CLEAR_SIGHAND`, `CLONE_INTO_CGROUP` | `clone3`-only flags above bit 31; they cannot reach legacy `clone` after its 32-bit mask (`clone3` stays `-ENOSYS`) |

The census (#337) saw exactly two flag sets: `0x4111` (`posix_spawn`'s
`CLONE_VM | CLONE_VFORK | SIGCHLD`) and `0x7d0f00` (`pthread_create`'s
`VM | FS | FILES | SIGHAND | THREAD | SYSVSEM | SETTLS | PARENT_SETTID |
CHILD_CLEARTID | DETACHED`). **`clone3` (435) stays `-ENOSYS`:** no
census workload calls it (musl 1.2.5 doesn't), and glibc falls back to
`clone` on `-ENOSYS`. Tested by
`scenario:test_linux_proc::test_fork_wait_and_exit_status` and
`scenario:test_linux_proc::test_pthread_create_and_join` (section 15).

**A process clone** (no `CLONE_THREAD`). Capture-mode test processes
are refused with `-ENOSYS` and an INFO log until C7/C10 give children a
shared console; copying captured stdin and discarding child stdout is not
supported. `ProcessClone` carries validated flags. Every step that can fail comes
before `ProcTree::fork`, which can't be undone (lumen-proc has no
`abort_fork`):
1. allocate: the child's `PendingSignals::new(1, RT_QUEUE_CAP)`
   (`RT_QUEUE_CAP` = 1024 queued real-time signals per queue, `-EAGAIN`
   past it; `PendingError::NoMemory` is `-ENOMEM`), its `AddressSpace`
   (`fork_copy`, B4, or the shared one; `fork_copy` takes the child's Lock
   1 under the parent's, the one place two Lock 1s nest, and holds both
   through the copy and the child's mapper registration, so a truncate
   reaching the child waits for the copy, and one that snapshotted the
   mappers before the registration has already set the size the copy
   stops at, B7), its `Files` and `FsState`
   copies, a `RusageSlot` value, and its kernel thread
   (`sched::spawn_fork_child(..)`, held at a start gate;
   `MAX_STACKS`, section 6, gives `-EAGAIN`, Linux's errno for its thread
   limits). A failure frees what was made, in thread context, with no
   lock held (a failed `fork_copy` drops its child after Lock 1 is
   released);
2. under `PROCS` then the caller's `signal`, the checks a thread clone
   makes (below: `-EINTR` on `group_exit`, `-ERESTARTNOINTR` on
   `exec_waiter`, a `group_stop` or a signal pending for the caller, as
   Linux's `copy_process`), then `ProcTree::fork(parent, slot)` (`-EAGAIN`
   with no free pid, `-ENOMEM` from its reservation), the pid and tid
   stored, and the `procs` and `threads` inserts. Since `kill(-pgid)`
   sends under `PROCS`, either the caller has the signal pending here or
   the child is already in the group. A refusal frees step 1's
   allocations after the locks are released. `NewProcess` owns the partially
   filled process; `ParkedChild`, declared after it, cancels and joins before
   `NewProcess` releases resources on a refusal;
3. write `parent_tid`, then open the start gate.

The child's `SignalState` copies the dispositions, the caller's mask and
alternate stack (cleared only for `CLONE_VM` without `CLONE_VFORK`),
the controlling terminal and `rlimit_stack`; it has an
empty pending set and no timers (neither `timer_create`'s nor
`ITIMER_REAL`: not inherited, as on Linux). The child's registers are the
parent's at the `syscall`, with `rax` = 0 and `rsp` = `newsp` if
non-zero; it returns through `iretq` (section 6). The parent gets the
child's pid.

**A thread clone.** The kernel thread, behind its start gate, and the
`ThreadSlot` (`PendingSignals::add_thread`; `-ENOMEM`) come first; then,
in one `PROCS` → `signal` hold, `add_thread` for the tid, and the checks
Linux's `copy_process` makes under its `siglock`: a `group_exit` refuses
the clone with `-EINTR` (the caller is dying anyway), and an
`exec_waiter`, a `group_stop` or a signal pending for the caller with
`-ERESTARTNOINTR` (restarted once it has been handled); otherwise the
`threads` and `ThreadSignals` entries are linked and `live_threads`
raised in that hold, so the group-exit kicker sees the new thread (the
tid is stored in `UserThread::tid` there too). A
refusal undoes, in reverse order, after the hold: the tid
(`remove_thread`), the `ThreadSlot`, the kernel thread (never started).
The new thread has the caller's mask, no alternate stack (Linux clears it
for `CLONE_VM` without `CLONE_VFORK`), and the same `Process`.

**`vfork` and `posix_spawn`:** the child shares the parent's
`AddressSpace` (no copy; `users` raised) and the parent's thread waits,
killable, on the child's `vfork_done` (a one-shot flag and wait queue),
released by the child's `execve` at its point of no return (step 6,
before it installs its new address space, as Linux's
`mm_exit_exec_release` completes `vfork_done`) or by its exit (step 2,
when it detaches from the shared space). musl's `posix_spawn` reports
an `execve` failure through a `pipe2(O_CLOEXEC)` pipe (C7).

#### `execve` (C2)

`execve(path, argv, envp)` (59). `execveat` (322) stays `-ENOSYS`: no
census workload calls it, and musl's `fexecve` falls back to
`/proc/self/fd/N` on `-ENOSYS`. C2 builds the single-threaded path; C3
adds `de_thread` and the tid exchange, C4 the robust-list walk, C13 the
timers. In this order (each error before step 6 leaves the caller
untouched):

1. Copy `path` (`UserPath`, section 8's handler rules), resolve it from the
   cwd following symlinks: a missing file `-ENOENT`, a directory or a
   file with no execute bit or not regular `-EACCES` (M7's
   `exec::open_inode` rule). The resolved file is held open from here
   by an `OpenGuard` (`OpenGuard::try_new`, as `KeptFile` does), so an
   unlink can't free its inode while the exec streams it.
2. Count `argv` then `envp` without storing pointers: a bad word gives
   `-EFAULT`, `MAX_ARG_STRINGS` gives `-E2BIG`, and each iteration checks
   fatal pending state (`-EINTR`, consumed by syscall exit). Then start `lumen_elf::args::ArgBudget::new(argc, envc,
   limit)`, with Linux's `bprm_stack_limits`: `limit = max(min(6 MiB,
   rlimit_stack / 4), 128 KiB)`, 2 MiB at the default 8 MiB stack. Then
   charge and copy, in the budget's documented order: `execfn` (the path
   as given), `envp` last to first, `argv` last to first (each length from
   a bounded `strnlen_user`; 0, a fault, is `-EFAULT`), and, when `argc` is
   0, the one byte of the synthetic `""` `argv[0]`; `E2BIG` from any
   `BudgetError`. Only after this pointer budget succeeds may storage be
   allocated. Each vector uses a compact byte arena plus `Vec<u32>` ends:
   read pointers last-to-first for bounded strlen and charging, reserve the
   exact byte total, then re-read pointers to copy. A changed NULL or an
   unmapped word gives `-EFAULT`; both passes check fatal pending state.
3. Read the file's first `BINPRM_BUF_SIZE` (256) bytes through the page
   cache. On `#!` (`lumen_elf::shebang::parse`; a `ShebangError` is its
   errno, `-ENOEXEC`), in this order: Linux's check that the script's own
   path stays reachable (`BINPRM_FLAGS_PATH_INACCESSIBLE`, `-ENOENT`),
   which only an `execveat` of a descriptor can trip, so it never fires
   while `execveat` is `-ENOSYS`; `ArgBudget::rewrite` with
   `script_path` the execfn the first time and the previous `#!` line's
   interpreter string after that (`E2BIG`, `ENOMEM`); `Shebang::open_error`
   (`-EACCES` for an empty interpreter path); resolve and open the
   interpreter as in step 1 (`-ENOENT`, `-EACCES`); then
   `Nesting::follow_interpreter` (`-ELOOP` past `MAX_NESTING`); and back to
   this step for the interpreter.
4. An ELF: the header and program headers (at most 64 KiB, Linux's
   `load_elf_phdrs` limit) parsed by `lumen_elf::loader` (`-ENOEXEC` for a
   bad one; a bad `PT_INTERP` interpreter `-ELIBBAD`, a missing one
   `-ENOENT`, M7's rules). Neither ELF nor `#!`: `-ENOEXEC`.
   lumen-elf's header-split API (host-tested in `cargo:lumen-elf`) parses
   the bounded pieces the kernel reads:
   - `ElfHeader::parse(head) -> Result<ElfHeader, ElfLoadError>`, a public
     parse of the `EHDR_SIZE`-byte ELF header with private geometry and a
     `phdr_range()` accessor, bounded by `MAX_PHDR_BYTES` (64 KiB). The
     table can lie past the first 256 bytes; the supplied table slice must
     contain exactly that range's bytes;
   - `parse_headers`, `parse_headers_with_pie_base` and
     `parse_interp_headers`, each taking `(ehdr, phdrs, file_size)` and
     keeping its mode's load bias, with every segment's
     `p_offset + p_filesz` checked against `file_size` instead of a slice;
   - `LoadedElf::interp_range() -> Option<Range<u64>>`, the `PT_INTERP`
     bytes' file range (at most `PATH_MAX`); the kernel reads them and passes them to
     `validate_interp(bytes)`, which keeps today's checks (NUL-terminated,
     no inner NUL, `InvalidInterp`);
   - `LoadSegment::file_range()`, the
     segment's file bytes' range, which the loader copies from the page
     cache.
5. Build the new `AddressSpace` completely (`AddressSpace::from_images_with_stack`,
   the initial stack from the copied strings): `-ENOMEM` if frames or heap
   run out. The stack's size is the soft limit `rlimit_stack.soft()` clamped to 128 KiB..=256
   MiB, rounded up to a page by `StackSize` (section 5), whose argument
   budget also supplies step 2. Each `AddressSpace` retains the bottom.
   **No whole-file read:** the kernel's `ElfImage` (in
   `mm/user_vm.rs`, not lumen-elf; its `file: Option<&ImageFile>` stays)
   carries an `ImageSource` instead of the whole file's bytes,
   `ImageSource::File(OpenGuard)` (step 1's) or
   `ImageSource::Bytes(Cow<'a, [u8]>)` (a boot module or owned test image).
   The loader copies each `PT_LOAD` segment's file bytes from the page cache
   straight into the new frames, 4 KiB at a time, instead of M7's
   whole-file reads into one heap `Vec` (whose `READ_ALL_MAX`, 16 MiB,
   refuses `cc1plus`, 41.4 MB, and whose heap never shrinks: #344's
   numbers). A short read gives `ImageRead(VfsError::Io)` / `-EIO`;
   `ElfLoadError::NoMemory` converts to `OutOfMemory` / `-ENOMEM`. An
   executable's size is then bounded by memory alone, as on Linux;
   `READ_ALL_MAX` stays for `vfstest` and the other whole-file readers.
   The segments stay copied into private frames (M7's loader), not mapped
   from the page cache: that keeps an executable's pages out of the `ext4`
   cache's 64 MiB hard limit (#344: a file-backed loader would pin about
   32 MiB of `cc1plus` while it runs). Everything step 6 needs is
   allocated here too: the new `exe_path` string, and an empty table for
   an unshared `Files` (its 1024 slots) if the table's `users` is above 1
   now. Until C3 implements de_thread, a multithreaded or group-exiting
   process is refused with `-EAGAIN` before commit. `prepare_exec` packages
   the new mm, identity, task name and `PreparedFiles` token into `PreparedExec`.
6. The point of no return: C2's `commit_exec` is infallible; all storage
   was prepared in step 5. C3 extends the single-thread boundary below:
   - **`de_thread` (C3).** Under `signal`: if `group_exit` or another
     `exec_waiter` is set, `-EAGAIN` (Linux's `de_thread`: a concurrent
     exec or a group exit wins). Else set `exec_waiter` to the caller's
     tid (a flag of its own, not `group_exit`, so the caller survives);
     clear `group_stop`, `stopped` and every thread's `stop_pending` and
     `stop_dequeued`, the caller's included (so no mark reaches step 2's
     count or the new image), and wake `stop_wait`; and set a
     pending `SIGKILL` directly in each other thread's private queue (not
     through the send path, whose group-kill rule would end the caller
     too), with `sigpending`, `fatal_pending` (B2) and `kick`, as Linux's
     `zap_other_threads` zeroes `group_stop_count`, clears
     `JOBCTL_PENDING_MASK` on the caller and every sibling, and sets
     `SIGKILL` in each sibling's `pending` with `signal_wake_up(t, 1)`. A
     sibling in any killable wait (a stop park, a `vfork` wait) wakes, and
     the syscall-exit hook ends it through exit step 2 alone (section 6).
     The caller waits on `Process::exec_wait`, killable, until
     `live_threads` is 1 (Linux's `notify_count`); only a real group exit
     (a fatal signal sent to the process through the send path, which
     sets `group_exit`) or a `SIGKILL` sent to the caller itself ends that
     wait, and the caller with it. Either way the caller
     clears `exec_waiter` under `signal` when its wait ends. If the caller
     isn't the leader it takes the pid as its tid (Linux exchanges the
     tids): in one `PROCS` then `signal` hold, its `PROCS.threads` and
     `SignalState::threads` entries are re-keyed, its `UserThread::tid`
     stored and its old tid removed. The interleavings this bullet, exit
     steps 1 and 2 and section 6's hook and stop list (a stop starting in
     its two-hold gap, a fatal signal dequeued before this hold, a
     sibling's `exit_group`, a signal aimed at a sibling this kills, a
     `vfork` wait) are ktests, with `process::EXEC_DELAY`, a ktest-only
     delay knob between step 5 and this hold (section 15). This issue's
     PR models every de_thread/timer interleaving listed here against
     Linux's code before implementation, as a host-tested state machine
     where practical.
   - Still on the old address space, as Linux's `exec_mmap` runs
     `mm_exit_exec_release` first: walk the robust list
     (`futex_exit_exec_release` → `exit_robust_list`, C4) and clear its
     head; clear `clear_child_tid`, storing 0 and then (C4) waking one waiter
     only if the space's `users` is above 1 (a `vfork` parent sharing it), as
     Linux's `mm_release` stores only while `mm_users > 1`, which after
     `de_thread` holds only for a sharer. Before releasing a `vfork` parent,
     `commit_unshare(PreparedFiles)` installs the private files table and
     `finish_exec` closes its CLOEXEC descriptors. `Private` asserts
     `users() == 1`; `Copy` uses the prepared table if still shared, otherwise
     discards it. The exclusive `files_access` lease waits for reservations
     without spinlocks held. C3 must revisit this with de_thread concurrency.
     Then release the parent, so it cannot change the child's old shared table
     while the child is still copying or closing it.
   - Install the new space: with interrupts off, as
     `UserSpaceGuard::enter` does, replace the calling thread's
     `UserThreadState::address_space` and load its root with
     `AddressSpace::switch_loaded` (the `active` bits following, section 5),
     then store it in `Process::mm`, through the one writer
     `replace_current_mm`.
     Lower the old space's `users`, tearing it down at 0 through
     `teardown`, which waits until no CPU has it loaded (section 5).
   - `ProcTree::exec(pid)` marks even an unnamed exec; install the prepared
     optional `exe_path`, clearing the previous identity when absent.
     `finish_exec` also resets handled signals to default, keeping ignored
     dispositions, masks and pending queues, and clears the alternate stack
     and fault/frame_failure diagnostics. Reset FPU, `fs_base` and
     `restart_block`. C2's commit is infallible: all allocations precede it.
   - Timers (C13): delete the POSIX timers and drop every queued
     `SI_TIMER` signal (Linux's `posixtimer_exec`: `posixtimer_delete_timers`
     and `flush_itimer_signals`); `ITIMER_REAL` survives (an `alarm`
     outlives `execve`, as POSIX requires; it is freed only at exit).
   - The thread's name becomes the path's last component as given, the
     script's for a `#!` file (`ThreadName::from_path`, as `linux /path`
     does, and Linux's `comm`); `exe_path` the file finally loaded, the
     interpreter's path for a `#!` script, as Linux's `bprm_identity_file`
     makes `/proc/<pid>/exe`. `resolve_named_path` supplies the reached file's
     absolute identity, including for PT_INTERP: ordinary symlinks and dot
     components are resolved, and the final name is joined to its physical
     parent's path. Magic file links use their target identity, followed with
     the same symlink budget and checked against the reached inode; a detached
     file may retain its description's absolute ` (deleted)` path. Original bytes remain in script argv
     and AT_EXECFN. Identity retains raw bytes: `/proc/<pid>/exe` readlink
     returns them unchanged; maps text and comm escape non-UTF-8 paths.
     `NamedPath` separates the resolved inode from `identity: Result<Vec<u8>,
     VfsError>`. Only an identity ENOMEM aborts exec; any other naming failure
     is debug-logged and runs the reached file unnamed, with unnamed maps
     lines and `/proc/<pid>/exe` returning ENOENT. This includes fd execution
     after rename, where the descriptor still reaches the file but its
     recorded path is stale.
7. Return to the new image's entry through `iretq` with M6's initial
   register state (section 8, "ELF64 loader").

Both the shell loader and execve draw AT_RANDOM inside `Exec::load`/`load_with_stack`. If the
RNG pool is unseeded, a single warning accompanies the infallible TSC/time
fallback (not cryptographic entropy). Unlike Linux, executable sources have
no deny_write_access/ETXTBSY exclusion: holding an OpenGuard keeps an unlinked
inode alive but does not prevent writes or truncation while streaming.
Another Linux difference: executable identity is a stored path, not a pinned
inode/dentry; a rename or unlink after exec can leave it stale.

Tested by `scenario:test_linux_proc::test_fork_cow_and_exec_shebang`
(section 15).

#### `wait4` and `waitid` (C1, C6)

`wait4(pid, wstatus, options, rusage)` (61) and `waitid(idtype, id, infop,
options, rusage)` (247) take the request from
`ProcTree::wait4_request`/`waitid_request` under `PROCS` (Linux's argument
checks and errnos, lumen-proc's tests), then loop:
1. Read `child_seq`; under `PROCS`, `ProcTree::wait(request)`.
2. `Reported(report)`: release `PROCS`; for `Reaped`, add the child's
   ticks to `children_ticks` and drop its `procs` entry (taken out under
   the lock, dropped after it); write the status word (`WaitStatus::word`)
   or `siginfo_t` (`si_signo = SIGCHLD`, `si_code` and `si_status` from
   `WaitStatus::siginfo`, `si_pid`, `si_uid` 0) and `rusage` (below);
   return the pid (`waitid`: 0). A bad user pointer is `-EFAULT`, the
   child already reaped, as on Linux.
3. `NoneReady` with `WNOHANG`: return 0; `waitid` writes 0 to `infop`'s
   `si_signo`, `si_errno`, `si_code`, `si_pid`, `si_uid` and `si_status`,
   as Linux's `waitid` does. Only those six fields are written; padding,
   other union fields and `si_utime`/`si_stime` remain untouched. Linux's
   `SYSCALL_DEFINE5(waitid)` also writes these zero fields on an error
   (including `ECHILD`); copyout `EFAULT` supersedes the wait error.
4. Otherwise `child_events.wait_interruptible(None, || child_seq changed)`;
   `Interrupted` returns `-ERESTARTSYS`.

The census's calls use options 0 and `WNOHANG`; `WUNTRACED`/`WSTOPPED`
and `WCONTINUED` arrive with stops (C6, for ash's job control). `rusage`
(`struct rusage`): `ru_utime` and `ru_stime` from the reaped child's
totals plus its own reaped children's (Linux's `cutime`/`cstime`); every
other field 0 (Lumen counts nothing else). Tested by
`scenario:test_linux_proc::test_fork_wait_and_exit_status` (section 15).

#### One interruptible, killable blocking wait (B2)

M7's waits ignore signals (none are delivered). B2 adds one wait every
blocking syscall uses, in `kernel/src/sched.rs`:

```rust
pub enum Wake { Woken, TimedOut, Interrupted }
impl WaitQueue {
    /// Until `ready()` holds, the deadline passes, or any signal to deliver
    /// (or the group's exit) reaches the calling thread.
    pub fn wait_interruptible(&self, deadline: Option<u64>, ready: impl FnMut() -> bool) -> Wake;
    /// The same, but only a fatal signal (SIGKILL, the group's exit) ends it.
    pub fn wait_killable(&self, deadline: Option<u64>, ready: impl FnMut() -> bool) -> Wake;
}
pub fn kick(thread: &Thread);             // after its `sigpending` (and `fatal_pending`)
```

- **Order of the answers:** `ready()` first, under the queue's lock, as
  `wait_until` checks it (`Woken`, even with a signal pending: a wake that
  won the race is never lost, as lumen-futex's `AlreadyWoken`); then the
  deadline (`TimedOut`); then the pending check (`Interrupted`). `ready`
  may read atomics only: a waiter whose condition lives under a
  `SleepMutex` reads a sequence number its wakers bump after each change.
  `PipeObject` (section 10) has two, `read_seq` and `write_seq`,
  separating changes visible to readers and writers; `Tty` has `seq`.
  A waiter reads its counter under the object lock before releasing it
  and waits for it to change, as `wait4` reads `child_seq`.
- **No lost signal:** under the queue's lock the thread pushes itself, sets
  `Blocked` (marked interruptible or killable), and only then reads its
  `sigpending` (interruptible) or `fatal_pending` (killable), `SeqCst`; a
  sender stores `sigpending`, and for a fatal signal `fatal_pending`
  (`SeqCst`), and then calls `kick`, which moves the thread out of
  `Blocked` (if it is blocked interruptibly, or killably and its
  `fatal_pending` is set) as a wake does. One of the two sees the other's
  store: Linux's `set_current_state` before `signal_pending`. Every way
  out takes the thread's own entries off the queue and its CPU's
  sleepers, as `wait_until_or_deadline` does.
- **`fatal_pending`** is Linux's `__fatal_signal_pending` (`SIGKILL` in
  the thread's own queue), which `signal_wake_up(t, 1)` pairs with a
  `TASK_WAKEKILL` wake: `kick` alone can't tell that a `SIGKILL` set
  straight in a private queue (`de_thread`'s) is fatal. It is set by the
  send path's group-kill rule (section 6), exit step 1 (C1) and
  `de_thread` (C3) on each thread they end, before the kick, and is never
  cleared: a thread with it set never returns to ring 3.
- **`sigpending`** means "look at the pending signals" (Linux's
  `TIF_SIGPENDING`): a sender sets it on a thread that doesn't block the
  signal (`select_target`'s choice for a process-wide signal), and for a
  group exit or stop on every thread; the thread clears it in the
  syscall-exit hook, under `Process::signal`, when nothing deliverable is
  left.
- **Uninterruptible waits stay** for kernel-internal waits that must
  finish: page-cache and block-device I/O, `run_sized`, `join`, lock
  waits inside the kernel (Linux's `TASK_UNINTERRUPTIBLE`).
- **What B2 converts:** `F_SETLKW` and blocking `flock`
  (`process::wait_for_lock_release`: `Interrupted` is `-ERESTARTSYS`, as
  Linux's `locks_lock_inode_wait`). Console reads (serial and VT) and the
  serial input queue's wait move to C10 (#389); the console `poll` moves
  to B3 (#374). Each later issue uses the wait for its own calls; the
  syscall table names each one's restart code.
- **The syscall-exit hook (B2).** Restart codes (`-ERESTARTSYS`,
  `-ERESTARTNOINTR`, `-ERESTARTNOHAND`, `-ERESTART_RESTARTBLOCK`) are a
  handler's result for an interrupted wait, and none reaches user space.
  B2 adds the hook for syscall and non-IST interrupt returns to ring 3
  (section 6, "Signals"): it
  resolves a restart code with no handler to deliver
  (`lumen_signal::restart::decide` with `Handling::None`: a restart, `rax`
  = the original number and `rip` back 2 bytes, or `restart_syscall`
  (219) for `ERESTART_RESTARTBLOCK`, which resumes from the thread's
  restart block, a nanosleep's, a timed futex wait's or a `poll`'s
  deadline and arguments; with none, `-EINTR`, Linux's
  `do_no_restart_syscall`), and clears `sigpending` when nothing is
  deliverable. B2's tests raise a signal through the ktest-only
  `signal::raise_for_test(thread, sig)`. C5 delivers through this hook;
  the restart decision runs once, through
  `frame::build` when a handler is delivered (with its `SA_RESTART`) and
  through `decide` when none is.
- Tested by
  `ktest:lumen_kernel::sched::wait_interruptible_answers_woken_timed_out_and_interrupted`
  (each answer; a deadline answer is a floor, never early),
  `ktest:lumen_kernel::sched::kick_racing_a_block_is_never_lost` (a
  ktest-only delay knob, `sched::BLOCK_DELAY`, stalls before `Blocked`
  to check the waiter's pending read, after `Blocked` before the read
  (either side can end it), and after the read was false (only kick can
  queue it). Both blocked stalls assert `Ready` immediately after kick,
  before release. Timed-out stalls are retried, up to five attempts),
  `ktest:lumen_kernel::sched::fatal_kick_racing_a_killable_block_is_never_lost`
  (the same three stalls with `fatal_pending`, no `group_exit`, as
  `de_thread` gives; the after-read state assertion detects a late
  `wait_kind` store or a disabled killable kick),
  `ktest:lumen_kernel::sched::wait_killable_ignores_a_non_fatal_kick`,
  `ktest:lumen_kernel::linux::syscall::lock::setlkw_is_interrupted_by_a_signal`
  and `ktest:lumen_kernel::linux::syscall::restart_with_no_handler_restarts_or_resumes`
  (each restart code with `raise_for_test` and an ignored signal: a
  restart, `rax == 219` after the hook with the restart block's deadline
  unchanged and `restart_syscall` then resuming it, `EINTR` with no
  restart block),
  `ktest:lumen_kernel::user::syscall_return_restarts_once_through_sysret`
  (a real ring-3 syscall returns `-ERESTARTNOINTR`, restarts at the same
  two-byte instruction with its original number, and returns a marker;
  the second call sees `sigpending` cleared), and
  `ktest:lumen_kernel::sched::interrupt_return_to_kernel_keeps_sigpending`
  (at least three ticks returning to a kernel thread keep its flag).

#### `futex` (C4)

`futex(uaddr, op, val, timeout, uaddr2, val3)` (202) drives
`lumen_futex::FutexTable` through the protocols in that crate's
documentation (the value check under the bucket lock with faults off,
the retry after faulting the page in, the `BucketIndex` recheck on every
return from blocking). Its user accesses are section 6's step 6, "Non-faulting
user access":

- **Reading the word under the bucket lock** is `user::read_u32_nofault`.
  On `Fault`, `FUTEX_WAIT` drops the bucket lock, reads the word with a
  normal `copy_from_user` (which faults the page in; a bad address is
  `-EFAULT`), recomputes the key and retakes the lock to check again, as
  Linux's `futex_wait_setup` does after `futex_get_value_locked` fails (a
  plain `get_user`, then `retry`). `FUTEX_CMP_REQUEUE` does the same for
  `uaddr1` with both buckets released.
- **Ops:** `FUTEX_WAIT` (0), `FUTEX_WAKE` (1), `FUTEX_REQUEUE` (3),
  `FUTEX_CMP_REQUEUE` (4), `FUTEX_WAIT_BITSET` (9) and `FUTEX_WAKE_BITSET`
  (10), each with or without `FUTEX_PRIVATE_FLAG` (128);
  `FUTEX_CLOCK_REALTIME` (256) only with `FUTEX_WAIT_BITSET` (else
  `-ENOSYS`, Linux's `do_futex`). Every other op (`WAKE_OP`, the PI ops,
  `WAIT_REQUEUE_PI`, `CMP_REQUEUE_PI`) is `-ENOSYS`; `futex_waitv` (449)
  too. The census saw ops 0, 1, 0x80 and 0x81 only. musl 1.2.5 also uses
  `FUTEX_REQUEUE` (implemented) and, for priority-inheritance mutexes,
  `FUTEX_LOCK_PI`/`FUTEX_UNLOCK_PI`: those stay `-ENOSYS`, so libc-test's
  `pthread_mutex_pi` (static and dynamic, both passing natively) fail in
  Lumen and D4 lists them as expected failures with a backlog issue for
  PI futexes.
- **Errors:** alignment is checked before the user range: not 4-byte aligned
  `-EINVAL`, otherwise an address outside user space `-EFAULT`; timeout
  copying/validation for wait commands precedes flag rejection (`-ENOSYS`)
  and a zero bitset's `-EINVAL`; a negative requeue count `-EINVAL`
  (`check_requeue_counts`, before any key lookup, fault or value
  compare), a word that doesn't match `-EAGAIN`, an unreadable word
  `-EFAULT`, a bad timeout (a negative `tv_sec`, or a `tv_nsec` negative
  or past 999,999,999) `-EINVAL` (Linux's `futex_init_timeout`,
  `timespec64_valid`), a deadline passed
  `-ETIMEDOUT`.
- **Timeouts:** `FUTEX_WAIT`'s is relative on `CLOCK_MONOTONIC`;
  `FUTEX_WAIT_BITSET`'s absolute, on `CLOCK_MONOTONIC` or, with the flag,
  `CLOCK_REALTIME` (converted to a monotonic deadline when the wait
  starts; a later clock change doesn't move it, a deviation). A signal:
  `-ERESTARTSYS` without a timeout, `-ERESTART_RESTARTBLOCK` with one (the
  restart block holds the deadline), as Linux's `futex_wait`. The handler
  stores that block and returns its errno through
  `Err(linux::signal::interrupted_with(block))`; `syscall::finish` negates it.
- **Keys** (`FutexKey`, `Ord + Copy` as lumen-futex asks):
  `Private { space, uaddr }` for any op with `FUTEX_PRIVATE_FLAG`, where
  `space` is the `AddressSpace`'s L4 root frame (unique while it lives);
  for a shared op, by what backs the page, found under Lock 1 (faulting it
  in first if needed): `Anon { space, uaddr }` for a private anonymous or
  copy-on-write page, as Linux keys an anonymous page by its mm and
  address; on a writable private file mapping, the op first calls
  `user::fault_in_writable`, which breaks copy-on-write, and keys the
  private copy `Anon`, as Linux's `get_futex_key` faults the page for
  write (`FOLL_WRITE`) before keying it, so a `WAIT` before the first
  store and a `WAKE` after it meet; `File { file_id, offset }` only for a
  `MAP_SHARED` page or a read-only private mapping's page-cache page
  (section 5's `file_id`), as Linux keys one by its inode and offset; `SharedFrame {
  frame, offset }` for a `MAP_SHARED | MAP_ANONYMOUS` frame shared across
  `fork` (section 5; the key holds no reference, so a frame freed and
  reused can at worst cause a spurious wake, which futex users tolerate).
  A private op and a shared op on the same word never meet, as in Linux,
  whose keys differ by `FUT_OFF_MMSHARED`. The census's shared
  `WAIT`/`WAKE` pair on musl's thread-list lock are both `Anon` keys of
  one address space, and so is the shared wake at a thread's exit
  (`clear_child_tid`), so they meet.
- **Buckets:** `FUTEX_BUCKETS: [SpinLock<FutexTable<FutexKey,
  Arc<FutexWaiter>>>; 256]`, indexed by a hash of the key, under
  lumen-futex's bucket rule (section 9, level 10d). The `FutexNode` and
  the waiter's `Arc` are allocated before, and `Woken`/`Cancelled` nodes
  dropped after.
- **Robust lists:** `set_robust_list(head, len)` (273) stores the head
  (`len` other than 24, `sizeof(struct robust_list_head)`, is `-EINVAL`,
  checked by `set_robust_list`, not `get_robust_list`);
  `get_robust_list(pid, head_ptr, len_ptr)` (274) reads a thread's (0: the
  caller's; any live tid otherwise, since every process is root and Linux
  allows it with ptrace access; no such thread `-ESRCH`). musl's
  `pthread_mutexattr_setrobust` calls `get_robust_list` once and refuses
  robust mutexes if it fails. At a thread's exit, before its
  `clear_child_tid` store, and at `execve`, the kernel walks the list as
  Linux's `exit_robust_list` does, with no lock held: at most 2048 entries
  (`ROBUST_LIST_LIMIT`), each read with `copy_from_user`. A fault reading
  an entry's next pointer is propagated after handling that entry's owner;
  any failure abandons the remaining walk and logs tid, head and errno at
  debug level. For each entry, `handle_futex_death`: a futex word whose tid
  bits are the thread's is replaced by `(word & FUTEX_WAITERS) |
  FUTEX_OWNER_DIED` with `user::cmpxchg_u32`, retried while the word changes
  and, on `Fault`, after `user::fault_in_writable` (abandoning the walk if
  that fails). With `FUTEX_WAITERS` a non-PI entry gets one shared wake.
  PI list entries and PI pending entries still get `OWNER_DIED`, but no
  ordinary wake: Linux leaves their wake to `exit_pi_state`; Lumen has no
  PI futexes. The head's `list_op_pending` entry is skipped in the loop and
  handled once afterwards, if set. When it names a different owner the
  word is unchanged; a non-PI pending entry wakes one waiter only when its
  owner bits are zero. This narrower pending-wake rule is a deviation from
  Linux sources with `FUTEX_ROBUST_UNLOCK`, which also wake when another
  owner has not set `FUTEX_WAITERS`.

Tested by `cargo:lumen-futex`,
`scenario:test_linux_proc::test_futex_timeouts_and_nanosleep`,
`ktest:lumen_kernel::linux::futex::futex_cancel_follows_requeue_and_wake_wins`,
`ktest:lumen_kernel::linux::futex::robust_walk_checks_owners_pending_and_faults`,
`ktest:lumen_kernel::linux::futex::futex_file_keys_and_private_cow_before_wait`,
`ktest:lumen_kernel::linux::syscall::futex::sleep_and_futex_restart_preserve_deadlines`,
and `scenario:test_linux_proc::test_futex_and_pthread_mutex_stress` (section 15).

#### Timers: `setitimer`, `alarm` and POSIX timers (C13)

The census found `setitimer(ITIMER_REAL)` (musl's `alarm`) and
`timer_create`/`timer_settime` (vim over the pty: musl's `SIGEV_THREAD`
timer, a helper thread waiting in `rt_sigtimedwait` for signal 32 aimed at
it with `SIGEV_THREAD_ID`). vim 9.1's `start_timeout` prints an error
(`e_could_not_set_timeout_str`) when `timer_create` fails, so `-ENOSYS`
would put an error on D3's screen: M8 implements them, as C13 (after C3
and C5; C6 for `rt_sigtimedwait`), and D3 depends on C13.

- **State:** a process's `SignalState::timers` holds `ITIMER_REAL`, up
  to 32 POSIX timers (`PosixTimer { clock, notify, signal, sival,
  interval_ns, deadline, instance, overrun, overrun_last }`) and
  `next_instance: u64`. `notify` is the `sigevent`'s mode,
  `TimerNotify::{Process, Thread(Weak<UserThread>), None}`: `SIGEV_SIGNAL` is
  `Process`, and so is a raw `SIGEV_THREAD`, which Linux's `good_sigevent`
  accepts with `SIGEV_SIGNAL` (musl never passes it: its `timer_create`
  turns `SIGEV_THREAD` into `SIGEV_THREAD_ID` aimed at its helper);
  `SIGEV_THREAD_ID` is `Thread`, holding the target's identity, not its
  tid, so a tid reused after the target exits is never signalled (Linux's
  timer holds the target's `struct pid`, `it_pid`, and `pid_task` finds no
  task once it has exited); an expiry whose target has gone sends nothing
  and still re-arms. `SIGEV_NONE` is `None`.
  POSIX timers are deleted at `execve` and exit; `ITIMER_REAL` only at
  exit. `timer_delete` (and `execve`) discards the timer's queued
  `SI_TIMER` signal, and an expiry asks whether it is still queued, which
  need two lumen-signal additions, C13's:
  `PendingSignals::discard_if(|info| ...)`, removing queued entries a
  predicate picks, and `PendingSignals::any(|info| ...)`, whether a queued
  entry matches (both host-tested in `cargo:lumen-signal`).
- **Instances:** every `timer_settime`, `setitimer`, `alarm` and delete
  replaces the timer's `instance`: an arming with a fresh one from the
  process's `next_instance`, never reused (so a deleted timer's id taken
  again by `timer_create` starts on a new instance too), a disarm or
  delete with none, so an entry already in the timers thread's list no
  longer matches, as Linux's `timer_settime` and `timer_delete` bump
  `it_signal_seq`; the timers thread's periodic re-arm keeps it. A
  `timer_settime` also discards the timer's queued `SI_TIMER` signal
  (`discard_if`) and zeroes `overrun` and `overrun_last`, as Linux drops
  the old setting's signal at delivery (`__posixtimer_deliver_signal`'s
  `it_signal_seq != it_sigqueue_seq`) and `posix_timer_set_common` resets
  the overrun count (a deviation in one corner: Linux's `rt_sigpending`
  still shows that signal until a dequeue drops it). `TIMERS` is keyed
  `(deadline, pid, instance)`: unique, since an instance is never reused
  within a process and exit step 3 removes a process's entries before its
  pid can be freed. A disarm, re-arm or delete removes the timer's current entry
  (its key known from the timer) under `signal`, then `TIMERS`.
- **Expiry:** each armed timer but a `SIGEV_NONE` one has an entry in
  `TIMERS`, a kernel-wide `SpinLock<BTreeMap<(deadline, pid, instance),
  (Weak<Process>, TimerId)>>` (`TimerId`: `Real` or a POSIX timer's id),
  whose earliest deadline is also kept in an atomic. The boot CPU's timer
  tick compares that atomic with `now_ns` (one load) and, once it has
  passed, wakes the `timers` kernel thread; no signal is sent from the
  interrupt. The thread moves the expired entries out of `TIMERS` into a
  local list and releases it; then, with no lock held, it upgrades each
  entry's `Weak<Process>` (a process gone is skipped, the `Arc` dropped
  outside every `SpinLock`). It then sends through the send path's timer
  variant, all in that path's one `PROCS` then `signal` hold, as Linux's
  `posixtimer_send_sigqueue` works under one `siglock` hold, in this
  order:
  1. The check: the timer at that `TimerId` still has the entry's
     `instance`. If not (re-armed, disarmed or deleted since), the entry
     is stale and nothing else happens; this is the only branch that
     skips the re-arm.
  2. For a POSIX timer, whether its `SI_TIMER` signal (that `si_timerid`)
     is still queued, asked of the queue itself (`PendingSignals::any`),
     as `posixtimer_send_sigqueue` asks `list_empty(&q->list)`: if it is,
     no send and `overrun` + 1. Otherwise the ordinary send (section 6,
     which handles `SIGCONT`, `SIGSTOP` and `SIGKILL` as any sender's) of
     the timer's `SigInfo` (`SI_TIMER` with `si_timerid` and `si_value`),
     to the process or, for `Thread`, that thread if it is still alive. `ITIMER_REAL`
     skips the question: a plain send of `SI_KERNEL` `SIGALRM` each
     expiry, as Linux's `it_real_fn` calls `kill_pid_info`, a standard
     signal that coalesces in the queue. With no flag of its own to
     clear, nothing is left stuck by a signal discarded otherwise (a
     disposition changed to `SIG_IGN`, `timer_delete`, `execve`).
  3. The update and re-arm, before the target's `signal` is released: an
     enqueue the queue refused (`Coalesced`: the same standard signal
     already pending from another sender; `Full`; `NoMemory`) counts as
     an overrun, a deviation, since Linux queues a timer's preallocated
     `sigqueue` past `legacy_queue` and the queue limit
     (`posixtimer_queue_sigqueue`); then a periodic timer is re-armed by
     its interval in every branch but a stale entry's, the periods it
     skipped counted as overruns.
  4. With the target's `signal` released and `PROCS` still held, the
     send's `StateChange` (a `SIGCONT` timer's `CLD_CONTINUED`) is acted
     on as any sender's.

  Re-arming at expiry is Lumen's mechanism: Linux leaves an expired
  periodic timer stopped (`POSIX_TIMER_REQUEUE_PENDING`, and `ITIMER_REAL`
  likewise) and re-arms it when its signal is dequeued
  (`__posixtimer_deliver_signal`'s `timer_rearm`,
  `posixtimer_rearm_itimer`), forwarding past the periods it missed. What
  Lumen keeps of Linux's is the count user space sees: dequeuing an
  `SI_TIMER` signal moves the timer's `overrun` (the expiries after the
  one that queued it) into `si_overrun` and `overrun_last` and resets it
  to 0; `timer_getoverrun` reports `overrun_last`, clamped to `INT_MAX`,
  as Linux's `it_overrun_last`, which only a delivery sets and
  `timer_settime` resets to 0.
- **`SIGEV_NONE`:** such a timer has no `TIMERS` entry and never sends, as
  Linux's `common_timer_set` never starts its hrtimer ("SIGEV_NONE timers
  are not queued"); `timer_gettime` works its state out from the clock,
  as `common_timer_get` does: a periodic one's deadline moved forward by
  whole intervals past now (the skipped ones added to `overrun`, which no
  delivery reports), so its `it_value` is above 0; an expired one-shot
  reports 0. `timer_getoverrun` on it is 0, since only a delivery sets
  `overrun_last`.
- **Clocks:** deadlines are `CLOCK_MONOTONIC` nanoseconds; a
  `CLOCK_REALTIME` absolute time is converted when the timer is armed (a
  later change of the realtime clock doesn't move it: a deviation, as for
  `futex`). A timer fires at the first tick after its deadline, never
  before.
- **musl's `SIGEV_THREAD` helper** blocks signal 32, leaves it at
  `SIG_DFL` and waits for it in `rt_sigtimedwait`: section 6's send rules
  exempt a thread's `real_blocked` set from the ignored-drop and the
  group-kill shortcut, so the expiry reaches the helper and kills
  nothing.

This issue's PR models every de_thread/timer interleaving listed here
against Linux's code before implementation, as a host-tested state machine
where practical.

Tested by `scenario:test_linux_proc::test_interval_and_posix_timers`
(section 15).

#### Sessions and process groups (C8)

`setsid` (112), `setpgid(pid, pgid)` (109), `getpgid(pid)` (121),
`getpgrp()` (111, now `getpgid(0)`) and `getsid(pid)` (124) are
`ProcTree::setsid`/`setpgid`/`getpgid`/`getsid` under `PROCS`, with
lumen-proc's errnos (`EPERM`, `ESRCH`, `EACCES` after the child's `exec`,
`EINVAL`). `setsid` also clears the caller's controlling terminal
(`SignalState::ctty`), as Linux's `ksys_setsid` does (`proc_clear_tty`).
`kill(-pgid, sig)` and `kill(0, sig)` send to `pgrp_members` under
`PROCS` (`-ESRCH` if the group has none). Controlling terminals, the
foreground group, `tcsetpgrp` (`TIOCSPGRP`) and the job-control signals
are section 10's. Tested by
`scenario:test_linux_proc::test_sessions_and_process_groups` (section
15).

### Capabilities and sandboxing (WASM M5; Linux namespaces M11)

Lumen's Principle 2 (no ambient authority) holds in two layers:

- **M5 WASM apps:** hold opaque `u32` capability handles (`Log`, `Time`,
  `Desktop`, `Window`, `Trace`) checked on every host call ("Apps", below).
- **M11 Linux sandboxes:** each sandboxed Linux process/agent receives a
  **per-process VFS mount namespace** (only explicitly granted subtrees are
  mounted into its `/`; `..` cannot escape a mount root) and a **network
  policy filter** (allowed `host:port` patterns, or no network), plus optional
  `AppActions` access to Lumen's GUI automation tree (section 11). Every
  sandboxed agent action is appended to the tamper-evident audit log
  (section 13) and paired with a filesystem snapshot (section 9).

| Kind | Scope | Built / planned |
|---|---|---|
| `Log`, `Time`, `Desktop`, `Window`, `Trace` | M5 WASM app handles | built (M5) |
| VFS mount namespace (`Dir` subtree grants) | Scoped directory mounts for Linux processes/agents (read-only or read-write) | planned (M11) |
| Network policy (`Net` allowlist) | Per-process host/port/protocol filter over BSD sockets | planned (M11) |
| `AppActions` | Named automation-tree actions in one GUI window | planned (M11) |
| `Snapshot` | Read-only snapshot view or restore right over a sandbox root | planned (M11) |

### Apps: WASM modules in the kernel (M5: designed in #195, built by #196 to #208)

The app interface every M5 issue codes against: what a module may
import, how it holds capabilities, what limits it runs under and how it
ends. The types (handle kinds, error codes, the manifest, the limits,
the wire formats and fixed layouts) are `crates/lumen-app-api` (`no_std`
+ `alloc`, built for the host and wasm32); the runtime is
`kernel/src/app.rs` (created by #197) and `kernel/src/app/` (#199:
`runtime.rs` the app table, loading, slices, endings and the shell's
commands; `host.rs` the host functions, `window.rs` (#206) the window
and text calls; `tests.rs` their ktests); the
guest SDK is `apps/lumen-app` (#206). Every number named in capitals below is in the limits table
(`APP_OVERHEAD`'s parts included), and only there.

#### What an app is

- **A module:** a `no_std` + `alloc` `cdylib` built by the pinned rustc
  for `wasm32-unknown-unknown` with that target's default wasm features
  (#196's harness test lists them). Float functions such as `f32::sqrt`
  aren't in `core`: the SDK is to re-export `libm`, added to `apps/` in a
  dependency PR of its own (a new crate there); until then apps avoid
  them.
- **Its exports:** `memory` and `lumen_main: [] -> [i32]`, called once;
  its result is the app's exit code (0 is success). A missing export or
  another signature is refused at load. Nothing else is called from the
  host: events, requests and wake-ups are what `lumen_main`'s own loop
  asks for.
- **Its memory is fixed:** the module's one memory declares a maximum
  equal to its initial size, at most `MEMORY_PAGES` (each app sets it
  with `--initial-memory` and `--max-memory` link arguments, written by
  the template's `build.rs`). wasmi allocates it once, at its exact size,
  when the module is instantiated; `memory.grow` then always returns -1
  (wasm's answer), refused before anything is allocated. The SDK's
  allocator manages `[__heap_base, end of memory)` and never grows it.
  A memory without a maximum, or with one that differs from its initial
  size, is refused at load.
- **Its imports:** functions of the `lumen` module only, from the table
  below, with the signatures given. Anything else (another module, an
  unknown name, a known name with another signature, an imported
  memory, table or global) is refused at load, naming the import (its
  name sanitised as log text is).
- **One kernel thread per app**, named `app:<name>` (generated by
  `kernel/build.rs`, which refuses an app name over 11 bytes, as
  `tools/new-app.sh` does, so the whole `app:<name>` fits in a thread's
  15-byte name, section 6, "Kernel threads"; #276), so `threads`, `top` and
  `perf top --threads` (#205) show it by name. `run` places apps round robin over the started CPUs in app-id
  order (`sched::spawn_on`), on an ordinary guarded kernel stack.
  `portable-dispatch` (#197) keeps the native stack's depth constant
  however long a module runs (host calls aside); wasmi's value and call
  stacks are on the heap.
- **App ids** count from 1 and are never reused in a boot. An app is
  `loading` (compiling, on its own thread), `running` (in a slice),
  `waiting` (in a blocking host call), `ending` (its outcome recorded,
  its thread dropping what it held) or `ended` (its thread done with it,
  not yet joined): one phase and one outcome per app, changed together
  under the app's own lock (`runtime.rs`'s `Life`).

#### The wasmi configuration

`wasmi = "=2.0.1"`, `default-features = false`, features `validate` and
`portable-dispatch`. Each app has its own `Engine`, dropped when it ends
(a shared engine keeps every module's compiled code for the boot and
caches value stacks). Its `Config`:

- `consume_fuel(true)`;
- `compilation_mode(CompilationMode::Eager)`: lazy translation compiles
  a function at its first call and charges it to fuel by the byte, and
  a compile that runs out of fuel is a plain error that can't be
  resumed; eager compilation charges no fuel and keeps every compile
  allocation inside `Module::new`, where it is measured (below);
- `wasm_multi_memory(false)` (on by default; a second memory fails
  `Module::new`) and `allow_start_fn(false)` (a start section fails
  `Module::new`: a start function would run inside instantiation, outside
  any slice). A second table passes validation (reference types allow
  it) and is refused at instantiation by the limiter's `tables(1)`;
- `set_max_recursion_depth(CALL_DEPTH)`, then
  `set_max_stack_height(VALUE_STACK_BYTES)` and
  `set_min_stack_height(VALUE_STACK_BYTES)` (wasmi counts the value stack
  in bytes; a minimum equal to the maximum is accepted, so the stack is
  allocated once at its full size and never grows); past either limit
  the app traps with "call stack exhausted";
- the default wasm features otherwise. wasmi also accepts
  `extended-const`, which rustc's defaults don't use; #196's
  `target_features` check catches an app that does.

The store's limiter is `StoreLimitsBuilder::new().memories(1).tables(1)
.instances(1).memory_size(MEMORY_PAGES × 64 KiB)
.table_elements(TABLE_ELEMENTS).build()`, set with `Store::limiter`.
wasmi allocates tables with `try_reserve`, so `table.grow` past the
limit or the heap returns -1.

#### Memory: each app's worst case, checked at compile time

The kernel heap has no `realloc`, and wasmi grows its `Vec`s by
doubling, so a growing buffer costs up to three times its size (twice
its size reserved, plus the old one while it is copied), and the heap
window never shrinks. Each app's allocations are therefore fixed in
size or bounded:

| Part | Charge |
|---|---|
| linear memory | `MEMORY_PAGES` × 64 KiB, allocated once (above) |
| value stack | `VALUE_STACK_BYTES`, allocated once (above) |
| compiled code | `CODE_FACTOR` × `MODULE_BYTES` |
| store, instance, call stack (`CALL_DEPTH` frames), handle table | `STORE_BYTES` |
| table | `TABLE_BYTES` (`TABLE_ELEMENTS` × 16) |
| its published tree (#207): the desktop's rows and the id map held, and a republish's peak | `TREE_COPIES_BYTES` (1 MiB: the worst trees' peak, measured by `lumen-app-api`'s `tests/publish_peak.rs`, with the headroom `limits.rs` states) |
| glyph cache (#206) | `GLYPH_CACHE_BYTES` |
| its window: two surfaces plus `text_draw`'s scratch | `WINDOWS` × `SURFACES` × `WINDOW_PIXELS` × `BYTES_PER_PIXEL` |

The middle four are `APP_OVERHEAD`'s parts: a `const` assertion checks
their sum is at most `APP_OVERHEAD`, which `APP_WORST` charges.

`lumen_app_api::limits::APP_WORST` is their sum, and a `const`
assertion checks `MAX_APPS × APP_WORST <= APP_BUDGET`, so there is no
runtime counter: holding a slot is holding its worst case.
`KERNEL_RESERVE` plus four worst cases is right at what the frame
allocator owns on the guests, though:
`qemu.py`'s default and `make run` give 256 MiB, which left the frame
allocator 39,393 to 39,444 frames in #278 (the kernel's ELF, debug info
and embedded apps included, is reserved), right at the 39,440 (154.06
MiB) four slots need, so such a guest gets 3 or 4 as the kernel's image
moves. `test_run_refuses_past_max_apps` boots with 384 MiB
(`boot(memory=...)`, 72,210 frames) and checks the slot count `mem`'s
frames give before it fills them. The runtime computes,
once (at the first `run`), `slots = min(MAX_APPS, (usable RAM −
KERNEL_RESERVE) / APP_WORST)` (`lumen_app_api::limits::slots`), usable RAM
being what the frame allocator owns, and `run` refuses past `slots` with
the `MAX_APPS` message. The slot count is `run`'s only refusal for
memory.

`OutOfMemory` means the heap refused although the worst case fits: a
bug, but one the app survives where the heap is asked fallibly. #206's
host calls fail with it. Where wasmi allocates fallibly while loading, its
translator's buffers (its error type isn't exported, so its message tells
it apart) or a linear memory or table at instantiation (wrapped in an
`InstantiationError`), `run` prints `run: <name>: not enough memory`
(`runtime::out_of_memory`; a ktest makes the heap refuse each through a
seam, `heap::refuse_from`). wasmi's other allocations, its execution
stacks among them (the value stack reserved at the first call, frames
pushed), are infallible: running out there is the kernel's
allocation-error panic, after "started".
#199 adds the heap's high-water mark (`heap::peak()`, reset by
`heap::reset_peak()`; `app::tests::the_heaps_peak_follows_its_use`), and
ktests check the charges against it, assuming nothing else allocates
meanwhile (ktests run one at a time, with no app running and the other
CPUs idle): `app::tests::compiled_code_fits_its_charge` (for every
embedded module, `BUNDLED` and `TEST_APPS`, a non-empty list, the peak
across `Engine::new` and `Module::new` is at most `CODE_FACTOR` × the
module's size), `app::tests::a_fixed_memory_is_allocated_once` (the peak
across instantiation is at most the memory's size plus `APP_OVERHEAD`,
across the first call at most `VALUE_STACK_BYTES` plus `APP_OVERHEAD`;
`memory.grow` returns -1 and leaves `used` and the peak unchanged) and
`app::tests::the_value_stack_is_allocated_once_at_its_full_size` (a
shallow first call allocates at least `VALUE_STACK_BYTES`; a 900-deep
call then grows only the call stack, within `STORE_BYTES`, and a second
one nothing).

#### Capabilities as handles

Nothing is ambient: every call that touches an object takes a handle,
an opaque id in 1..=`i32::MAX` (`Handle(NonZeroU32)` in the SDK).
Handles are never reused within an app's life; once the app's count is
used up, `window_open` answers `Limit` rather than wrap (host-tested on
the handle table with a small cap). An app holds at most five: its
grants (`handle_find` returns the same handle every time) and its one
window. A missing, closed or made-up handle is `BadHandle`, one of
another kind `WrongKind`: an error code, never a trap.

| Kind | Number | Object | Granted |
|---|---|---|---|
| `Log` | 1 | the kernel log, lines tagged with the app's name | always |
| `Time` | 2 | the clocks | `caps` `time` |
| `Desktop` | 3 | opening a window; the shared UI font | `caps` `desktop` |
| `Window` | 4 | the app's window | only by `window_open` |
| `Trace` | 5 | the per-CPU trace rings, read only (X-ray) | `caps` `trace` |

`lumen_app_api::Grant` (`Log` = 1, `Time` = 2, `Desktop` = 3, `Trace`
= 5) is what `handle_find` takes; `Listable` (`Time`, `Desktop`,
`Trace`) is what `caps=` and `run --without` name. An app starts with
`Log` and its manifest's grants, minus what `run --without <grant>`
withholds: `run` can only take away (`run --without log` is refused:
`log` isn't listable). `handle_find(kind)` answers `NotGranted` for a
grant the app doesn't hold (never given, withheld, or closed), and
`BadArgument` for `Window` (4) or a number that names no grant. Closing
the `Window` handle destroys the window; closing another drops that
grant for the rest of the app's life. An app has at most one window in
M5 (`WINDOWS`).

#### The manifest

A custom section named `lumen.manifest`, exactly `caps=` followed by
zero or more listable grants in lower case separated by single commas
(`caps=time,desktop`, or `caps=`), with no whitespace or newline. The
app's name is its file's stem; `Log` isn't listed (`caps=log` is an
unknown grant). The SDK writes it with `lumen_app::manifest!(time,
desktop)`, a `#[unsafe(link_section = "lumen.manifest")]` byte string
built with `concat!`. wasm-ld concatenates sections with the same name
(checked with the pinned toolchain), so a second `manifest!` anywhere in
the app makes one section that breaks the grammar. The kernel reads it
with `lumen_app_api::manifest::find(&[u8]) -> Result<Manifest,
ManifestError>`, which walks the module's sections itself. No manifest,
two sections, an unknown or repeated grant, or anything off the grammar
is refused at load, naming why. Loading modules from disk (M7+) adds an `abi=`
key.

#### The host interface (import module `lumen`)

Arguments and results are wasm `i32` and `i64`. Every call returns its
result, `>= 0`, or a negated `lumen_app_api::Error` (also the calls
returning `i64`). The SDK wraps every call (#206: the imports declared
once, `lumen_app_api::sys`, wasm32 only); apps don't call them
directly. #199 builds the calls without a window (`handle_*`, `args_*`,
`exit`, `panic`, `log`, `time_*`, `trace_*`: `lumen_app_api::host`
lists them, and a module importing anything else is refused); #206 the
window and text calls (`window_open`, `window_commit`,
`window_next_event`, `window_wait`, `text_*`: `app/window.rs`) with
`Event`; #207 `window_publish` and `window_answer` with the tree and
answer wire types.

**Every call, in this order** (`host.rs`'s `entry` wraps every one for
step 1):

1. checks the kill flag (set: the app ends with the runtime's `Stop`
   host error, below); if the slice has less than `CALL_FUEL` left, ends
   the slice right there (counted, a kill or the close grace ending the
   app, the CPU given up, a fresh slice: wasmi checks fuel only where a
   block starts, so a run of calls in one block would otherwise be free);
   then charges `CALL_FUEL`, refused or not;
2. checks the handle, then its kind;
3. copies fixed structures (`Canvas`, `TextDraw`, `Rect`s) in by value,
   then checks every (pointer, length) range, taken as unsigned, with
   checked arithmetic (`checked_add`/`checked_mul`; an overflow or any
   byte outside the memory's current size is `BadPointer`), including the
   pixel ranges the copied structures name;
4. checks its other arguments' values (`log`'s level, a negative sleep:
   `BadArgument`), then copies inputs out of the app's memory: text into
   a kernel buffer, refused with `BadArgument` past its cap before it is
   copied, then decoded (bad UTF-8 is `BadArgument`);
5. does its work, and charges fuel for what it did (`runtime::charge`:
   what the slice has left, and the rest carried as debt that the next
   slice starts without, so a charge never fails the call and is never
   lost);
6. only then takes the output range and writes it whole, or writes
   nothing.

So overlapping input and output ranges are harmless, a refused call
writes nothing, and no guest pointer is held across anything that can
run the app.

| Function | Result | Notes |
|---|---|---|
| `handle_find(kind: i32) -> i32` | the handle | above |
| `handle_close(h: i32) -> i32` | 0 | |
| `args_len() -> i32` | bytes | the `run` arguments after the app's name, each followed by a NUL |
| `args_read(buf: i32, len: i32) -> i32` | bytes written | `BufferTooSmall` |
| `exit(code: i32)` | never returns | ends the app as if `lumen_main` returned `code` |
| `panic(msg: i32, len: i32)` | never returns | the SDK's panic handler; at most `TEXT_BYTES` of the message are read, decoded lossily (bad UTF-8 as U+FFFD); a range past the memory or a negative `len` still ends the app, its message `<a bad pointer>` |
| `log(h: i32, level: i32, msg: i32, len: i32) -> i32` | 0 | level 1 (error) to 5 (trace), else `BadArgument`; text up to `TEXT_BYTES` |
| `time_now(h: i32) -> i64` | ns since boot | `time::since_boot_ns` |
| `time_wall(h: i32) -> i64` | seconds since 1970-01-01 UTC | the RTC (taken as UTC) through `rtc::now_quiet`; `Unavailable` if it failed |
| `time_sleep(h: i32, ns: i64) -> i32` | 0 | a slice end, then blocks (`waiting`) on the app's own wait queue until the time passes or a kill; 0 (or a deadline already reached) yields the CPU instead; `ns < 0` is `BadArgument` |
| `window_open(h: i32, title: i32, len: i32, width: i32, height: i32) -> i32` | the `Window` handle | `desktop::open_window`; `Unavailable` (no desktop), `Limit` (`WINDOWS`, `desktop::MAX_WINDOWS`, the handle count), `BadArgument` (a zero size, past `WINDOW_PIXELS` or the screen, a title past `TITLE_BYTES`), `OutOfMemory` |
| `window_commit(h: i32, canvas: i32, rects: i32, n: i32) -> i32` | 0 | `n` (1..=`COMMIT_RECTS`) `Rect`s of the `Canvas`, whose `w` and `h` must equal the client size (else `BadArgument`), copied into the owner's surface, then `WindowHandle::commit`ted |
| `window_next_event(h: i32, buf: i32, len: i32) -> i32` | bytes, 0 for none | one encoded `Event`, never blocking (below) |
| `window_wait(h: i32, deadline: i64, animating: i32) -> i32` | 1 something to read, 0 the deadline | below |
| `window_publish(h: i32, tree: i32, len: i32) -> i32` | 0 | an encoded `Tree` (#207): past `TREE_BYTES` refused before it is copied, then decoded node by node and checked (`lumen_app_api::tree::Tree::decode`); refused (`BadArgument`, a DEBUG line naming why) keeps the last one |
| `window_answer(h: i32, answer: i32, len: i32) -> i32` | 0 | an encoded `AppAnswer` for the window's running request (one the app has read: one kept in hand by `BufferTooSmall` isn't); `BadArgument` with none (checked after the range, before the copy), past `ANSWER_BYTES` (before the copy), or one that doesn't decode; a refused answer leaves the request running |
| `text_line_height(h: i32, px: i32) -> i32` | pixels | on the `Desktop` handle |
| `text_measure(h: i32, px: i32, text: i32, len: i32) -> i32` | advance in pixels | `font::Text::measure` |
| `text_draw(h: i32, canvas: i32, args: i32, text: i32, len: i32) -> i32` | advance in pixels | a `TextDraw` at `args`; below |
| `trace_cpus(h: i32) -> i32` | one past the highest online CPU | below |
| `trace_read(h: i32, stream: i32, cpu: i32, since: i64, buf: i32, len: i32) -> i32` | events copied | below |

Text (`log`, `panic`, titles, `text_*`) is at most `TEXT_BYTES` (titles
`TITLE_BYTES`); every text call takes `px` in
`TEXT_PX_MIN..=TEXT_PX_MAX`, else `BadArgument`. Each call's fuel:
`CALL_FUEL` at its entry (step 1), every call, refused or not; then, for
what it did (step 5), `BYTE_FUEL` per byte `log` copied in (charged even
if the text is then refused), `LOG_FUEL` per line logged (so a logging
loop writes few lines per slice), `PIXEL_FUEL` per pixel of a commit, of
`text_draw`'s clip and of the three surfaces `window_open` allocates and
fills (`SURFACES` × w × h), `PIXEL_FUEL` × px² per glyph `text_draw`
rasterizes (one its glyph cache didn't hold: `font::Text::draw` skips a
glyph whose box misses the clip before rasterizing it, so rasterizing
follows the clip), `TEXT_CHAR_FUEL` per character of each layout pass
over a text (`text_measure` one, `text_draw` two: its advance and its
drawing walk every character, whatever reaches the clip),
`EVENT_FUEL` per trace event copied out, `BYTE_FUEL` per byte of a tree
or an answer copied in (refused or not), and `NODE_FUEL` per node of a
tree decoded, items counted, refused or not, and per node of the tree an
accepted one replaces (freed) (#207: its strings copied,
its id mapped, its row made; a refusal can come after every node was
decoded and its ids sorted, or partway through a list's items, which
counts the items its bytes could hold, `tree::MIN_ITEM_BYTES` each:
`Tree::decode_counted`)
(`app::tests::host_calls_charge_their_fuel` checks `CALL_FUEL`,
`BYTE_FUEL`, `LOG_FUEL` and `EVENT_FUEL`;
`app::tests::window_calls_charge_their_fuel` the pixels and glyphs, as
floors; `app::tests::window_publish_charges_by_the_byte_and_the_node` a
tree's bytes and nodes, items counted, a refused tree's too;
`app::tests::window_answer_charges_by_the_byte` an answer's bytes;
`app::tests::host_calls_cost_fuel_however_many_in_one_block` a run of
calls in one block across slices, debt included).

**Fixed layouts** (little-endian, no alignment needed; encoded and
decoded by `lumen-app-api`, their bytes pinned by host tests):

| Layout | Bytes | Fields |
|---|---|---|
| `Canvas` | 16 | `ptr: u32` (a multiple of 4, else `BadPointer`), `stride: u32` (pixels, `>= w`, else `BadArgument`), `w: u32`, `h: u32`; xRGB8888 pixels, every row inside the memory (checked in that order: `lumen_app_api::layout::Canvas::pixels`) |
| `Rect` | 16 | `x: i32`, `y: i32`, `w: u32`, `h: u32`; clipped to the canvas, an empty one skipped (`window_commit`'s count, 1..=`COMMIT_RECTS`, is checked before the rectangles' range: it says how many bytes that range has) |
| `TextDraw` | 32 | `x: i32`, `y: i32` (the line box's top left), `px: u32`, `rgb: u32` (its top byte ignored), `clip: Rect` |
| trace header | 16 | `next: u64`, `missed: u64` |
| trace event | 56 | `seq: u64`, `tsc: u64`, `cpu: u16`, `kind: u16`, `pid: u32`, `args: [u64; 4]` (`lumen_trace::Event`) |

**Text** is the kernel's, so the UI font and its parsing stay shared
(`font::face`): the app's thread has one `font::Text` (its glyph cache),
made on its first text call. `text_draw` needs the app's window
(`Unavailable` without one): it copies the pixels of `clip`, cut to the
canvas and then to the window's size (from the clip's corner), out of
the app's memory into a scratch `lumen_gfx::Surface` the
runtime keeps for the window (no bigger than it: the third surface in
the charge), draws with `font::Text::draw`, and copies them back; its
result is the advance `font::Text::measure` gives. Its errors:
`BadHandle`, `WrongKind`, `Unavailable`, `BadPointer` (the canvas, the
`TextDraw`, the text, the clip's pixels), `BadArgument` (`px`, the
text). `lumen_ui::Text` inside an app (#207) is the SDK's `ui::HostText`
over the three text calls, at `UI_TEXT_PX` (the kernel windows'
`font::TEXT_PX`, a `const` assertion in `font.rs` ties them), each text
cut to `TEXT_BYTES` first.

Error codes (`lumen_app_api::Error`, `#[repr(i32)]`, returned negated;
stable, never reused, pinned by a host test):

| Code | Error | Meaning |
|---|---|---|
| 1 | `BadHandle` | no such open handle |
| 2 | `WrongKind` | a handle of a kind the call doesn't take |
| 3 | `NotGranted` | `handle_find` for a grant the app doesn't hold |
| 4 | `BadPointer` | a range outside the app's memory, an overflow, or an unaligned canvas |
| 5 | `BadArgument` | bad UTF-8, a value out of range, text past its cap, an encoding that doesn't decode or breaks its rules |
| 6 | `BufferTooSmall` | the output didn't fit: nothing written |
| 7 | `Limit` | a count reached: windows, handles |
| 8 | `Unavailable` | the object can't answer now: no desktop, no window, the RTC failed |
| 9 | `OutOfMemory` | the heap refused although the worst case fits (a bug) |

#### Wire formats

Events, trees and answers are `postcard` (pinned with `=`,
`default-features = false`, `alloc`) over `serde`-derived types
`lumen-app-api` owns. postcard with `serde` is one new dependency, in a
PR of its own ahead of #199. serde numbers an enum's variants by their
declaration position (a `#[repr(u8)]` discriminant is ignored on the
wire), so `lumen-app-api` holds a numbered table for every wire enum and
bit set (`Role`, `ActionKind`, `Refusal`, `KeyCode`, `Event`,
`AppAnswer`, `ThemeId`, `Palette`'s field order, button numbers and
mods bits), declared in that order. Field-less enums carry explicit
discriminants with a `const` check that each equals its position, and a
host test pins the encoding of every variant (each data-carrying enum's
tags through an exhaustive match, so an appended variant fails to build
until it is pinned). `From` conversions from `lumen-ui`'s, `lumen-wm`'s
and `lumen-input`'s types (and back, where an app needs them: key codes,
the theme, the palette, actions, buttons, key actions) live beside them in
`lumen_app_api::event`, so those crates need no `serde` and reordering
their variants can't change the ABI; so does `toolkit_key`, the one
mapping from a key event to `lumen_ui::Key` that the kernel's toolkit and
an app's both use (#207). What every wire type shares is
`lumen_app_api::wire`: `wire_enum!`, `DecodeError`, `decode_exact` (take,
then refuse a remainder), `Capped<N>` (a string held to `N` bytes however
it is made: `SetValue`, `Failed`'s text) and `CappedVec<T, N>` (the capped
visitor below: the tree's items). Apps are built into the image with the kernel, so M5 has no
per-message version. A value that doesn't decode, has bytes left over,
or breaks its rules is `BadArgument`, never a trap.

Decoding rules for the wire types (measured with the pinned postcard
1.1.3 and serde 1.0.229 in #215's review; #206 and #207 follow them, each
host-tested):

- **Leftovers:** `postcard::from_bytes` accepts trailing bytes, so decode
  with `take_from_bytes` and refuse a non-empty remainder; fuzz tests feed
  trailing bytes as well as truncations and flipped bytes.
- **Malformed values,** each refused with its exact postcard error in a
  host test (fuzzing can't see a decoder that accepts one without
  panicking): bad UTF-8 (`DeserializeBadUtf8`), a bool byte of 2
  (`DeserializeBadBool`), an overflowing u32 varint
  (`DeserializeBadVarint`), a bad `Option` tag (`DeserializeBadOption`), a
  surrogate `char` (`DeserializeBadChar`).
- **Sizes first:** check `TREE_BYTES` and `ANSWER_BYTES` on what the app
  sent before decoding (never on a re-encode: overlong varints decode, so
  one value has more than one encoding; compare decoded values, never
  bytes). `EVENT_BYTES` bounds the host's own encoding, which the app
  only reads.
- **Sequences:** strings and `serde_bytes` buffers are safe (postcard
  checks the bytes left before allocating), but a derived `Vec<u8>`
  decodes as a sequence like any `Vec`. postcard gives serde no size hint
  when the claimed length is more than the bytes left; otherwise serde
  preallocates up to `min(len, 1 MiB / size_of::<T>())` elements,
  infallibly, before decoding them, which for wide elements is far past
  what the input holds. So every wire `Vec` takes a visitor that refuses a
  length past its cap and preallocates at most `min(hint, cap)`; the tree is decoded node by node
  (`take_from_bytes::<Node>` in a loop, counted against `TREE_NODES`); a
  counting-allocator host test checks the peak.
- **Shapes:** no recursive wire type (`Box<Self>`, `Vec<Self>`, mutual
  recursion: postcard has no depth limit), and no sequence of zero-byte
  elements (`()`, unit structs, `PhantomData`: a short length prefix
  loops practically forever, and decoding isn't charged fuel).
- **Encoding:** a `u8` is one raw byte, wider integers LEB128 (signed ones
  zigzagged); an enum's tag is its declaration position (two bytes from
  position 128); postcard's custom errors drop serde's message, so a
  refusal's log names the decoder, the input's length and the bytes left
  over.

**`Event`** (host to app; each `WindowEvent` becomes one, section 11's
queue rules unchanged):

| Event | Fields |
|---|---|
| `Theme` | `theme: ThemeId`, `palette: Palette` (as the kernel's `ui::palette(&Theme)` gives, so an app's toolkit looks like a kernel window's; always the first event) |
| `Key` | `code: KeyCode`, `mods: Mods`, `action: Press { text: Option<char> }` or `Release` (`lumen_input::KeyAction`, so a release says which key) |
| `PointerDown`, `PointerUp` | `x: i32`, `y: i32`, `button: Button`, `buttons: Buttons` (client coordinates) |
| `PointerMove` | `x: i32`, `y: i32`, `buttons: Buttons` |
| `FocusGained`, `FocusLost`, `Lost`, `Close` | none |
| `Request` | `node: u64` (the app's own id), `action` |

`KeyCode` numbers every `pc_keyboard::KeyCode` pc-keyboard 0.9.0 has
(125, `F1` to `F12` included, each one byte on the wire), with `From`
both ways (`lumen_input::KeyCode` is pc-keyboard's) generated from one
list as exhaustive matches and host-tested over every variant. `Mods`
is one raw byte, `lumen_input::Mods::bits()` (`lumen_app_api::event::mods`:
left and right Shift, Ctrl and Alt, then Caps Lock and Num Lock, bits 0
to 7); `button` is an `event::Button`, a wire enum (left 0 to extra 4, a
number past it refused when decoded), and `Buttons` one raw byte, the
held set's bits, `1 << number`. A `Request`'s `node` is the app's own id
(the kernel maps the global one back, below). A `set_value` text past `SET_VALUE_BYTES` is refused
(wrong arguments) before it is encoded, so every `Event` fits
`EVENT_BYTES`; a host test encodes the largest, with the biggest
varints.

**`Tree`** (app to host; `lumen_app_api::tree`): the app's nodes in
pre-order, each a `Node` encoded one after another (no length before
them: no bytes is a tree of no nodes), a role-fused enum mirroring
`lumen-ui`'s per-role constructors, so a mismatched value, a flag a role
doesn't allow, a window's root, children under a leaf, a stray item and a
disagreeing selection can't be written (each variant's fields: `common`,
the five every node has, first, then its own, in the table's order):

| Node | Fields (each also `id: u64`, `name`, `bounds: Rect`, `disabled`, `hidden`) |
|---|---|
| `Group` | `children: u32` (its direct children, which follow it) |
| `Label` | `text` |
| `Button` | `focused` |
| `Checkbox` | `focused`, `checked` |
| `TextField` | `focused`, `text` |
| `List` | `focused`, `selected: Option<u32>`, `items: Vec<Item>` |
| `Menu` | `focused`, `expanded`, `items: Vec<Item>` |

`Item` is `{ id, name, bounds, disabled, hidden }` (`common`'s fields): a
list item's `selected` flag is derived from its list. Names and texts are
`tree::Text` (`Capped<TEXT_BYTES>`: an app's toolkit cuts a longer one, a
longer one decoded is refused), shown sanitised as log text in the
kernel's rows (`ui tree` can't be forged a line). What is left to
check: ids non-zero and unique (items' too); at most one node focused; `selected` below
`items.len()`; every `bounds` with `x + w` and `y + h` fitting in `i32`;
`children` counts consuming the list exactly; at most `TREE_NODES` nodes,
items counted, and `TREE_BYTES` encoded. The kernel decodes it
iteratively (its stack on the heap, bounded by `TREE_NODES`), never by
recursion on a kernel stack. Top-level nodes hang under the window's
root. `Tree::from_automation` (an app's `Ui::nodes()`, the SDK's publish)
and `Tree::rows` (the kernel's copy, each id mapped) are the two ways
over the wire; a host test sends every widget `lumen-ui` has across and
back.

**`AppAnswer`** (app to host; `lumen_app_api::answer`): `Ran`,
`Refused(Refusal)` (`NoNode`, `Disabled`, `Hidden`, `NotAllowed`,
`Arguments`, numbered so), `ClosedItsWindow`, or `Failed(FailText)` (text
up to `TEXT_BYTES`, then cut to `LOG_BYTES` and sanitised as log text).
`AppAnswer::from_served` is what the SDK answers for what
`lumen_ui::serve` said. The runtime builds what the asker gets from its
own copy of the tree (below).

#### Running an app

**Loading (`run [--without <grant>] <app> [args]`):**

1. `run` finds the module (`BUNDLED`, then `TEST_APPS`; `run: no app
   <name>`), checks its size (`MODULE_BYTES`) and its manifest
   (`runtime::precheck`, which the ktests share), and encodes the
   arguments (`ARGS_BYTES`; `run: <name>: arguments too long (past 1024
   bytes)`, or `an argument holds a NUL`).
2. Under the app table's lock, it takes a slot (refused past `slots`,
   `run: <name>: <n> apps running already`), and gives the app its id
   and an entry. A refusal above takes neither.
3. It spawns the app's thread, stores the `JoinHandle` in the entry
   under the lock, then opens the entry's start gate: the thread waits
   at that gate first, so an app that fails at once can't end before
   its handle is stored. A `SpawnError` frees the slot at once.
4. On its own thread (`loading`), the app's `Engine`, `Module::new`
   (validating, eager: a start section, a second memory), our checks of
   its imports, exports and memory, the linker with the `lumen`
   functions and instantiation (the limiter refusing a second table or
   too big a one). Then, unless a kill came meanwhile (the app ends
   `killed` without running), it is `running` ("started") and
   `lumen_main` is called.
5. `run` waits for "started" or the failure, and prints `run: started
   <name> as app <id>` or `run: <name>: <why>` (not valid (wasmi's
   message), a refused import or export, not enough memory; `killed` if
   a kill came while loading). `q` or Ctrl-C stops the wait with `run:
   <name>: still loading (kill <id> ends it)`, as `perf burn` does
   (#212); there is no wall-clock timeout. The launcher doesn't wait: it
   reports a failure through `Then::Failed`.

A load failure is the app's outcome (`didn't start`): `run` prints it
while it waits; once nobody waits (`q`, or a launch nobody watches),
the app's thread logs `WARN  app: <name> (app <id>) didn't start:
<why>` instead: whichever of the app's thread (recording the failure)
and the waiter (giving up) comes second reports it, decided under the
app's lock, so it is reported once. A `run` whose console closed doesn't
take the failure (nobody would see it printed): its drop logs it. The app's slot is freed when the
reaper joins its thread, as for any ending.

**Slices.** The app runs with `call_resumable`. A slice ends when its
fuel runs out (`TypedResumableCall::OutOfFuel`), at a blocking host call
(`window_wait`, `time_sleep`), or at a host call's entry with less than
`CALL_FUEL` left. Every end goes through one path, `runtime::end_slice`
(over the store or a host call's `Caller`: it counts the slice, adds the
fuel used to the app's total, checks the kill flag and the close grace,
which lives in the store's data), then the runtime yields
(`sched::yield_now`) or blocks (host.rs's `blocking`, with
`AppState::waiting` owning the `waiting` phase), and
`runtime::begin_slice` refuels with `max(SLICE_FUEL, required_fuel)`
less any debt (bulk memory operations are charged by size; a smaller
refuel could keep a slice from ever making progress). The last slice,
partial, is counted when the app ends. A blocking call checks the kill
flag again just before it blocks, and a deadline already reached doesn't
block but yields. Fuel is the safe point
where an app can be stopped; the timer still preempts the thread inside
a slice, so a busy app never keeps a CPU from the shell.

**Events and `window_wait`.** The runtime keeps at most one event in
hand per window.

- `window_wait` returns 1 at once with one in hand; otherwise it is
  `WindowHandle::wait_event_until`, its deadline converted from
  `time_now`'s clock to `time::now_ns`'s (`-1`: none, `wait_event_until(
  None, No)`); the event it wakes for is kept in hand. A `Woken` wake
  re-checks the kill flag and waits again until the deadline. Animating
  without a deadline, `animating` other than 0 or 1, or a deadline below
  -1 is `BadArgument`. A `Request` it wakes for is kept in hand as it
  came (not started): if `window_next_event` then skips it or answers it
  without the app, that call returns 0, so a 1 may be followed by a 0.
- `window_next_event` checks its output range first, then takes the
  event in hand (or the next queued one) and encodes it into a kernel
  buffer of `EVENT_BYTES`; only an app buffer shorter than the encoding
  is `BufferTooSmall`, which keeps the event in hand. Every event fits
  (`SetValue` holds a request's text to `SET_VALUE_BYTES`, however it is
  made), so a failed encoding is a kernel bug: one ERROR, then a crash
  report (debug assertions are on in every built kernel). Handing over `Close` starts the close grace. A `Request` is
  `start`ed (`lumen_app_api::request::take`): a `Skipped` one (its asker
  cancelled) is dropped and the next event taken; one whose node isn't in
  the newest tree is answered `no node`, one whose node's role doesn't take
  the action `NotAllowed` (a window's root actions, `close`, `minimize`,
  `restore`, never reach an app), one whose `set_value` text is past
  `SET_VALUE_BYTES` wrong arguments, each without the app, and the next
  event taken; otherwise it is running and handed over as the app's id
  (kept in hand, still running, across `BufferTooSmall`). So a request runs
  only once its asker can no longer cancel it.

**One end reason, first writer wins.** Returning from `lumen_main`,
`exit`, `panic`, a trap, a kill or the close grace records the app's end
reason in its state (under the state's own lock), unless one is recorded
already (in #199 the app's own thread is the only writer; the rule is for
#206's and #207's). `exit`, `panic` and a kill seen by a host call raise
one runtime-private host error, `Stop`, after recording why in the
store's data. wasmi hands it back as a `HostTrap`, or as a plain error
when the call it ends is a tail call to a host function from
`lumen_main`'s own frame (on any slice: the first call or a resume) or a
`lumen_main` that is an exported import. Both go through one reader,
`runtime::step`, which takes the recorded reason, so it is never resumed
and never reported as a trap
(`app::tests::a_kill_at_a_host_call_is_never_a_trap`,
`app::tests::a_kill_at_a_resume_is_never_a_trap`).
The reason decides the app's one end line:

| Reason | Line (target `app`) |
|---|---|
| returned or `exit` | `INFO  app: <name> (app <id>) exited with <code>` |
| killed | `INFO  app: <name> (app <id>) killed` |
| close grace | `INFO  app: <name> (app <id>) ended: didn't close its window` |
| `panic` | `WARN  app: <name> (app <id>) ended: panicked: <msg>` (cut and sanitised as log text) |
| a trap | `WARN  app: <name> (app <id>) ended: trap: <wasmi's trap>` (`wasm \`unreachable\` instruction executed`, `out of bounds memory access`, `call stack exhausted`, ...) |
| didn't start | `WARN  app: <name> (app <id>) didn't start: <why>` (only when no `run` waits for it, above) |

So an app that faults gives exactly one end-line WARN, and the kernel
goes on (its window may log section 11's "behind" WARN of its own).

**Ending.** The app's thread destroys its window (its `Pending` requests
answered "window closed", section 11), answers a started request with
`Answer::OwnerEnded` (below), closes its handles, drops its store,
instance, module and `Engine`, logs its line, marks its state `ended`
and wakes the state's wait queue and the runtime's (private) `ENDED`
queue. The table's entry holds the thread (`Starting`, `Spawned` with
its `JoinHandle`, `Joining`) and an `Arc` of its state. The `app-reaper`
thread (started by the first `run`; a failed spawn of it is logged once
and retried by every later `run`, so ended apps keep their slots until a
later `run` starts it, never for good) waits on `ENDED`; for each ended
entry it takes the `JoinHandle` under the lock, joins outside it, then
removes the entry, which frees the slot. So `apps` lists an app until it
is joined (`ending`, then `ended`, meanwhile). Tests confirm the join by the thread id from the
app's `apps` record (two copies of one app share `app:<name>`), gone
from `threads`.

**Kill (`kill <id>`)** takes the entry's state `Arc` under the lock
(`kill: no app <id>` without one), sets the kill flag, wakes the app's
waits (its wait queue and its window's `Waker`), then waits on the
state's queue until its thread has marked it `ended` (its outcome is
recorded before), or `KILL_WAIT_NS` passes. It prints from that state
(`runtime::kill_outcome`): `kill: app <id> (<name>) killed`, `kill: app
<id> (<name>) had already ended: <its line's reason>` (another reason
won, or `didn't start: <why>`), or `kill: app <id> (<name>) still ending
(<place>)`, the place one of `loading`, `in a slice`, `in <host call>`
(`in time_sleep`) or `ending` (its outcome recorded, its thread not yet
done); the reaper joins it either way. A second `kill` of an ending app
waits the same way.

**Close grace.** A `Close` event (the close box, `ui invoke <root>
close`) is the app's to act on (an SDK app returns, and the `Window`
dropped with it closes the window; closing the `Window` handle ends the
grace).
Once it has been given `Close` and still holds the window, it has
`CLOSE_GRACE_SLICES` more slice ends before it is ended. An app that
ignores `Close` while blocked has no slice ends and stays open until
`kill` (section 11's "still open"); one that never reads its events
never sees `Close`.

**Log lines** are `<LEVEL> app: <name>: <text>` (`INFO  app: hello:
hello from wasm`): one line per call, text up to `TEXT_BYTES`, then cut
at `LOG_BYTES` on a char boundary, each control character and each line
or paragraph separator (U+2028, U+2029, which the harness's
`splitlines` also splits on) shown as U+FFFD, so an app can't forge a
line a test matches on. The result, `lumen_app_api::text::LogText` (the
only way such text reaches a log line), is at most `3 × LOG_BYTES` bytes
(a 1-byte control character becomes U+FFFD's 3). Titles, `panic`'s
message, a trap's, `Failed`'s text and an unknown manifest grant (cut at
`text::NAME_BYTES`) are sanitised the same way.

#### Limits

`lumen_app_api::limits`:

| Limit | Value | When reached |
|---|---|---|
| `APP_BUDGET` | 128 MiB | a `const` assertion (`MAX_APPS × APP_WORST`) |
| `MAX_APPS` | 4 listed (loading until joined) | `run` refuses |
| `KERNEL_RESERVE` | 32 MiB | fewer slots on a smaller machine |
| `MODULE_BYTES` | 1 MiB | `run` refuses |
| `CODE_FACTOR` | 8 | the code charge |
| `MEMORY_PAGES` | 256 (16 MiB) | refused at load |
| `VALUE_STACK_BYTES` | 1 MiB | trap: call stack exhausted |
| `CALL_DEPTH` | 1000 frames | trap: call stack exhausted |
| `APP_OVERHEAD` | 2 MiB | the overhead charge, at least its parts' sum |
| `STORE_BYTES` | 512 KiB | its part for the store, instance, call stack and handle table |
| `TABLE_BYTES` | `TABLE_ELEMENTS` × 16 | its part for the table |
| `TREE_COPIES_BYTES` | 1 MiB | its part for its published tree (#207: what the kernel holds for it and a republish's peak, measured) |
| `GLYPH_CACHE_BYTES` | 128 KiB | its part for the glyph cache (#206; a `const` assertion in `font.rs` ties it to `font::Text`'s) |
| `WINDOW_PIXELS` | 307,200 (640 × 480) | `window_open`: `BadArgument` |
| `SURFACES`, `BYTES_PER_PIXEL` | 3, 4 | the window charge |
| `APP_WORST` | about 30.5 MiB (the sum above) | |
| `TABLE_ELEMENTS` | 10,000 | refused at load; `table.grow` returns -1 |
| `SLICE_FUEL` | 1,000,000 | the slice ends |
| `CALL_FUEL`, `LOG_FUEL` | 1,000, 10,000 | fuel per call, per log line |
| `BYTE_FUEL`, `PIXEL_FUEL`, `EVENT_FUEL` | 1, 1, 16 | fuel per byte, pixel, trace event |
| `TEXT_CHAR_FUEL` | 256 | fuel per character of a text layout pass (`text_measure` one, `text_draw` two) |
| `NODE_FUEL` | 1,280 | fuel per node of a published tree, items counted, and of the tree it replaces (#207; priced from a disjoint republish, `limits.rs`) |
| `UI_TEXT_PX` | 16 | an app toolkit's text size (the kernel windows') |
| `WINDOWS` | 1 per app | `Limit` |
| `COMMIT_RECTS` | 16 | `BadArgument` |
| `ARGS_BYTES` | 1024 | `run` refuses |
| `TEXT_BYTES`, `TITLE_BYTES` | 1024, 64 | `BadArgument` |
| `LOG_BYTES` | 256 | cut |
| `TEXT_PX_MIN`, `TEXT_PX_MAX` | 8, 96 | `BadArgument` |
| `SET_VALUE_BYTES` | 512 | the request refused (wrong arguments) |
| `EVENT_BYTES` | 1024 | every event fits |
| `TREE_NODES`, `TREE_BYTES` | 1024, 64 KiB | `BadArgument` |
| `ANSWER_BYTES` | 4 KiB | `BadArgument` |
| `TRACE_EVENTS_PER_READ` | 256 | the rest on the next read |
| `CLOSE_GRACE_SLICES` | 4 | the app is ended |
| `KILL_WAIT_NS` | 1 s | `kill` says "still ending" |

The heaviest app, the viewer (#204), fits `MEMORY_PAGES`, and takes all
of it (16 MiB): its 1 MiB wasm stack and up to 512 KiB of data, a
full-size canvas, and, while one decodes, `zune-png`'s buffers for the
costliest PNG it takes (`lumen_view::admit` bounds the file's length and
its header alone, so the worst is both at once: 16-bit RGBA at the
inflated-bytes cap, interlaced, its stream padded to the 512 KiB cap; the
inflated rows, two 16-bit copies of the samples and `lumen-png`'s 8-bit
buffer, reserved first, and the file's two copies, about 11.3 MiB; the
pixels only once those are freed), or the image shown (1280 × 800 × 4
bytes) and its fitted copy. `lumen-view`'s `tests/memory.rs` measures it
through the app's own allocator logic: a 12.7 MiB peak of a 14.5 MiB heap
(1.8 MiB to spare), and nothing refused in three rounds of images,
fragmentation included. wasmi runs floating point as soft-float in the
kernel (no SSE); #197 checks the results' bits.

#### Apps' automation trees (#207)

- **Ids:** apps run `lumen-ui` inside the module, so their
  `lumen_ui::NodeId`s come from the module's own counter: app-local
  `u64`s. For each window the kernel keeps a map from the app's ids to
  global `lumen_ui::NodeId`s: an id first seen in a published tree gets
  `NodeId::fresh()`, so global ids are never reused in a boot and two
  apps' trees (even two copies of one app, whose local ids are the same)
  never share one. An id missing from the newest tree leaves the map,
  so its global id names nothing again; if it comes back it gets a new
  one. The map goes with the window.
- **`ui tree`** shows an app's window like a kernel one: the root (the
  compositor's), then the app's nodes with global ids, bounds in screen
  pixels.
- **One request per window.** An app's window (opened with
  `desktop::Requests::OneAtATime`, so it never exists without the rule)
  holds at most one request that is queued or started and not yet
  answered: `desktop::push_request` refuses another with `Refused::Busy`,
  the request marked held out (`ui invoke: a request is still queued or
  running in window <id>`: retrying helps only once that one is answered),
  before anything is queued, which also covers a second
  request pushed before the first is taken. The request holds the slot
  (`ui::RequestSlot`), which frees when it is answered (`OwnerEnded` and
  "window closed" included), skipped or refused, before its asker is
  woken. A request its asker cancelled stays queued, holding the slot,
  until the app takes (skips) it: an app that never reads its events keeps
  its window busy (its root's actions still work). So `window_answer`
  answers *the* running request, and requests carry no ids. Deadlines are
  section 11's ("A request is a reply slot").
- **Answers.** The app runs `lumen_ui::serve` (the same path as a
  click), publishes its tree, then answers. The runtime builds the
  asker's answer from its own copy of the newest tree: `Ran` becomes the
  node's row (its depth and global id from the map), or `RanNodeGone`
  if the node isn't in it; `Refused(NotAllowed)` becomes
  `InvokeError::NotAllowed(node.role(), action.kind())` and the other
  refusals their `InvokeError`s; `Failed` passes through, cut and
  sanitised; `ClosedItsWindow` closes the window first (its handle
  closed, as `handle_close` would), then answers, keeping section 11's
  rule that this answer comes once the window is gone.
- **`Answer::OwnerEnded`**, a new `ui::Answer`: the started request when
  the app ends or closes its window unanswered. `ui invoke` prints `ui
  invoke: window <id> closed while running it (it may have run)`. None
  of section 11's answers would be true, and a `Started` dropped
  unanswered is "dropped while running" (a bug). The app's window
  (`app/window.rs`'s `AppWindow`) drops its desktop handle first in its
  `Drop`, then answers its running request, so the window is gone first;
  `ClosedItsWindow` closes the window's handle (as `handle_close` would:
  later calls on it are `BadHandle`, and a close grace ends), then answers
  (`answer_request`). The ktests check both through a probe on the reply,
  which sees the window gone when the asker is answered.
- **Refusals** of a tree or an answer (the caps included) log a DEBUG
  line naming the call, the length and why; a refused `Refused(_)` for a
  node gone from the newest tree answers `no node`.
- **Ids** (`lumen_app_api::tree::IdMap`): each window's app ids and global
  ids both ways, the reuse and leave-and-come-back rules host-tested.
- **The SDK** (`lumen_app::ui`): `HostText`, and `Toolkit`, which runs an
  app's `lumen_ui::App` one step per event as the kernel's `ui::Toolkit`
  does (painted, committed, published, then answered); an app may take
  some events itself first (the calculator its keys). The calculator
  (`apps/calculator`, bundled) is the first app built on it; its
  arithmetic is `crates/lumen-calc` (left to right, exact decimals,
  host-tested).
- **Root actions** (`focus`, `minimize`, `restore`, `close`) work on an
  app's window as on any.

#### The `Trace` capability (#198, read by X-ray in #208)

- **Streams:** `stream` 0 is a CPU's main trace ring, `stream` 1 its
  X-ray ring (#198; `lumen_trace::Stream`). Section 14 says which kinds
  go to which ring (`lumen_trace::kind::stream_of`).
- **`trace_read`** reads with `lumen_trace::TraceRing::read_since(since,
  out) -> Since { copied, missed, next }`, which never blocks a writer:
  every sequence number from `since` to `next` is either copied or
  counted in `missed` once (gone from the ring, dropped, overwritten).
  An event not yet readable (being written, or its number claimed but
  not yet its slot) ends the read there, for the next read; one that
  never lands (dropped, or its writer stopped mid-write) can't be told
  from that until the ring's head passes its number by the ring's size,
  and is counted missed then, so a real drop holds a reader there for a
  lap. `buf` gets the trace header, then `copied` trace events (at most
  `TRACE_EVENTS_PER_READ` and what fits); the result is `copied`. A `len`
  below one header and one event is `BufferTooSmall`; a negative or
  unknown `stream`, a `cpu` that is negative or not online, or a `since`
  that is negative or past the ring's next sequence number, is
  `BadArgument` (`since` equal to it copies nothing yet). `since` 0
  reads from the oldest event the ring holds (the numbers before it
  counted missed).
- **`trace_cpus`** is one past the highest online CPU's index
  (`trace::cpus`): every CPU `trace_read` accepts is below it, but an
  index below it may still be offline (indices can have holes).
- **`pid`** is 0 for every event in M5 (the kernel's); an app shows by
  its thread's id in switch and run-queue events, which `apps`' `thread`
  column names.
- **Kinds** are numbered in one place, `lumen_trace::kind` (#198 adds
  it): numbers and argument meanings stable, pinned by a
  `cargo:lumen-trace` test, used by the kernel's `trace::Kind` and
  re-exported by `lumen-app-api`. The kinds X-ray reads have typed
  layouts there (`Switch` with `ThreadState`, `RunQueue`, `Irq`,
  `LockWait`), each pinned.

#### Building and bundling

- **On main (#196):** `apps/` is its own Cargo workspace (members
  `[!.]*`, so every directory but a dot directory is a crate; the root
  workspace excludes it), built for `wasm32-unknown-unknown` with
  `--release` into `target-apps/`, its `Cargo.lock` committed. `make
  apps` (part of `make build`) copies one module per `apps/*/` crate to
  `build/apps/<name>.wasm`, and `harness:test_wasm_module::*` checks
  them after they are built (section 16).
- **What #199 adds:** the runtime's test apps are `apps/test-<name>`
  (`hello`, `busy`, `trap`, `greedy`, `nocap`, `abuse`, and #206's
  `window`, built on the SDK; #196's `hello`
  moved there and exports `lumen_main`), sharing `apps/testkit` (the
  `lumen` imports, the manifest macro, a panic handler
  reporting through `panic`, a bump allocator of 256 KiB, never freeing:
  `abuse window`'s trees are `TREE_BYTES` long; #206's `lumen-app` is the
  real SDK). They can't live under `apps/tests/`: the member glob would
  take `tests` itself as a member, and excluding it also excludes
  `tests/*`. Their fixed memory (2 MiB) is one `rustflags` entry in
  `apps/.cargo/config.toml` (`window`'s is 4 MiB, in its `build.rs`: its
  `ui-big` toolkit holds more widgets than `TREE_NODES`). `make
  apps` copies only crates that build a `cdylib` (`testkit` and #206's
  `lumen-app` make none), the test apps to
  `build/apps/tests/<name>.wasm`, with `cp -p` and only stale modules
  removed, so the kernels rebuild only when a module did. The Makefile's
  `app_out` is the one place a module's path is worked out. The kernel
  builds depend on `make apps` (order-only), and `KERNEL_SRCS` includes
  `apps/` and `apps/Cargo.lock`. The test apps use `testkit`'s imports
  directly: declared once, the calls that only read the app's memory
  `safe fn` (the host checks every range), those that write into it
  (`args_read`, `trace_read`, `window_next_event`, `text_draw`) `unsafe
  fn`, the caller owning the buffer.
  `kernel/build.rs` embeds both sets in both kernels, sorted by name, as
  `app::BUNDLED` and `app::TEST_APPS` (`&[App]`, `App { name, thread,
  module }`, `thread` the `app:<name>` string); it refuses a stem
  outside `[a-z0-9_-]`, one over 11 bytes (`MAX_APP_NAME`: `app:<name>`
  must fit in a thread's 15-byte name, section 6, "Kernel threads") or
  one in both sets, and a missing `build/apps`
  (`make lint` runs before `make build` in CI) gives empty lists, said
  in a build warning (as does a directory that can't be read). Every dependency in `apps/` is pinned with `=`.
  Crates the apps share with the kernel (`lumen-app-api`, `lumen-ui`,
  `lumen-demo`, ...) are path dependencies on `crates/`.
- **What #206 adds:** `apps/lumen-app`, the SDK (an rlib): `main!`
  (`lumen_main` from `fn() -> i32`), `manifest!(time, desktop)`, a global
  allocator (`lumen-heap`'s free list, the kernel heap's own logic, over
  `[__heap_base, end of memory)`, never growing it), a panic handler
  formatting into a fixed buffer for `panic`, and wrappers: `Handle`
  (`NonZeroU32`), `log`'s functions, `Time`, `Desktop` (`open_window`,
  `line_height`, `measure`) and `Window` (its canvas, `fill`, `text`,
  `commit` in turns of `COMMIT_RECTS`, `wait`, `next_event`, `wait_event`;
  dropping it closes the window), `Trace` (`cpus`, `read`: X-ray, #208);
  `Event`'s `Mods`, `Buttons` and their bit constants re-exported. The log
  functions cut a text past `TEXT_BYTES` on a char boundary (the kernel
  refuses a longer one), the panic message is cut to make room for ` at
  <file>:<line>`, a refused free ends the app (as a bad free in the
  kernel's heap does), and `open_window` checks the size before it
  reserves its canvas. `apps/template`, the app to copy, sets
  its own memory (4 MiB) in its `build.rs`: link arguments from a build
  script come after the workspace's `rustflags`, and wasm-ld takes the
  last. `make app NAME=foo` (`tools/new-app.sh`) copies it to `apps/foo`
  and renames the crate. It refuses, making nothing
  (`harness:test_make_app::*`): a name `kernel/build.rs` wouldn't embed
  (its rule: lower case letters, digits, `-` and `_`) or one not starting
  with a letter (the script's own: a leading `-` or `_` breaks cargo for
  the whole workspace); a `test-` or `test_` name, or a test app's module
  name (`hello` for `apps/test-hello`: `kernel/build.rs` refuses a name in
  both sets); an existing directory; a name the workspace has already
  once `-` and `_` are alike (cargo names the output file with `_`, so
  `a_b` would overwrite `a-b`'s module): a directory, or a package of
  `apps/Cargo.lock` (a path dependency, what it pulls in, a registry
  crate: `cargo build -p` would be ambiguous; a lock file naming none
  fails closed); a name over 11 bytes (`kernel/build.rs`'s
  `MAX_APP_NAME`, for the same reason). The copy is made in a dot directory (not a member) and
  moved into place only once finished, `apps/<name>` checked again just
  before. Nothing needs registering: the member
  glob takes the crate, `make apps` bundles its module, and the launcher
  lists it.
- **What #207 adds:** `lumen_app::ui` (`HostText`; `Toolkit`, with
  `run`, `run_with` (an app takes some input events itself first: the
  calculator its keys), `step`, `refresh` and `publish`, a refused tree
  answered `couldn't publish`), `Window::publish(&Tree)` (`PublishError`:
  the tree's or the host's refusal), `Window::answer`, `Window::handle`,
  and `lumen_app::report` (an app's exit code from its steps' result): what
  wave 3's apps build on; the window's canvas is a
  `lumen_gfx::Surface` (`Window::surface_mut`), which lumen-ui paints.
  `apps/calculator` (bundled: the launcher lists it), with its logic in
  `crates/lumen-calc` (a path dependency, host-tested); `test-window`'s
  toolkit modes (`ui`: Counter in the SDK's toolkit; `ui-trap`; `deep`:
  the deepest tree; `deaf`: a published button whose events are never
  read).
- **What #222 adds (the SDK's tick loop, before wave 3):** no host call
  changes; `window_wait`'s deadline and `animating` carry it.
  - **Who gets an event:** `Event::offer` (exhaustive) says. Input is
    `Offer::App`: offered to the app's `take` first. The focus coming or
    going and `Lost` are `Offer::Notify`: offered to `take`, then always
    handed to the toolkit, which cancels a press and moves the focus,
    unless `take` quits. A `Request`, `Close` or `Theme` is
    `Offer::Toolkit` and never reaches `take`. So a request is always
    answered, a close always ends the loop and the palette always follows
    the theme, whatever an app takes.
  - **`Toolkit::run_with(take)`:** `take` returns `ui::Taken`. `Pass`
    hands the event to the tree. `Took` means the app acted on it (a
    game's arrows, the calculator's keys), and the window is refreshed.
    `Quit` stops the loop.
  - **`Toolkit::run_until(time, take)`** runs an app that is `ui::Ticking`:
    - `period()` is asked before every wait, and `None` waits for events
      only, so the desktop can settle;
    - `animating()` is the wait's flag while ticking (yes by default);
    - `tick(now)` returns a `Flow`: `Carry` or `Quit`. A tick may come
      before a target of the app's own (one it just moved later, or the
      floor after a tick whose period it gave as 0: one extra, never more),
      so it checks `now`; at a due moment an app gives the period of the
      tick after it (a blink: a fixed 500 ms with a "solid until" time set
      on each key).

    Each turn handles every event waiting first. Then, once a `Theme` has
    come (the `Toolkit` remembers, through any loop or `step`), it takes a
    due tick and refreshes, else waits until the deadline.
  - **How a loop ends:** both loops return `ui::Ended`, `Closed` or `Quit`.
    The window stays open until the caller drops the `Toolkit`, which owns
    the `Window`; a request still queued then is answered "window closed".
    A failed host call ends a loop with its error, logged first as one
    ERROR naming the stage (`ui: run_until: wait: <e>`).
  - **Pacing** is `lumen_ui::Pacer`, through two calls: `poll(period, now)
    -> Pace { Due, Until, Idle }` before every wait (one clock read a turn,
    the same `now` handed to the tick), and `ticked(after)` once a due
    tick's work is done (a second or stray `ticked` moves nothing). All
    host-tested:
    - a period below `MIN_PERIOD_NS` (10 ms) is taken as that;
    - due is decided on the schedule as it stands, before a changed period
      moves it;
    - a changed period not yet due takes the earlier of the old deadline
      and one new period from now, so a shorter period takes effect at
      once and a longer one after the next tick, and a period that changes
      on every ask (a clock's "until the next second") still ticks at its
      boundary;
    - a tick is due once; the next is a period after it was found due, or
      a period after its work if that ran past that deadline.

    So ticks are always at least a period apart and never burst to catch
    up. The next tick counts from when a tick was found due, not from its
    deadline: a period asked at that moment is measured from then, and
    counting it from an earlier deadline would tick early. A steady period
    therefore drifts late by each tick's lateness.
  - **Custom drawing:** `lumen_ui::App::paint_over` is handed an `Over`:
    the surface, the window's text and palette, and the rectangles being
    repainted. `App::take_drawn` reports the rectangles the app's drawing
    will now paint differently from what is shown. `lumen_ui::repaint`
    then:
    - merges the tree's damage and the app's through a `lumen_gfx::Damage`,
      then makes them disjoint (each with the ones before it subtracted;
      past `REPAINT_RECTS` pieces, their bounding box): a commit pays by
      the pixel, and an app drawing once per rectangle (blended text)
      would draw an overlap twice, so each pixel is painted and paid once;
    - gives every rectangle the tree's paint first (its background, and
      the widgets reaching it), inside the tree's area or not, whether the
      repaint is full or partial;
    - calls `paint_over`, which draws only its foreground (Life's cells, a
      clock's hands);
    - commits.

    What is drawn outside the rectangles shows only at a later commit
    covering it, and text drawn there is still charged by the character.
    The tree's `changed` alone decides a publish, so a frame of the app's
    own never republishes an unchanged tree. That is the SDK's one frame
    path, `Toolkit::refresh`.
  - **Layout:** `Toolkit::with_area` lays the tree out in part of the
    client (`BadArgument` if none of it is on the client); the rest is the
    app's. Labels that change on every tick should be `fixed_label`s: a
    label changing width moves the widgets after it, repainting and
    committing the whole window.
  - **Also new:** `Toolkit::palette()` (all zero before the first `Theme`).
    `lumen_app::gfx` re-exports `lumen-gfx`: its `Rect` is for drawing and
    damage, and the root `lumen_app::Rect` is the wire layout's.
  - **The harness:**
    - `gui.press_animated` presses in a window that may be animating
      (through `gui.press_button`, as `press_buttons` does) and confirms
      by the tree (`wait_ui`, its predicate false before the press), never
      by `wait_settled`;
    - `gui.invoke(..., animating=True)` retries a held slot instead of
      waiting for the desktop to settle, which it never does while a
      window animates; it fails `INVOKE_HELD_TIMEOUT` seconds after the
      first held answer.
  - **`test-window tick [greedy|quit|slow]`** exercises it (`slow`: each
    tick's work longer than its period, the next still a period after it).
- **What #200 adds:** `apps/clock` (bundled, `caps=time,desktop`; the
  launcher lists it), on `run_until`. Its window is
  `lumen_clock::face::Face`, a `lumen_ui::App` in `crates/lumen-clock`: an
  analog face drawn in `paint_over` (sixty dots, three hands and a hub in
  the theme's colours) above a tree holding one `fixed_label`, `Time`,
  whose value is the time (`HH:MM:SS`, from `time_wall`, taken as UTC, as
  `date` shows it). When the time changes, only the boxes of the hands
  that moved (each exactly its brush at either end) are its `take_drawn`
  damage: the second hand's every second, the minute hand's every fifth,
  the hour hand's every minute; host tests check that a partial repaint
  leaves what a full one does, a second at a time over twelve hours
  (compared every 37 s, through the first two minutes and around each
  hour's turn) and after 1,500 jumps. It reads the RTC every 250 ms; its waits aren't `animating`
  (a tick finding the same second draws nothing), so the desktop settles
  between ticks. A failing RTC is handled as the taskbar's clock does
  (`lumen_clock::rtc`): the last good reading until `CLOCK_FAILURES` (3)
  reads fail in a row, then `--:--:--` and no hands; the first failure of a run a WARN, a
  recovery an INFO, one at startup showing no time rather than ending the
  app. `run clock --at H[H]:MM[:SS]` freezes it: no period at all, so the
  window settles for good (its golden); anything else is one ERROR naming
  the argument, with the usage, and exit 2 (`lumen_clock::args`). Its
  geometry has no floats (an integer sine table in half-degree steps), as
  no app has `libm`.
- **What #201 adds:** `apps/comet` (bundled, `caps=time,desktop`; the
  launcher lists it), Comet, an original snake-style game: a comet steered
  across a sky of cells with the arrow keys, its tail growing with each
  grain of stardust, past each level's asteroids (our own layouts: Open
  sky, Twin belts, Crossing, Ring of rocks). Its logic is
  `crates/lumen-snake` (a path dependency, host-tested): steering (a turn
  back into the tail refused, one turn a step), growth, the edge, the tail
  and asteroids ending a flight, a level every few grains (its field placed
  on empty cells, never the two straight ahead of the head), stardust only
  on empty cells, `Rng` (SplitMix64, checked against its published
  outputs), so one seed plays one game, and `Game::step` reporting exactly
  the cells it changed. On the SDK's tick loop: flying, it steps on a
  period that shortens each level; `Step` switches to stepping, one step
  per press and no period, so the window settles between presses (`run
  comet --seed N --step` starts that way, for its golden and tree tests);
  `Play` flies again. Over, `Play` and `Step` are disabled until
  `Restart`, which starts the next game in the mode it was launched in.
  Each step's changed cells are its `take_drawn` damage (Restart the whole
  sky), drawn in the theme's palette; `Score`, `Level`, `State` and
  `Comet` (the head's cell and the heading the next step takes, so a test
  sees a turn arrive) are `fixed_label`s whose texts (lumen-snake's label
  functions) all fit their width in the UI font, a host test measures;
  arrow presses are taken before the toolkit. Arguments it doesn't take (an
  unknown one, a flag twice, `--seed` without a number or with a bad seed,
  an argument that isn't UTF-8) are one ERROR naming them, then exit 2; any
  other failure to read the arguments is exit 1. Launched without `--step`
  it flies from the start, and its Restarts fly too.
- **What #202 adds:** `apps/life` (bundled, `caps=time,desktop`),
  Conway's Game of Life on the SDK's tick loop, replacing #157's kernel
  Life window (`demo.rs`'s window path is gone; the text-mode `life`
  command stays). Its rules and patterns are `lumen_demo::life`'s, a path
  dependency (host-tested, `cargo:lumen-demo`).
  - **The window** (600×320): a tree on the left (`with_area`): the
    `Patterns` list, `Start`, `Stop` and `Step`, and three fixed-width
    labels, `Pattern`, `Generation` and `State` (`running` or `stopped`).
    The grid is the rest, the app's own: a line in the palette's border
    colour, then cells 7 pixels square on an 8-pixel pitch, live ones in
    its accent. The pattern running is the list's selection (from the
    start, the focus left alone: `Ui::set_selected`) and the `Pattern`
    label's text.
  - **Running**, a generation every 100 ms (`Ticking::period`, animating,
    focused or not); a step commits the box around the cells that changed
    (`take_drawn`) and the `Generation` label. **Stopped**, no period:
    the window settles, and `Step` advances one generation. A pattern
    chosen in the list starts it again from that pattern at generation 0,
    running or stopped as it was.
  - **`run life [--stopped] [pattern]`** (`gun` without one; the launcher
    starts it running with `gun`); anything else is one ERROR naming it,
    with the usage, and exit 2.
- **What #203 adds (the text editor):** `apps/editor` (bundled; `caps=time,desktop`), its logic
  `crates/lumen-edit` (a path dependency, host-tested): the buffer and cursor, a typing run undone as
  one edit, the `Limits` (the text's bytes, a line's, and the history's edits and bytes, so its memory
  is bounded), a text given `normalise`d (a tab as spaces, `\r` as `\n`) and a cut one reported
  (`Loaded`), the status line, the `Change` naming the lines to redraw, the keys (`command`: Ctrl+Z
  and Ctrl+Y by the layout's letters, the press's control-code text, since a `KeyCode` names a US
  place; the other keys by code) and the view across (`view`: scrolled by characters, at most its
  width over the narrowest glyph shown, so every text drawn or measured costs no more than what the
  view can show). And `lumen_ui::Ui::text_area`: a `textfield` node of a given size whose text its
  app draws over it and edits (keys never edit it; `Ui::set_text` changes the node to publish but
  repaints nothing; a focus change repaints only its ring). The editor's node is a lossy view of its
  buffer: the first `TEXT_BYTES`, each control character (`\n` included) shown as U+FFFD; a
  `set_value` (at most `SET_VALUE_BYTES`) replaces the text as one edit. It draws by damage (a
  line changed at the cursor from just before it on, an undo's whole line, the rows below when lines
  come or go, a cursor move as one rect, each line once a repaint, only inside the rect repainted), and blinks its cursor on `run_until` only while its window has the focus (a
  fixed 500 ms period, solid through the tick after a key): unfocused, or with `--no-blink`, it has
  no period and the desktop settles.
- **What #204 adds (the image viewer, `apps/viewer`, bundled):** images
  built into the app (the wallpaper, `assets/wallpaper/lumen.png`; and
  `apps/viewer/images/`, made by `tools/viewer-images.py`: a palette image,
  a 16-bit grey one, `broken.png`, whose IDAT's CRC is wrong, and
  `costly.png`, a valid 8-bit RGBA PNG at full size, too costly to decode)
  decoded inside it by `lumen-png` (a path dependency of `apps/`, built for
  wasm32: `zune-png`, inflating with `zune-inflate`; `miniz_oxide` is only
  the encoder's; the module is under 400 KiB), so a bad file can only end the
  app. Manifest `time, desktop`: it runs on `run_until`, its one tick
  (`period()` set only while an image waits) the decode, so the request
  choosing an image is answered at once with `decoding <name>` (one
  arriving during a decode waits for it, about 350 slices for the
  wallpaper); the wait before that tick animates, so the desktop settles
  only once the image is shown.
  - **What it decodes:** `lumen_view::prepare`, its one call for an image:
    `admit` (from the file's length and the PNG's header, before anything
    is inflated: at most `MAX_FILE_BYTES`, 512 KiB; 1 × 1 to 1280 × 800;
    inflated rows of at most `MAX_INFLATED_BYTES`, 8-bit RGB at that size),
    `lumen-png`'s decoder, then the fitted copy. A refused or bad image:
    the reason shown in the view (wrapped to its width) and the `Info`
    label, one INFO line (`<name>: <why>`), and the app goes on; running
    out of memory, the memory plan failing, is a WARN instead.
  - **The window:** 640 × 480, the tree in a 200-pixel panel on the left
    (`with_area`: the `Images` list, `Previous`, `Next`, the `Fit`
    checkbox, the `Info` label: `<name> <w>x<h>`, `decoding <name>` or
    `<name>: <why>`), the view on the right, the app's own (one rectangle
    of damage when it changes, text drawn only where a line's box meets a
    repainted rectangle). Fitted (the start): the image's shape kept,
    nearest neighbour, centred, a copy scaled once. At 1:1: from its top
    left, the arrow keys panning (taken before the toolkit) in steps of
    40, clamped so the view stays filled; a smaller image centred.
    Choosing the image already chosen changes nothing; the list's
    selection follows a choice through `lumen_ui::Ui::set_selected` (new:
    a setter, like `set_text`, that leaves the focus where it is).
    `run viewer <name>` chooses the first image.
  - **Memory:** fixed at `lumen_view::MEMORY_BYTES` (16 MiB, its
    `build.rs`). At its start the app checks it, its heap's start against
    the plan's stack and data, and the image `run` names: a failure is one
    ERROR and exit 1, before the window opens.
  - **Its logic** is `crates/lumen-view` (host-tested): the geometry, the
    pan, the fitted copy, `admit`, `prepare` (`Prepared`, or `Why`:
    refused, the decoder's error, out of memory), and the memory plan.
- **What #208 adds (X-ray):** `apps/xray` (bundled, `caps=time,desktop,trace`),
  its logic `crates/lumen-xray` (a path dependency, host-tested).
  - **Reading:** live, every half second (`run_until`, animating), each
    CPU below `trace_cpus` (at most 8) has both rings read on from the
    last read's `next`, up to `READS_PER_RING` (16) reads of
    `TRACE_EVENTS_PER_READ` (256) a ring a refresh: 4096 events, a whole
    ring (the kernel's `trace::EVENTS`), so it keeps up unless a ring fills
    faster (the kernel counts the rest missed). A read refused with
    `BadArgument` is tried once more from 0: refused again, the CPU isn't
    online (marked offline until a read is taken again); taken, the kernel
    refused the last `next` (a bug), and, like any other error, that ends
    the app with 1 after one ERROR naming the CPU, the ring and `since`.
  - **Backlogs:** a ring's first read (from 0: what it holds since boot),
    its first after Resume and its first after the CPU comes back online
    read what piled up meanwhile: only the newest `runq` and `switch` are
    taken from it (the run queue and the thread stay right), its events are
    left out of the counts and its missed numbers out of `missed`, and the
    column it is read in is a gap.
  - **What it shows:** per CPU, as `fixed_label`s: `cpu <n>` (`cpu <n>
    off` only while its reads are refused), `cpu <n> runq` (from its
    newest `runq` event; one for another CPU's queue is odd, not shown),
    `cpu <n> thread` (from its newest `switch`), `cpu <n> switches`, `cpu
    <n> irqs`, `cpu <n> locks` and `cpu <n> missed` (counted after its
    rings' first reads). A value not known yet (nothing read, or the queue
    and thread of a CPU gone offline) is `-`. The lanes: `COLUMNS` (50)
    columns a CPU, a column a refresh swept over the oldest, the column
    written next a line (the sweep's front): the run queue's longest length
    in a column as a bar, its interrupts, locks that waited and switches as
    marks; a gap or an offline column is blank. A refresh's damage is two
    columns' strips (the column written and the sweep's front): only a
    refresh writes a column, and the drawing reads only the columns and the
    cursor.
  - **Odd events:** an event of the kinds X-ray reads (`switch`, `runq`,
    `irq`, `lock`) that doesn't decode, a kind in the other ring than
    `lumen_trace::kind::stream_of` says, or one the X-ray ring has no
    number for, is counted odd; the first time any CPU's count rises past
    0, one WARN says so.
  - **Pause and Resume:** paused, nothing is read (no period: the window
    settles); resumed, it reads on from where it stopped, what piled up a
    backlog.
  - **`--replay`:** `lumen_xray::replay`'s fixed trace (three CPUs, the
    middle one read for ten columns and then refused), read once through
    the same `View`: frozen, its buttons disabled, for `test_xray_golden`.
    It is generated by the crate, not recorded from a run, so the golden
    can't drift with the kernel's timing.
  - **Without `trace`** (`run --without trace xray`): live X-ray starts,
    logs `ERROR app: xray: no trace grant: NotGranted` and exits with 1,
    opening nothing. Arguments other than `--replay` log its usage as one
    ERROR and exit with 2.
  - **The scheduler scenario** needs a run queue that stays long while
    the harness reads it: `trace runq spin <n>` (section 14) keeps `n`
    threads spinning on the last CPU, X-ray (the first app) running on CPU
    0, and the label and the scheduler's count are polled until both say
    `n - 1`.
- **The launcher** (#157's taskbar) lists the bundled apps whose
  manifest lists `desktop`, never the test apps, after its four demo
  items, by name, and starts one through `run`'s path without waiting
  (`runtime::start`; `INFO  taskbar: <name>: app <id>`, a refusal
  `Then::Failed`, a load failure the app's one WARN). A bundled app
  `precheck` refuses is left out with a WARN naming why
  (`app::tests::every_bundled_app_passes_precheck` keeps that from
  shipping). Its Life item starts the Life app (#202: `taskbar.rs`'s
  `LIFE_APP`), which is therefore left out of the apps after the demo
  items, so it is listed once
  (`taskbar::the_launcher_lists_the_life_app_once`). An item starting an
  app is answered once the app is started, without waiting for its
  window, which the app's thread opens (so its button may come before
  the answer or after); `app <id>` is in the taskbar's INFO line, not
  the answer. An app that then can't open its window ends with its own
  ERROR and end line (as every bundled app does, #206), not with the
  item's `Then::Failed`, which only a refused start gives.
- **Loading from disk (M7+)** will need more: import names in errors are
  sanitised already, but wasmi reserves vectors up front for the counts a
  module declares (`reserve_exact`), which `CODE_FACTOR` doesn't bound
  for an untrusted module.

#### Shell commands

- `apps`, one record per listed app, by id:

  | Column | Meaning |
  |---|---|
  | `id`, `name` | the app |
  | `state` | `loading`, `running`, `waiting`, `ending` or `ended` |
  | `thread` | its thread's id (as in `threads`) |
  | `fuel` | fuel used so far |
  | `slices` | slice ends so far |
  | `memory` | its linear memory in KiB |
  | `windows` | 0 or 1 |

  No apps prints the header alone.
- `run [--without <grant>] <app> [args...]`; `run` alone prints its usage
  and the bundled apps' names.
- `kill <id>`.

## 9. Storage: POSIX VFS, writable `ext4` and snapshots (VFS, block/page cache, and `ext4` designed in #246, M7; OverlayFS/CoW snapshots planned, M11)

Milestone M7 introduces real persistent storage and the POSIX filesystem stack
needed to boot an unmodified Alpine Linux `x86_64` (`musl`) `ext4` rootfs and
execute dynamically linked binaries via `/lib/ld-musl-x86_64.so.1`. Pure logic
lives in two host-testable `no_std + alloc` crates (`crates/lumen-bcache` and
`crates/lumen-vfs`); hardware, frame-backed page caching, and syscall dispatch
live in `kernel/src/blk/`, `kernel/src/vfs/`, and `kernel/src/linux/`.

### Block cache (`crates/lumen-bcache`, host-tested)

`BlockCache<B: BlockDevice>` sits directly above `BlockDevice` (section 7) and
serves 4 KiB blocks (`BLOCK_SIZE = 4096`, 8 × 512-byte sectors) to filesystem
drivers (`ext4` superblock, group descriptors, bitmaps, inode tables, directory
blocks, extent trees, and file data writeback):

- **Validation, addressing, and sub-block reads:** `BlockCache::new` validates
  `sector_size` (512 or power-of-2 `<= 4096`), `sector_count > 0`, and
  `capacity > 0`. Keyed by 4 KiB `block_id: u64` (`lba = block_id *
  sectors_per_block`); partial tail blocks near device end clamp **both**
  `read_sectors` and `write_sectors` writeback to `valid_sectors * sector_size`.
  Byte-range `read_at(byte_off, dst)` and `write_at(byte_off, src)` return
  `Ok(())` on `len == 0` without calling `BlockDevice`, validate
  `byte_off.checked_add(len) <= sector_count * sector_size` **before** mutating
  any block in multi-block writes, and support 1 KiB, 2 KiB, and 4 KiB `ext4`
  block sizes cleanly.
- **Bounded LRU eviction and RAII pinning:** fixed entry budget (default 1024
  blocks = 4 MiB in M7, configurable for host tests and M10 memory-pressure
  reclaim). Callers pin blocks via an RAII `PinnedBlock` guard (so pins cannot
  leak on `?`); **pinned blocks are never evicted**, and if all slots are
  pinned, allocation returns `BlockError::CacheFull`. Evicting a dirty unpinned
  block writes it back to `BlockDevice` first (setting `needs_device_flush =
  true`); if writeback fails, the block remains dirty and cached and the error
  propagates to the caller (never silently discarding dirty data).
- **Read-only guard:** writes to a cache wrapping a `read_only` `BlockDevice`
  return `BlockError::ReadOnly` immediately before modifying or dirtying any
  cache entry.
- **Writeback ordering and durable flush barriers (`needs_device_flush`):**
  `BlockCache` maintains `needs_device_flush: bool`, set to `true` on **every**
  successful `write_sectors` (both LRU dirty eviction and explicit writeback)
  and cleared **only** when `BlockDevice::flush()` (`VIRTIO_BLK_T_FLUSH`)
  returns `Ok(())`. `sync()` and `flush_blocks(ids)` write dirty blocks in
  ascending `block_id` order (clearing each entry's dirty bit only after its
  `write_sectors` succeeds) and then invoke `BlockDevice::flush()` whenever
  `needs_device_flush` is `true`—even if 0 cached entries are currently marked
  dirty (e.g., after prior LRU dirty evictions or a retried failed flush). On
  any write or flush error, `needs_device_flush` stays `true` and the error
  propagates.
- **Where a device write error surfaces (#331):** `write_at` only copies
  into a cached block and marks it dirty, so its `Ok` means "cached", not
  "on the device". The device's error comes back later, from whichever call
  writes the block back: `sync` or `flush_blocks`, or the `read_at`,
  `write_at` or pin whose miss needed the block's slot (a read can thus fail
  with another block's write error). Besides its `ReadOnly` and
  `OutOfRange` refusals, a `write_at` fails at once only on a miss (that
  eviction failing, a failed pre-read of a partial block, or every slot
  pinned) or on a block pinned at that moment; the bytes for
  that block and any after it are then cached nowhere, while a write
  spanning blocks keeps its earlier blocks' bytes, dirty (no kernel caller
  spans blocks). No failed device write is ever reported as success or
  dropped: the block stays dirty and cached and the next writeback tries it
  again (`every_failed_device_write_reaches_a_caller_and_keeps_its_block_dirty`).
  Two consequences of how blocks are picked for writeback:
  - `sync` and `flush_blocks` go in ascending order and stop at the first
    block that fails, so the dirty blocks after it stay unwritten and the
    device unflushed, and a block that keeps failing keeps every higher one
    from being synced (an eviction may still write one back). A file's
    `fsync` is a whole-cache sync, so it can report another block's error.
  - The eviction victim is always the least recently used unpinned block,
    so while its writeback keeps failing, every miss that needs an eviction
    fails, reads included, even with a clean block that could make room
    (`a_failing_dirty_lru_victim_fails_every_miss_even_with_a_clean_slot`).
- **Stopping writeback (#349):** `BlockCache::stop_writeback` makes the
  cache write nothing more to its device and flush nothing, while it still
  takes writes: `sync` and `flush_blocks` return `BlockError::ReadOnly`
  without writing a block or flushing, and eviction takes only clean
  unpinned slots, so a dirty block stays cached, still served to reads,
  until reboot; a `read_at` miss while every unpinned slot is dirty is read
  straight from the device into the caller's buffer, uncached (the block
  isn't cached, so the device holds it), while a `write_at` or pin miss
  returns `CacheFull`. Under `stop_writeback_on_failure`, a miss whose
  eviction met the failure looks again the same way, so a read that met
  it is served (the failure kept in `first_failure`).
  `stop_writeback_on_failure` (off by default, when the failed block stays
  dirty for the next writeback, above) makes the first failed device write
  or flush stop writeback inside the call that met it, so nothing else
  reaches the device after it. `freeze` also refuses writes into the cache
  (`write_at` and `pin_mut` return `ReadOnly`). `thaw` (and
  `stop_writeback` by hand) exist only with the `test-util` feature, which
  the kernel's ktest build turns on: for the ktests that read back what
  such a cache holds; the kernel can't thaw. The state is one `Writeback`
  (`Running`, `Stopped`, `Frozen`), so frozen implies stopped.
  `first_failure` keeps the first failed device write's or flush's error.
  `writeback_failures` counts the device's failed block writebacks (an
  eviction's or a sync's) and flushes (what was written may not be
  durable), and `first_failure` is set at the first of them: the one
  signal that a device write failed, since a call's error alone can't
  say. `ext4`'s `errors=remount-ro` (below) turns
  `stop_writeback_on_failure` on at mount, watches `first_failure`, and
  freezes the cache when the last write-type operation running at the
  failure ends (`LatchableCache`'s methods).
- **Host tests (`cargo:lumen-bcache`):** verified against an in-memory
  `MemBlockDevice` with LRU eviction order, dirty writeback, RAII pin refusal
  (`CacheFull`), read-only rejection, partial tail block clamps, crash-before-
  flush vs crash-after-sync (including LRU-evicted dirty blocks + `sync()`),
  and injected I/O error recovery.

### Unified file `PageCache` (`kernel/src/vfs/page_cache.rs`)

To keep regular-file `read`/`write`/`pread64`/`pwrite64` and file-backed
`MAP_SHARED` / `MAP_PRIVATE` `mmap` **100% coherent without double buffering**,
every regular file (on both `ext4` and `tmpfs`) routes its data through a
per-inode `PageCache` holding 4 KiB `Arc<CachePage>` entries keyed by
`page_index: u64` (`file_offset / 4096`):

- **Zero-copy physical frames & write-back policy:** each `CachePage` owns one
  physical `OwnedFrame` (mapped RW through the HHDM for O(1) kernel
  `read`/`write`, and mappable directly into user `AddressSpace` PTEs with
  `PTE_SHARED = 1` as specified in section 5). `CachePage::drop` frees its
  `OwnedFrame` without taking any sleeping lock. Regular-file `write()` /
  `pwrite64()` are **write-back** into `PageCache`, updating `VfsInode`'s
  authoritative in-memory `st_size`/`mtime`/`ctime` (overlaid on `stat*`), with
  explicit writeback on `fsync`/`fdatasync`/`msync`/`sync` or global budget
  reclaim.
- **Dirty and mapping tracking (race-free clear):** each `CachePage` carries an
  atomic `dirty: AtomicBool` and `writable_maps: AtomicU32` (exact count of
  `PRESENT | WRITABLE | PTE_SHARED` PTEs, section 5). Writeback treats a page
  as dirty whenever `writable_maps > 0 || dirty` (in that order: "Dirty
  test order" below). To avoid losing concurrent
  stores during writeback, flushing clears `dirty` (`swap(false, AcqRel)`)
  **before** writing the page bytes to `ext4` (clamping the final page to
  `min(4096, file_size - page_off)`), and restores `dirty = true` on I/O error.
- **Safe global budget and eviction:** M7 configures a soft reclaim target for
  total `ext4` `PageCache` frames (e.g., 32 MiB on a 256 MiB guest; allowing
  temporary overshoot if all cached pages are actively pinned in
  `AddressSpace::pinned_pages`; dynamic memory-pressure reclaim arrives in M10).
  Clean `ext4` pages are evictable **only** when `Arc::strong_count(&page) == 1`
  (not pinned in any `AddressSpace` PTE or in-flight I/O); dirty `ext4` pages
  exceeding the target are written back before eviction (never on a volume
  errored by `errors=remount-ro`, below: its dirty pages stay, unwritten,
  until reboot). Global reclaim runs on
  the calling thread **outside BOTH Lock 4 (`VfsInode`) and Lock 5 (`PageCache`)**
  (pre-reclaiming before acquiring Lock 4, or releasing Lock 5 and Lock 4,
  reclaiming victim inode B in strict global order `Lock 4(B) → Lock 5(B) →
  Lock 6 → 7 → 8`, and retrying—never inside `frame::alloc`, which holds an
  IRQ-off `SpinLock`, and never holding another inode's Lock 4 or Lock 5).
  Its candidates are every live inode of each `ext4` filesystem in
  `EXT4_FILESYSTEMS` (the root volume, which `vfs::init` registers, and the
  ktests' volumes), whether or not the VFS's inode cache holds it, copied by
  `Ext4FileSystem::snapshot_live` under that filesystem's Lock 6, which is
  released before any inode is reclaimed.
  `tmpfs` pages and `detached_unlinked` pages are never evicted until truncated
  or dropped on last close.
  Because `ext4` blocks are allocated only at writeback, M7 also keeps a hard
  cap of twice the soft target (64 MiB): an `ext4` `write` that finds the cache
  at the cap (reclaim, run once before the write takes its locks, couldn't get
  below it, or this one write added more pages than the gap) returns a short
  count or `-ENOSPC` instead of growing the cache until the kernel runs out of
  frames, logged once per crossing. The cap is one check,
  `PageCache::admit_unreclaimable` (private to `page_cache.rs`), for a new
  page of a linked `ext4` file that reclaim can't drop: `insert_new` asks
  it for a page that is dirty when inserted, whichever path made it
  (`write_from` also asks before making the page, and the insert asks again
  in case the cache filled meanwhile), and `map_page` asks it before making
  a page a mapping will pin (a disk read) and again after it (the page is
  clean, so the insert lets it in); a clean page (a read's, which reclaim
  can drop) is let in at the cap. A write or truncate past the
  volume's largest file size returns `-EFBIG`. `create`/`mkdir`/`symlink`
  return `-ENOSPC` while fewer than 8 blocks are free, so a new entry can
  always be linked (or its inode released) without the directory growing
  into a full disk; `mkdir` in a directory at ext4's link limit (65000) is
  refused the same way before anything is allocated (`VfsError` has no
  `EMLINK`). A write failing inside `ext4plus` between allocating a block and
  recording it in the inode leaks that block (its free is crate-private):
  `e2fsck` reclaims it.
- **Shared page I/O (#264, #265):** `tmpfs` and `ext4` move a file's bytes
  through the same four `PageCache` methods, under the inode's Lock 4 and
  Lock 5: `read_into(offset, buf, file_size, miss)` (a page not cached is
  `miss`'s: a page read from disk, inserted after the copy, or `None`, a
  hole read as zeros without a frame; an error past the first page is a
  short count, the bytes before it, as Linux's `filemap_read` gives, #321),
  `write_from(offset, buf, old_size, fill)` (a page not cached is
  `fill`'s, a zero page or one holding the disk's bytes a partial write
  keeps, through the same `Ext4Inode::load_page` as a read, which logs a
  failed disk read, first page or not; a write starting past
  `old_size` first zeroes the old tail, the rule below; an error past the
  first page is a short count), `resize(old_size, new_size)` (a
  truncate's: a shrink's pages dropped, then the tail zeroed, the rule
  below) and `map_page(index, file_size, make)` (a mapping's page: the
  cached one, or `make`'s, a zero page or one read from disk, inserted
  past the hard cap's two checks; below). Pages are marked dirty by the
  cache's owner, a linked `ext4` file's alone (`writes_back`): a write
  never dirties a detached file's page (a writable shared mapping still
  may), and nothing writes one back. Their page arithmetic is
  `lumen_vfs::page`'s, host-tested: `spans` (each page a byte range
  touches, its offset there and its part of the buffer), `pages_in` (how
  many pages a file of that size has; a shrink drops the cached pages from
  there on), `valid_len` (a page's bytes below the file's size) and
  `resize_tail` (which bytes past the end a size change zeroes).
- **Truncate & extend interaction:**
  - Shrinking a file to `new_size` (`ftruncate`, `truncate`, `openat(O_TRUNC)`),
    in this order:
    1. the inode's `truncate`, under Locks 4 and 5 (`ext4` first cuts the
       file on disk and zeroes its disk tail): `PageCache::resize` drops
       all cached pages with `page_index >= new_size.div_ceil(4096)`, then,
       **only when `new_size % 4096 != 0`**, zeroes
       `[new_size % 4096 .. 4096)` of `page_index = new_size / 4096` if it
       is cached, marking it dirty for a linked `ext4` file
       (`writes_back`); then the size is set;
    2. once it has returned, `user_vm::invalidate_file_mappings` snapshots
       the inode's `Mappers` list and calls `AddressSpace::invalidate_file_from`
       under each registered space's Lock 1, unmapping all file-VMA PTEs
       covering `file_off >= align_up_page(new_size)` with cross-CPU TLB
       shootdowns (section 5, B7). The truncate syscall never holds Lock 1
       and Lock 4 together. Debug and kernel-side truncations (`vfstest
       truncate`, `create_truncated`, `open_kernel_file(O_TRUNC)`) skip
       invalidation; a stale read there is not a use-after-free because the
       pages stay pinned by their mappings.
  - Extending a file (`ftruncate` grow or `write`/`pwrite64` past `old_size`):
    if `old_size % 4096 != 0` and `page_index = old_size / 4096` is present in
    `PageCache`, zeroes `[old_size % 4096 .. 4096)` before exposing the
    new size so stale past-EOF `MAP_SHARED` stores never leak into extended
    regions, and marks that page dirty for a linked `ext4` file: a truncate
    at once, a write once it has written something (#321). A write refused
    outright leaves the size as it was and a clean tail page clean, its
    zeros past the end, so no writeback of the page can fail, and turn up
    as a later `fsync`'s `EIO`, for a write the caller saw fail. Writeback
    writes a page's bytes below the size alone (`valid_len`), so such a
    store never reaches the disk either.
- **Pages for `mmap` (built in #255's first part):** the file-backed VM
  takes a file's pages through one helper, `crate::vfs::mmap_page(inode:
  &Arc<dyn VfsInode>, index: u64) -> Result<MappedPage, VfsError>`.
  `MappedPage` has private fields (the page, its `index`, the file's size
  and the owning inode), `copy_to_frame` and `map_writable` (below), and
  getters: `valid_len()` (the page's bytes below the file's size), `page()`, the inode's
  own cached `Arc<CachePage>` at `index` (offset `index * 4096`), the same
  `Arc` `read_at`/`write_at` use (another call returns it again while it
  stays cached), and `file_size()`, read under the same Lock 4 hold.
  Holding the `Arc` pins the page (reclaim evicts only `strong_count == 1`).
  - *Filling:* both filesystems go through `PageCache::map_page`, which
    asks the fill for a missing page. A missing `ext4` page is read from
    disk by the same private fill as `read_at` and a write's partial
    pages (`Ext4Inode::load_page`, which logs a failed read naming the
    device, inode and page, once per streak of the same error: a mapping's
    fault turns the error into `SIGBUS`, and a read or write past its first
    page into a short count, neither of which says anything of the disk,
    #321).
    A hole in a `tmpfs` file (or a detached `ext4` one) gets a zero page
    (`CachePage::new_zeroed`, as `write_at`'s new pages).
    Unlike `read_at`, which reads a hole as zeros without a frame, a mapped
    hole costs a frame: the `tmpfs` or detached quota can refuse it
    (`NoSpc`). A missing page of a linked `ext4` file is refused (`NoSpc`,
    the hard-limit warning) while the `ext4` cache is at its hard limit, by
    the same check as a write's new page (`admit_unreclaimable`, before
    the disk read and after it): mapped pages are pinned, so reclaim can't
    keep them under it. A detached `ext4` file's pages count against the
    detached quota instead of the hard limit. Also `NoMem` and `Io`.
  - *EOF:* `Range` when `index * 4096 >= file_size` (or the product
    overflows): no page is mapped there, and the fault path raises
    `SIGBUS` (section 5). The page holding EOF is returned; its bytes past
    `file_size` are zero unless a mapping stored there.
  - *Truncate:* a shrink's `resize` drops the cache's own reference
    to pages past the new size and stops counting them, even if a mapping
    still holds one. The page stays alive, pinned by that mapping, until
    section 5's truncate invalidation (`invalidate_file_mappings`), run by
    the shrinking syscall once the truncate has returned, unmaps it from
    every registered address space mapping the file (section 5's rules: the
    syscall never holds Lock 1 and Lock 4 together).
  - *Other inodes:* `NoDev` for directories, symlinks, and every inode of
    another filesystem (`devfs`, `procfs`); `NoEnt` for an `ext4` inode
    unlinked and not open.
  - *Locking:* an `ext4` soft-limit reclaim first, with no lock held, then
    Lock 4 → Lock 5 (→ 6 → 7 → 8 to read the disk), as `read_at`. The
    caller may hold Lock 1 (`AddressSpace::inner`) or Lock 2, nothing
    numbered 3 or higher, and no `SpinLock`. It neither dirties the page nor
    counts a mapping: a writable fault does both through
    `MappedPage::map_writable` (below). `file_size` is a snapshot, which a
    truncate may make stale even while the fault holds Lock 1 (the truncate
    takes Lock 4 alone). A page mapped from a stale snapshot is still
    unmapped: the shrinking syscall's `invalidate_file_mappings` runs
    `invalidate_file_from` under each registered space's Lock 1 after the
    truncate, so it runs after that fault's mapping is in place (B7, section
    5). A private copy-on-write copy of the end page made from
    a stale snapshot can keep bytes past the new end (bytes a shared mapping
    stored past it).
  - *Reaching the concrete inode:* `VfsInode::as_any(&self) ->
    Option<&dyn Any>`, a default trait method returning `None`, overridden
    by `Ext4Inode` and `TmpfsInode` to return themselves. Its users are
    `mmap_page` and `mappable_parts` (behind `mappable`, `page_cache_lock`,
    `page_cache_mappers` and `MappedPage::copy_to_frame`), which downcast to
    either inode for its page cache and `Mappers` list, and, since #324,
    `tmpfs::quota_use`, which downcasts a mount's root inode to
    `TmpfsInode` alone for the shell's `mount`. Another cached filesystem
    that overrides it also needs its own branch in `mmap_page` and
    `mappable_parts`. For the page cache, a kernel-side registry was the
    alternative, but `tmpfs` has none, an unlinked (detached) file isn't
    reachable by walking a tree, and the `ext4` one costs a scan per
    fault.
  - *Bytes and state:* `CachePage::read_bytes`, `write_bytes`,
    `zero_range`, `copy_to_frame`, `swap_dirty`, `touch`, `last_access` and
    `new_zeroed` are `pub(super)` (inside `vfs` only); the byte accessors
    need the page's Lock 5 held (or the page not published yet), and `mm`
    can't name them. A copy-on-write split copies with
    `MappedPage::copy_to_frame(&mut OwnedFrame)`: the bytes below the file's
    size (`valid_len()`, from the page's own `index` and the size read with
    it, so no caller passes a length), zeros after. `MappedPage` keeps its owning inode (checked to be
    `ext4` or `tmpfs` by `mmap_page`), and the copy holds that inode's
    Lock 5, as the kernel's writers of the page do, so no kernel write
    races it (unsynchronised, that would be a data race, undefined in
    Rust). A store from ring 3 through a `MAP_SHARED` mapping is a
    hardware write outside Rust's memory model; one racing the copy leaves
    a mix of old and new bytes, as on Linux. The VM core keeps a bare
    `Arc<CachePage>` per mapped page (`AddressSpace::pinned_pages`) and
    looks the page up again with `mmap_page` when it needs a copy, under
    Lock 1: a snapshot kept from the first fault would carry a stale size
    (a file grown since would have its new bytes zeroed in the copy), and
    the new lookup also finds a page the cache has let go.
  - *Counting a writable mapping (built in #255's second part):*
    `MappedPage::map_writable() -> WritableMap` marks the page dirty and
    raises `writable_maps` together; `WritableMap`, an RAII guard holding
    the `Arc<CachePage>`, lowers the count when dropped
    (`dec_writable_maps`, which marks the page dirty first: below).
    `Drop` can't tell whether the PTE is gone and the TLB flushed, so the
    VM core keeps each guard beside its PTE (`AddressSpace::writable_maps`)
    and drops it only after clearing or downgrading the PTE and flushing
    (section 5's unmap order). `WritableMap::note_pte_dirty` marks the page
    dirty from a PTE's hardware `DIRTY` bit, for a walk that downgrades or
    clears the PTE (`mprotect`, `munmap`, truncate invalidation,
    teardown). `set_dirty`, `inc_writable_maps` and `dec_writable_maps` are
    `pub(super)`: inside `vfs`, only the guard and the ktests count
    mappings. A `fork` copying a writable shared PTE would take a second
    guard of its own.
  - *Dirty test order:* `is_dirty` loads `writable_maps` (Acquire) before
    `dirty`. Reclaim (and `fsync`'s `collect_dirty`) holds Locks 4 and 5,
    neither of which an unmap's `dec_writable_maps` takes, and may be
    preempted between the two loads. Read the other way round, it could see `dirty` still clear, then
    the count already 0, and evict the page unwritten. The count's Acquire
    load synchronises with the decrement, so a count of 0 means the `dirty`
    set before the decrement is visible.
  - *The last writable unmap (#266):* `dec_writable_maps` sets `dirty`
    (before decrementing, so the page never looks clean in between). A
    store through a `MAP_SHARED` mapping sets nothing in the page, and a
    writeback while it is mapped clears `dirty`, so without this the page
    looked clean once unmapped and reclaim dropped the stores unwritten.
    The caller decrements only after the PTE is cleared or made read-only
    and the TLB flushed (section 5's unmap order), so no store can follow
    through a stale entry.

### Writable `ext4` driver (`ext4plus =0.1.0-rc.2`, pinned in #248)

Following section 2 (one new dependency crate per PR), M7 evaluated the three
pure-Rust `no_std + alloc` writable `ext4` candidates in priority order and
selected **`ext4plus = "=0.1.0-rc.2"`** (multi-licensed `MIT OR Apache-2.0`,
elected license **`MIT`**, `default-features = false, features = ["sync",
"multi-threaded"]`; transitive linked crates `crc` `3.4.0` -> `crc-catalog`
`2.5.0`, `spin` `0.10.1` -> `lock_api` `0.4.14` -> `scopeguard` `1.2.0`,
`bitflags` `2.13.2`, and `async-lock` `3.5.0` -> `event-listener` `5.4.2` /
`event-listener-strategy` `0.5.4` / `pin-project-lite` `0.2.17`, plus build-time
proc macros `async-trait` `0.1.92` and `maybe-async` `0.2.11`). Both lower-priority
candidates failed evaluation: **`another_ext4`** is not published on crates.io
(cannot be pinned under section 2), and **`ext4_rs` `1.3.3`** requires nightly
`#![feature(error_in_core)]` (failing Gate 1). C FFI `lwext4` was not needed, so
CI and macOS stay free of a C cross-compiler and `make audit` stays clean.

**Vendored since #313.** Lumen builds `vendor/ext4plus`, the 0.1.0-rc.2
crate file unpacked (the first commit adding it changes nothing in it),
through `[patch.crates-io]` in the workspace `Cargo.toml`; the dependency
line still says `=0.1.0-rc.2`. Lumen's changes sit on top, each its own
commit, its code changes marked `Lumen (#n)` (the crate's own tests only
adjusted to new signatures), so `git log vendor/` lists them and a diff
against the crate file shows them all. So far one:
#313, the htree hash's signedness from `s_flags` (the #308 caveat below;
`Superblock::htree_hash_unsigned`, and `htree_hash`, public only for
`lumen-vfs`'s host test). The crate's own unit tests don't build from the
crate file (their `test_data/` isn't in it), so Lumen tests its changes
from outside. Its other known bugs (the allocator retry of #350, #355) are
fixable there now, each in a PR of its own.

`ext4plus` `=0.1.0-rc.2` is vetted against all six hard gates:
1. Compiles on pinned stable `rustc 1.99.1` for `x86_64-unknown-none` with
   `default-features = false, features = ["sync", "multi-threaded"]` and zero
   nightly features (`#![no_std]`, `#![forbid(unsafe_code)]`).
2. Passes `make audit` (no SSE/AVX, no red zone, kernel code model) when linked
   into both `lumen-kernel` and `lumen-kernel-ktest` (`kernel/src/vfs/mod.rs`,
   `ktest:lumen_kernel::vfs::tests::ext4_crate_smoke`).
3. Dual-licensed `MIT OR Apache-2.0` with elected license **`MIT`** recorded in
   section 2.
4. Mounts and mutates images built with section 15's pinned `mke2fs` flags
   (**`-t ext4 -b 4096 -I 256 -O ^orphan_file,^metadata_csum_seed,^metadata_csum,^has_journal`**):
   extents (including `depth >= 1` extent trees), 256-byte inodes, linear and
   htree directories, fast & block symlinks, hard links with accurate
   `i_links_count`, `chmod`/`chown`/`utimensat`, and
   create/write/truncate/unlink/rename/mkdir/rmdir, leaving mutated images
   passing **`e2fsck -fn`** on the host.
5. **No internal write cache:** `ext4plus` holds no internal dirty block cache—every
   mutation (`write_to_block`, `Inode::write`, `Superblock::write`,
   `BlockGroupDescriptor::write`) writes synchronously through
   `Ext4Write::write`. Routing all disk I/O through Lumen's `BlockCache` adapter
   satisfies Gate 5 without needing a pre-flush of an internal crate cache
   before `BlockCache::sync()` issues `BlockDevice::flush()`.
6. **Unlink-while-open contract (covering `unlink`, `rmdir`, and `rename` overwrite):**
   when `unlink(path)` or `rename`/`renameat2` removes the last link of a file
   (`st_nlink == 1`), **or `rmdir(path)` / `rename` overwrites an empty
   directory** (`st_nlink == 2`), while the target inode's explicit atomic
   `open_count > 0` (sum of live `OpenFile`, `AddressSpace::mapped_files`, and
   `FsState`'s `cwd` references (M7's `ProcessState::cwd`), maintained via
   RAII guards and distinct from
   `Arc<VfsInode>` cache refs):
   - The directory entry is removed/replaced immediately (so `open(path)`/
     `stat(path)` no longer find the old inode and a new file/dir can be
     created at `path`).
   - `fstat(fd)` reports `st_nlink == 0`, regular-file `read`/`write`/
     `ftruncate`/`mmap` continue to work until `open_count` reaches 0, and
     detached directories return `-ENOENT` on `lookup`/`create`/`mkdir`/`link`/
     `symlink` and empty EOF on `getdents64` (while `getcwd` returns `-ENOENT`).
   - `fstat(fd)`'s `st_ino` changes when the inode is detached, on ext4 and
     tmpfs alike: both give it a synthetic number with bit 63 set
     (`lumen_vfs::next_detached_ino`, ext4's reasons in step 2 of its list
     below), never a number a new inode could take. Linux keeps the number
     (a deliberate difference); `m7_vfs`'s `detached_ino` (#254) checks
     Lumen's: a new number, bit 63 set.
   - **An open racing the unlink (#262):** an open raises `open_count` (an
     `OpenGuard`) *before* its last check that the inode is live, and Gate 6
     reads `open_count` and then frees or detaches the inode within one hold
     of the target's Lock 4, the lock `stat()` takes. The check
     (`OpenGuard::try_new`: `stat()`, then `is_detached()`, in that order)
     fails with `-ENOENT` when `st_nlink == 0` and the inode isn't detached,
     giving the reference back. So an unlink racing an open either sees the
     reference and detaches (the new descriptor reads the data) or freed the
     inode first (the open fails as if its lookup had lost the race); never
     a descriptor on a freed inode. `try_new` may sleep: through `stat()` it
     takes the inode's Lock 4 and, on ext4 and tmpfs, Lock 5; it is never
     called holding that Lock 4 (or another inode's Lock 4 out of `lock_id`
     order), any of Locks 5 to 9, or a spinlock.
     - An `O_CREAT` open that creates the file takes its reference right
       after `create`, inside `with_rename_lock`, with no check: on a
       filesystem with a rename lock (ext4, tmpfs; the trait's default takes
       none) no unlink can run before it, so that open never fails this way.
     - An `O_CREAT` open that finds a file (an existing name, or a followed
       symlink's target; a directory fails with `-EISDIR` first) takes its
       reference after Lock 3, so an unlink can still free the file first;
       it then opens again (POSIX: the old file or a new one, never
       `-ENOENT`). Only that freed case is retried (a `stat()` error passes
       through), and after `MAX_CREAT_RACE_RETRIES` (64) more lost races
       the open fails with `-EEXIST`, the same error as the create loop's
       own limit when the name keeps being taken.
     - Every filesystem with Gate 6 keeps both halves: the count read and
       the free or detach under the lock `stat()` takes, and `st_nlink == 0`
       reported for a freed inode.
   - **`ext4plus` has no native `s_last_orphan` orphan list:** `Dir::unlink`
     immediately zeroes and frees the inode and blocks once `links_count == 0`.
     Therefore VFS (#253) always uses the `detached_unlinked = true`
     `PageCache` fallback—before mutating `ext4`:
     1. For regular files, check a separate non-evictable detached-memory quota
        (sharing/mirroring `tmpfs`'s 32 MiB cap; never triggering `ext4`
        `PageCache` reclaim while holding Lock 3/4) and copy the on-disk
        pages that aren't all zero into the inode's `PageCache` (aborting
        `unlink`/`rename` before touching `ext4` with `-ENOSPC` when the
        detached quota is used up, `-ENOMEM` when a frame can't be
        allocated). (Empty directories have 0 pages to fault.) The scan for
        those pages reads the disk inode once, stops once the non-zero
        blocks found cover the inode's allocated blocks, and gives up with
        `-ENOSPC` after 128 MiB of uncached range: unlinking an open file
        whose allocated blocks aren't all covered by non-zero data within
        that range (data lying further out in a sparse file, a long run of
        zero-filled blocks, extent or indirect blocks counted in `i_blocks`)
        is refused rather than stalling every namespace operation.
     2. The VFS removes `(dev_id, old_ino)` from `InodeCache` before the
        backend `unlink`/`rmdir`/`rename` (putting it back if that call
        fails), and only after the backend call has succeeded (an abort
        leaves the inode untouched) does the adapter assign the live
        `VfsInode`'s `st_ino` a
        synthetic non-colliding value (`(1u64 << 63) | detached_seq`; Lock 4
        ordering uses an **immutable** creation-time `lock_id: u64`, never the
        mutable `st_ino`, and `/proc/self/maps` reads `inode.ino()` dynamically)
        so on-disk `old_ino` reuse by `ext4` can never alias `InodeCache` or
        break `ld-musl` `(st_dev, st_ino)` deduplication.
     3. Mark `detached_unlinked = true` (`st_nlink = 0`): `PageCache` treats
        its pages as non-evictable (like `tmpfs`), unfaulted hole pages read as
        zero, and `fsync`/`fdatasync`/`msync`/`fchmod`/`fchown`/`utimensat`/
        `ftruncate`/`write` operate strictly in-memory (`fsync`/`fdatasync`
        returning `0` as a no-op) without ever calling back into `ext4` with
        the freed inode number. On-disk `e2fsck -fn` stays 100% clean even
        across abrupt power loss, and all `CachePage`s free when the last
        `Arc<VfsInode>` drops!

- **Adapter invariants required in #253 (`kernel/src/vfs/ext4.rs`):**
  - **Symlink 60-byte bug & inline-symlink readlink/unlink workarounds:**
    `Ext4::symlink` (and Linux/`mke2fs`) stores inline symlink targets `<= 60`
    bytes in `i_block` (`!EXTENTS && i_blocks == 0 && i_size <= 60`) while
    `Inode::symlink_target` enforces `MAX_INLINE_SYMLINK_LEN = 59` (falling back
    to `read_inode_file` -> `BlockMap` on 60-byte inline symlinks). #253 MUST
    route `target.len() <= 59` to `Ext4::symlink` and `target.len() >= 60` to
    block symlinks via `create_inode(FileType::Symlink)` + `write_at` +
    `Dir::link`, AND handle `!EXTENTS && i_blocks == 0 && i_size <= 60`
    directly in `readlink` so pre-existing 60-byte `mke2fs`/Linux inline
    symlinks in the Alpine minirootfs read cleanly. In addition, when unlinking
    the last link (`links_count == 1`) of any inline symlink (`!EXTENTS &&
    i_blocks == 0 && i_size <= 60`), `Ext4::delete_file` calls
    `BlockMap::free_all()` on `i_block` and panics interpreting ASCII target
    bytes as block numbers; because `Inode::set_inline_data` is `pub(crate)` in
    `ext4plus`, #253 MUST zero `i_block` (`0x28..0x64`) and `i_size` on disk via
    `BlockCache` (and re-read `Inode`) before `Dir::unlink` on inline symlinks.
    The same applies to character and block device nodes (which Linux and
    `mke2fs -d` create): they keep `rdev` in `i_block` with no blocks
    allocated, so the adapter zeroes it the same way before freeing them.
  - **Device numbers and timestamps encoded and decoded by the adapter
    (#292, #294):** `ext4plus` exposes neither a device node's `i_block` nor
    signed times (it reads a time's low 32 bits as unsigned, so 1969 reads as
    2106).
    - A character or block device's `i_block[0..2]` is read raw when the inode
      is first loaded and decoded as `ext4_iget` does
      (`lumen_vfs::ext4::rdev_from_i_block`: the old encoding in `i_block[0]`
      when non-zero, else the new one in `i_block[1]`), and kept for the
      inode's life.
    - Times go through `lumen_vfs::ext4::time_to_disk`/`time_from_disk`:
      Linux's signed second plus two epoch bits and nanoseconds (1901 to 2446).
    - Which times have their extra field follows `i_extra_isize` as `ext4plus`
      reads it (`i_ctime_extra` needs 8, `i_mtime_extra` 12, `i_atime_extra`
      16, as Linux's `EXT4_FITS_IN_INODE`), found once per load by asking a
      copy of the `ext4plus` inode. A deviation `ext4plus` causes: an
      `i_extra_isize` of 0 on a volume with large inodes means no extra
      fields here, where Linux's `__ext4_iget` takes it as 32 and writes 32
      back; mke2fs never writes 0 (`s_min_extra_isize` is 32), but `ext4plus`
      gives inodes it creates `min(32, s_min_extra_isize)`.
    - Every time is held in memory as the inode holds it, through
      `InodeTimes`, whose fields are private: clamped by every setter with
      `truncate_time` (Linux's `timestamp_truncate` for a range: seconds
      clamped, nanoseconds dropped if that changed the second, whole seconds
      without the extra field), and on load too, defensively (a no-op for
      anything `ext4plus` decodes), and written only through `store` and
      `store_ctime`. Linux decides the range per filesystem and grows an
      inode that lacks the fields (`ext4_try_to_expand_extra_isize`), or else
      stores the time clamped while keeping it untruncated in memory.
      Applying it per inode and per field, in memory, is Lumen's design,
      since `ext4plus` can't grow `i_extra_isize`. So `stat` before and after
      a flush or a remount agree.
    - `ext4plus`'s `create_inode` sets a new inode's atime, mtime, ctime and
      crtime before its `i_extra_isize`, so only the low 32 bits of the first
      three land and nothing of `i_crtime` (0x90: `ext4plus` places the
      fields by `128 + i_extra_isize`, which is still 0 then).
      `times::new_inode`, the adapter's one way to make an inode (`create`,
      `mkdir`, `symlink`), sets all four again before the inode is written
      and linked, `i_crtime_extra` too when the inode reaches it (#303), so
      the inode stores its creation time where Linux's `statx` reads it (a
      ktest checks the bytes; nothing reads them with Linux's tools). With
      `i_crtime` but no `i_crtime_extra`, the time is clamped to 32 bits as
      for the others (untested: the ktests' images give new inodes an
      `i_extra_isize` of 32, past both fields; the branch needs 20 to 23).
      Lumen itself reports no btime.
    - A corrupt extra word whose nanoseconds are 1e9 or more is carried into
      the seconds by `ext4plus`, which can decode a different year than
      Linux does (base `0x7fff_ffff`, extra `0xffff_fffc`: 1901 here, 2038
      there); nothing checks for it.
  - **Truncate partial-block tail zeroing:** `ext4plus::file::truncate` does
    not zero `[new_size % 4096 .. 4096)` in the partial final block on shrink;
    #253's `PageCache` / truncate path MUST enforce the tail-zeroing rule above.
  - **Fresh `Inode` reads:** `Dir` caches a copy of its `Inode`; always
    re-read `Inode::read(&ext4, ino)` under Lock 6 before mutating/unlinking.
  - **Explicit `rmdir` & cross-dir `rename` handling:** `ext4plus` lacks
    dedicated `rmdir` and `rename` helpers; #253 must set a child directory's
    `links_count` to `1` before `Dir::unlink` and decrement the parent's
    `links_count`, and on cross-dir directory `rename` must update `..` in
    block 0 and adjust both parents' `links_count`.
  - **Writable check:** `Superblock::read_only()` is `pub(crate)`; check both
    `IncompatibleFeatures::RECOVERY` and `ReadOnlyCompatibleFeatures`
    (`BTREE_DIR | GROUP_DESCRIPTOR_CHECKSUMS | QUOTA | PROJECT_QUOTAS |
    BIG_ALLOC | METADATA_CSUM | READONLY | ORPHAN_PRESENT`, plus any bit past
    `ORPHAN_PRESENT`, which `ext4plus` keeps with `from_bits_retain` and the
    adapter masks) alongside `VIRTIO_BLK_F_RO` rather than calling a crate
    helper. Any of them mounts the volume read-only: `METADATA_CSUM` because
    the adapter's raw `BlockCache` writes (`..` updates, inline-symlink
    zeroing, writing and restoring an inline target) never recompute
    checksums, `READONLY` because the volume says so, `ORPHAN_PRESENT`
    because `ext4plus` never processes the orphan file's entries. So does
    a volume whose directories may hash names unsigned without saying so
    (#308, the caveat below): the superblock's `s_flags` (offset 0x160 in
    the superblock) having neither `EXT2_FLAGS_UNSIGNED_HASH` (0x2) nor
    `EXT2_FLAGS_SIGNED_HASH` (0x1) on a volume with `dir_index` (one with
    the unsigned flag mounts writable since #313). A forced
    read-only mount is logged with its reason and its `ro_compat` (and its
    `s_flags` when the hash is the reason; a journal needing recovery,
    checked first, wins over it), unless the device itself is read-only.
  - **Write errors part-way through a namespace operation:** when a later
    `ext4` write fails, `unlink`, `rmdir`, `link` and `rename` (all three
    flavours) undo the disk changes they already made (re-linking entries,
    restoring `links_count` and saved inline-symlink bytes) and return the
    error; once the entry change itself has landed (the name is gone on
    disk), they finish the operation instead and return the later error.
    When an undo write of a `links_count` fails, the adapter keeps in-memory
    bounds so `stat` and later operations see the true link count (the
    volume is read-only by then, `errors=remount-ro` below, so nothing
    writes the corrected count; writeback never writes `links_count`).
    Bounds left on a volume still writable (an undo that couldn't read an
    inode back, a read fault erroring nothing) are written before the next
    operation changes that count (`store_bounded_links`,
    `ext4_a_kept_link_count_is_written_before_the_next_unlink`). The
    bounds are not persisted, so an undo that never lands leaves work for
    `e2fsck` after a reboot. The
    bounds are `lumen_vfs::nlink::NlinkBounds` (host-tested): one map per
    filesystem, a floor and maybe a ceiling per inode, the floor never above
    the ceiling, and `reconcile` pins a count unless the disk already holds
    it. An undo that knows which entries exist writes their count through
    `write_links_or_pin`, pinning it when the write fails. Where the entries
    can't be read back, only a floor is kept: by the exchange's and the
    move's undo after trying to raise the count on disk
    (`raise_links_or_floor`), and by link's and mkdir's undo of a failed
    `Dir::link` (`undo_failed_link`) without; `unlink` and `rmdir`, unable to read
    their directory back (or finding the name on another inode), pin the
    count they assume, unless the child reads back zeroed or with its
    blocks being freed (`delete_file` frees an inode's blocks, then zeroes
    it, only after its entry went): its entry is then gone and the inode or
    its blocks may be handed out again, so it is forgotten rather than kept
    live where a new inode could alias it or a create in it, or a write
    through an open descriptor (it is detached, its pages never written
    back), could reach a new file given its blocks (#330), and for `rmdir`
    the parent is handled as for a removed entry. Blocks being freed are
    told by the first block `delete_file` frees: that block's bit clear in
    the block bitmap, read through the block cache as the allocator reads
    it, the block found by parsing `i_block` (`lumen_vfs::ext4`'s
    `first_block_freed`, and `Geometry` for the bit, host-tested). That it
    is the first rests on `ext4plus` 0.1.0-rc.2 freeing data blocks in file
    order before map blocks, read from its source and exercised by the
    ktests (one extent, a depth-1 tree, a block map); a bump of `ext4plus`
    must check it again. Forgetting one for that is logged; the rest of its
    blocks, and its inode, may stay allocated, for `e2fsck`. A child whose
    inode, map or bitmap byte can't be read, whose map doesn't parse or
    takes more than 16 reads, or whose first block is outside the volume,
    and one that isn't the last link and reads back corrupt (`delete_file`
    runs only for a last link), is still kept live, as when it reads back
    allocated, though its blocks may then be free: one line says so, with
    the cause. The geometry this reads by (`Geometry`) is checked at mount:
    a superblock whose groups have more blocks than a bitmap block has
    bits, none, a first data block off the volume or a block over 64 KiB
    refuses the mount, naming the field and its value; on a bigalloc
    volume the bound is on its clusters per group, and no block's bit is
    placed (it mounts read-only, so no unlink reaches this). A move that can't read back whether its old entry
    (after the unlink failed) or its new one (after undoing the link
    failed) is still there keeps the counts with the source at both names:
    the source's count from before plus one (at least 3 for a directory)
    and, for a directory, the new parent's plus one. Counts too high only
    leak, for `e2fsck`; one too low frees an inode that the other name
    still points at (#322). If an entry did go, a file is left one link
    high; a directory whose new entry went is one high with its new parent;
    one whose old entry went has in fact moved, though the rename reports
    it didn't: its count is one high, its old parent keeps the `..` link it
    lost, and its `..` still names that parent. Such a raise drops a
    ceiling below it, whether it lands or not (`NlinkBounds::raised_to`).
    An exchange's undo that can't read its entries back likewise raises
    each child to its count at both names (its retries can leave one at
    both). One that can writes the count a child's names call for, except
    where that would be 0 (a child at neither name with no other link): it
    then keeps the child's count from before rather than writing 0 onto its
    allocated inode, which `e2fsck` would clear instead of reattaching in
    `lost+found` (#323). A directory move whose unlink fails after taking
    the old entry out has moved: it still fixes the old parent's count and,
    between parents, the child's `..`, and returns the unlink's error
    (#323). An `rmdir` whose parent can't be read back once the entry is
    gone pins the parent's count one below its count in memory, and the
    parent's times move whenever the entry went, whatever failed after
    (#325). Each kept bound is logged at `error` (once per change, naming
    the inode and its disk count), and so is a rename undo that leaves its
    entries inconsistent or unreadable (judged once, from the entries' end
    state, not per attempt), a move that can't tell whether its old entry
    went among them. A directory move whose fix-ups fail after its unlink
    failed past the entry (it moved; no undo runs) logs that, naming the
    directory, both parents and the error; a moved directory whose `..`
    rewrite fails (a move's or an exchange's) logs it too, since its `..`
    may still name the old parent and nothing in memory says so; and an
    `unlink` or `rmdir` that took a last entry out and then failed freeing
    the inode logs that it may be left half-freed. `create`, `mkdir` and `symlink`
    release the inode they allocated when linking it fails (e.g. `ENOSPC`
    growing the directory).
  - **Write errors an operation doesn't see (#331):** an operation's result
    doesn't say whether every write it made reached the device:
    - Every write goes into the block cache, which writes back (above): an
      operation whose blocks are only cached returns `Ok`, and the device's
      error comes from the next `FileSystem::sync` or a file's `fsync`,
      `fdatasync` or `msync(MS_SYNC)` (`EIO`, the blocks kept dirty in the
      cache, never written: since #349 that error makes the volume read-only,
      below: `ext4_a_failed_device_write_makes_the_volume_read_only`), or from the
      eviction that writes a block back, to whichever read or write missed
      then (a lookup or a page read can get another block's write error,
      unless `ext4plus` drops it, below). A sync stops at the first block
      that fails (above), so an `fsync` can report another file's block and
      leave its own unwritten.
    - `sync(2)` returns 0 whatever `vfs::sync_all` returns (Linux's
      behaviour), and global reclaim's page writeback has no caller: a
      failing streak is logged once per inode, its pages kept dirty. Once
      dirty pages can't be written back, the `ext4` page cache reaches its
      hard limit and other files' writes get `ENOSPC` (the page cache,
      above).
    - `ext4plus` 0.1.0-rc.2 frees an extent-mapped file's block range again
      block by block when the range free fails (`file_blocks/utils.rs`,
      `free_freed_ranges`), so a write that fails inside it leaves
      `ext4plus`'s call `Ok` (the operation returns `EIO` since #349, below:
      `WriteOp::end`; unlink and rmdir probed in the kernel, truncate on the
      host in #351's review; a rename's replaced target reaches the same
      code, read from the source). A block-mapped file's free
      (`block_map.rs`) has no retry and returns the error. `free_blocks` and
      `free_block` (`lib.rs`) raise each count in memory just before writing
      it and never put it back, so a failure at or after the descriptor
      write carrying the raised group count doubles the group's count, and
      a failed superblock write doubles the superblock's as well; a failed
      bitmap byte or bitmap-checksum write doubles nothing
      (`ext4_unlink_whose_failed_write_ext4plus_retried_returns_eio`,
      by the `FaultCache`). `e2fsck` recomputes the counts (read, not
      tested).
    - `ext4plus`'s `Extent::allocate` (`extent.rs`) retries any failed
      `alloc_contiguous_blocks(n)` with one block fewer and gives `NoSpace`
      at 0. It runs for every new block a write needs: page writeback
      (`fsync`, `fdatasync`, `msync`, `sync`, reclaim), long symlink
      targets, truncate's tail zeroing. `alloc_contiguous_blocks` writes the
      bitmap bits, then the descriptor's checksum, then lowers the group's
      count in memory and writes it, then the superblock's, and puts nothing
      back. On 4 KiB blocks a page needs 1 block, so nothing is retried: a
      failed allocation write comes back as `NoSpc`, not `Io`, and once the
      bit has landed it leaks (owned by no extent). If the checksum write
      failed, neither count was lowered, so both stay one above the bitmap
      for good, every later write storing them so; if the count's write
      failed, the superblock's stays one above for good (the group's is put
      right by its next write); a failed superblock write is put right by
      the next one. Since #349 those later writes land only in a block cache
      that never writes back (the volume read-only), so on the device the
      state the failure left stays
      (`ext4_a_failed_write_in_a_block_allocation_comes_back_as_nospc`). On
      a 1 KiB-block extent volume, where a page needs 4 blocks, a host probe
      in #351's review (not a test; 2 KiB reaches the same code, not probed)
      saw the shorter retry succeed: `ext4plus`'s write returns `Ok` (the
      writeback then `EIO`, the volume errored, #349), with up to 4
      bits leaked (a failed bitmap write after the first leaves the earlier
      bits and both counts above the bitmap for good), and a count lowered
      twice when its own write failed.
    - Under the real block cache only a miss can fail. The bitmap is read
      just before its bits are written, and the count's descriptor write
      follows the checksum's, so those are hits; the writes that can fail
      are the descriptor's first write in the operation (the checksum's)
      and the superblock's, under cache pressure. The free's retry needs
      the error to be transient (a persistent one fails the retry's own miss
      too), and then only the superblock write can double a count (both
      counts, then). The 4 KiB allocation has no retry, so a persistent
      error is enough, and its checksum write failing leaves the permanent
      over-count. The likeliest route, though, is the bitmap's own read
      missing and its eviction of another dirty block failing: the free's
      retry turns that into `ext4plus`'s `Ok` (the operation's `EIO` since
      #349), the allocation's into `NoSpc` (nothing
      doubled or leaked; the victim stays dirty).
    - `ext4plus`'s allocators (`alloc_block`, `alloc_contiguous_blocks`,
      `alloc_inode`) loop forever on a group whose count promises more than
      its bitmap has, or, for `alloc_contiguous_blocks(n)`, on one with no
      run of `n` free bits: the next allocation from such a group hangs
      (read from the source, and seen hanging a file's writeback on an
      image made so; #350). Any allocation whose checksum write fails after
      its bit landed leaves such a count, retry or not, whether it reports
      `Io` (`alloc_block` for a directory's or an index block,
      `alloc_inode` for create, mkdir, symlink) or `NoSpc` (through
      `Extent::allocate`); so does the free's double count.
    - Two narrow paths drop an error before any of these (read from the
      source, not probed): `ext4plus`'s duplicate-name check in
      `add_dir_entry` (`dir.rs`) takes a failed lookup (an eviction's write
      error) as "absent", so `Dir::link` can return `Ok`; and a rename's
      `take_out_target_entry`, when the target's `Dir::unlink` fails after
      its entry went, carries on as if it had worked, unlogged. Since #349
      the failed write errors the volume, so the link or rename returns
      `EIO` all the same (`WriteOp::end`).
    - After a first error, the adapter's undo writes (above) report nothing
      further: a failed one is pinned in memory or logged, and the operation
      returns the first error. Among them: `undo_failed_link`,
      `write_links_or_pin` in the unlink, rmdir, rename and exchange undos,
      the inline-symlink restore, `discard_orphan`, `put_back_target`,
      `rollback_link_entry` and truncate's tail restore.
    - A rename over an existing target, once the move landed, returns `Ok`
      whatever retiring the target does when the volume stays healthy: its
      lowered `links_count` (pinned in memory if the write fails), freeing
      it when that was its last link (logged), and a directory target's
      parent's count (pinned); a failed device write among them makes it
      `EIO` since #349 (`WriteOp::end`).

    So a write error can't be told from `ext4plus`'s results (an `Ok` or a
    `NoSpc` may follow one), nor from the block cache's: a failed `write_at`
    may be a failed pre-read or another block's eviction, a failed `sync` a
    failed flush or a pinned block, and an eviction's write error reaches a
    read as an ordinary `Io`. #349 reads one signal inside `BlockCache`
    instead, `first_failure`, set where a block's device write or a flush
    first fails (`ensure_slot`'s dirty victim, `writeback_and_flush`), which
    every route goes through. A failed flush counts: the writes before it
    may not be durable (jbd2 aborts its journal on one too).
  - **`errors=remount-ro` (#349):** Lumen's one policy for a write error,
    as Linux's for a root mounted `errors=remount-ro` (whose journal also
    aborts on a write error). `ext4plus` has no journal and writes each
    step of an operation as it goes, so a failure can leave any of a
    dozen partial states; rather than write on from one of them, the
    volume stops writing.
    - *Trigger:* the first device write or flush of the volume's that
      fails, told by one signal, the block cache's first failure
      (`LatchableCache::first_failure`, over `BlockCache::first_failure`,
      above), which `LatchedCache` (the seam the mount wraps the volume's
      cache in, which every call goes through; the volume holds it as an
      `Arc<LatchedCache>`, and the cache's controls, `freeze` and
      `stop_writeback_on_failure`, are on `LatchableCache`, a supertrait only
      the caches below the latch implement) checks after each call while
      healthy: a block written back by a `sync` (`fsync`, the filesystem's
      sync) or by the eviction of whichever `write_at` (`ext4plus`'s, for
      directory blocks, inodes, bitmaps, group descriptors, the superblock,
      a file's data and map at writeback; the adapter's raw ones, `..` and
      inline-symlink bytes) or `read_at` missed. A call's result alone
      decides nothing: a `write_at` can fail on a pre-read or a full cache
      with no write failing (it then errors the operation, not the volume),
      and `ext4plus` can return `ENOSPC` (its allocation retry, which the
      operation returns as its own error) or `Ok` (its range-free retry,
      above; the operation then returns `EIO`, `WriteOp::end`) after one
      did. A data block's
      failed write counts too: the cache can't tell data from metadata, and
      a file's writeback allocates blocks and writes its map in the same
      call. Read errors, `NoSpc` and `NoMem` that no failed write caused,
      and refusals made before anything is written (`EFBIG`, the page
      cache's hard cap), don't trigger it. In the ktests the `FaultCache`
      reports each write or sync it fails as the device's failure.
    - *Nothing more reaches the device, from the failure on:* the block
      cache stops writing back inside the call that met it
      (`stop_writeback_on_failure`, above): no later eviction, sync or
      flush writes, during the rest of that operation or after it. Syncs
      and writebacks also stop there (`FileSystem::sync` across inodes,
      `fsync`'s and reclaim's page loops): the pages left stay dirty in
      memory, never written into the cache either, whose slots reads need.
    - *Read-only from the failed write on* until reboot (nothing clears
      it): `FileSystem::read_only()` and `errored()` say so at once, so a
      write-type operation starting after the failure is refused (`fsync`
      too: `EIO`; `writable_op` checks again once the operation is counted,
      so one starting as the failure comes is either refused or counted).
      Those running at the failure finish against the cache and memory:
      their later writes and the failing one's undo (above) go into the
      block cache, never to the device. Each holds a `WriteOp`; an operation
      that began on a healthy volume and ends on an errored one returns
      `EIO` whatever it returned itself (`WriteOp::end`; its own error, if
      any, stands): what it did went into a cache that never writes back,
      so it didn't happen on the disk, though until reboot it may show in
      memory (so lumen-vfs drops the names an unlink, rmdir or rename
      touched from the dentry cache on a failure too, and `truncate`
      unmaps the caller's pages past the size the file has then on a
      failure too, never below it; an
      `open(O_TRUNC)` that fails so doesn't). A read whose eviction met the failure is served (below), so
      the operation may not see the failure itself (a `link` would return
      `Ok`). When the last one ends, the latch settles (below). The VFS
      refuses every write-type path with
      `EROFS` (create, `mkdir`, `symlink`, `link`, `unlink`, `rmdir`,
      `rename`, an open for writing or truncating,
      `chmod`/`chown`/`utimensat`), and the adapter's own checks refuse a
      `write`, `truncate` and attribute change through a descriptor opened
      before (a detached inode's, all in memory, still work, as on a
      read-only mount). A `write` checks again after its own reclaim
      (`EROFS`, nothing written) and after each partial page's read, either
      of which can meet the failure: nothing of the write lands in that
      page, so the first page's refuses the write (`EROFS`), and the last
      page's makes it short, the pages before it taken while the volume was
      healthy, as any write's bytes are until a sync. A new
      shared writable mapping is `EROFS` (section 5's `may_write`); on one
      made before the error, a store fault (a page not yet mapped, or one
      mapped read-only by a read) is a bus error (`SIGBUS`, as Linux's
      `page_mkwrite` on a shut-down `ext4`), and `mprotect` can't add write
      access (`EACCES`); a PTE made writable before the error stays so, its
      stores reaching the page cache, never the disk.
    - *Reads keep working,* from the block and page caches (which hold the
      failed operation's in-memory undo): the stopped cache evicts only
      clean blocks, and a miss with no clean slot is read straight from the
      device, uncached (`BlockCache::read_at`). A miss whose eviction met
      the failure looks again inside the block cache (`ensure_slot`), so
      the read that met it is served too.
    - *Then nothing more is written to the cache either:* when the last
      write-type operation running at the failure ends, or at once when a
      read meets it with none running, the latch settles: it freezes the
      block cache (`BlockCache::freeze`, above), and the seam refuses every
      write past it (`ReadOnly`, so `EROFS`). Dirty pages and metadata are
      never written back: reclaim evicts clean pages only, and `fsync`,
      `fdatasync`, `msync` and `FileSystem::sync` write nothing and return
      `EIO` (what they would write will never land; `sync(2)` still returns
      0), without a warning per inode.
    - *One line,* when the freeze comes:
      `ERROR vfs: ext4 <dev>: <op> failed writing to the device (<cause>);
      the volume is read-only until reboot (errors=remount-ro); run e2fsck
      -f`, `<op>` the operation whose write failed (`unlink`, `rename`,
      `writeback`, `fsync`, ...: the innermost write-type operation, begun
      before the failure, of the thread whose call first saw it, which with
      two operations running at the failure may be the other one's), or
      `a read's eviction`, and `<cause>` the block cache's first failure. `mount` shows its mode as
      `ro,errors`, `/proc/mounts` as `ro,errors=remount-ro`.
    - *Not marked on disk:* the superblock's `s_state` error bit and
      `s_error_count` aren't set. `ext4plus` 0.1.0-rc.2 neither reads nor
      writes them, and the only way to the disk is the block cache, whose
      block holding the superblock also holds the free counts the failed
      operation changed in memory: writing it would land part of that
      operation, on a device that has just failed a write. So nothing on
      disk says the volume errored, and no mount refuses it for that. Nor
      does Lumen clear `EXT2_VALID_FS` in `s_state` at a writable mount and
      set it again at a clean unmount, as Linux does: it never unmounts
      cleanly, so a mount that refused a volume without the bit would mount
      every boot read-only. `e2fsck -p` therefore skips the volume: run
      `e2fsck -f`.
    - Tested by `ktest:lumen_kernel::vfs::ext4::ext4_a_write_error_makes_the_volume_read_only`
      (a fault in `unlink`, `rename` and `rmdir`, then every write-type
      operation `EROFS`, reads working, the device's bytes unchanged
      through writes, syncs and evicting reads, the line, `ro,errors`),
      `ext4_a_failed_device_write_makes_the_volume_read_only` (the real block
      cache over a failing device: an `fsync`'s sync, and a read's
      eviction, which is then served),
      `ext4_nothing_reaches_the_device_after_the_first_failed_write` (one
      device write failing part-way through a sync: the device's write
      count and bytes never change again, through the rest of that sync
      and after it, and a file cached nowhere reads back though every slot
      is dirty), `ext4_the_latch_settles_when_the_last_write_type_operation_ends`,
      `ext4_a_write_whose_reclaim_meets_a_failed_write_is_refused`,
      `ext4_a_write_refused_by_the_cache_alone_leaves_the_volume_writable`,
      `file_vm_a_volume_gone_read_only_takes_no_store_through_an_older_mapping`
      and the fault sweeps, which check the
      failed operation's own outcome and then that the volume went
      read-only.
  - **Rename over an existing target:** the target is held, not removed,
    until the source has landed at its name. Its entry is taken out with
    its `links_count` unchanged (`Dir::unlink` is handed one more than the
    count it keeps), so its inode and data stay allocated; if linking the
    source (or any later step before the move lands) fails, the entry is
    put back with every count unchanged and the rename returns the error,
    leaving the target as it was (POSIX). Once the move lands, the target
    loses that link; if it was the last, its inode is freed through a
    temporary entry in the target's directory, the first free name of
    `.lumen-rename-<ino>`, `.lumen-rename-<ino>-1`, ... (64 tries; a name a
    user or the rename itself already took is skipped) (linked
    and unlinked again, since `ext4plus` frees inodes only through
    `Dir::unlink`; retried once while that entry survives) and an open
    target is detached (Gate 6). A crash between taking the entry out and
    the move landing leaves the target as an inode no entry points at
    (`e2fsck` reattaches it in `lost+found`, data intact); one during the
    free can leave the temporary entry. A free that fails inside `ext4plus`
    leaves the inode for `e2fsck` (logged at `error`); the rename itself has
    happened. Putting the entry back can only fail on a second fault; the
    target then keeps its inode and data without an entry (logged).
  - **Names that aren't UTF-8** are listed, looked up, unlinked and
    renamed as any other (#268: VFS names are bytes, "Core abstractions"
    below), except on a volume whose directory hashes may be unsigned
    without a flag saying so, which mounts read-only (#308, the caveat
    below).
  - **A directory's size and blocks (#357)** are `ext4plus`'s: it grows a
    directory by a block inside `Dir::link` (an entry that doesn't fit a
    linear directory, an htree leaf split) and writes its inode itself, and
    the adapter writes neither back. So each operation that links into a
    directory refreshes the cached values (`Ext4InodeMeta::take_dir_geometry`):
    a create, mkdir, link or symlink that worked from the `Dir` it linked
    through; a failed one (its failed link, or `discard_orphan`'s, may have
    grown the directory) and a rename (both parents, however it ended)
    re-read the inode (`Ext4Inode::retake_dir_geometry`: Lock 6 under the
    Lock 4 guard, as writeback takes them; an inode it can't read leaves
    the cached values, logged at debug). `stat` itself only reads memory,
    so it never fails (it answers alike while reads of the inode fail), and
    a removed directory keeps its own values even once its inode number is
    another file's
    (`ktest:lumen_kernel::vfs::ext4::ext4_a_directory_grown_by_ext4plus_stats_its_size_on_disk`:
    a linear directory grown by a link, a rename, a mkdir and a symlink, and
    an htree one by a create's split, each against the inode and a fresh
    mount; `stat` while reads of the inodes fail; a removed directory
    whose inode a new file took). The refresh after a failed create, mkdir,
    link or symlink, and after an exchange (which keeps both names, so
    doesn't grow a directory), is untested.

### POSIX VFS (`crates/lumen-vfs`, host-tested; kernel integration in `kernel/src/vfs/`)

Architecture adapted conceptually from Starry / `axfs_vfs` (any directly
adapted code elects `Apache-2.0` per section 2; fresh code is MIT):

- **Core abstractions:**
  - `FileSystem`: `dev_id()`, `root_inode()`, `sync()`, `read_only()`
    (asked at every check, never cached: an `ext4` volume becomes read-only
    when it errors, section 9's `errors=remount-ro`), and `errored()`
    (#349; `false` by default): read-only because of an error, which
    `MountInfo::errored` carries to `mount` and `/proc/mounts`.
  - `VfsInode` (cached by `(DevId, Ino)` as `Arc<VfsInode>`, except a
    non-`cacheable()` one (below), so hard links and multiple open FDs share
    one inode, one `PageCache`, one `AdvisoryLocks`
    table protected under the inode lock or a leaf `SleepMutex`, and one
    metadata lock): `stat()`, `lookup(name)`, `create(...)`, `mkdir(...)`,
    `unlink(name)`, `rmdir(name)`, `link(name, target)`, `symlink(name,
    target)`, `readlink()`, `rename(...)`, `readdir(cursor)` (the cursor
    opaque, any value taken without harm, each entry's `next_cursor` past
    the one before, and **stable as entries come and go**: a listing
    resumed after removals or additions lists every entry that stayed
    exactly once. tmpfs numbers entries as they are added (a name keeps
    its number through a rename over it or an exchange); procfs keys them
    as Linux does (`/proc`'s pids past its fixed entries, `/proc/<pid>/fd`
    by fd); devfs, whose entries never change, uses an index (procfs and
    devfs both list through `lumen_vfs::pseudo::fill_readdir`,
    host-tested); ext4 uses an
    entry's byte position, as Linux does for a linear directory, which
    holds because `ext4plus` never moves a record there (a removal merges
    or zeroes it, an insert takes the first slack with room or a new
    block), with two exceptions: a rename over a name, or an exchange, is
    an unlink and a link, so the name that stayed may move behind the
    cursor (skipped) or ahead (listed again); and in a hashed (htree)
    directory an insert repacks a leaf block, split or not, moving its
    records (`ext4plus` never makes one, so only directories made outside
    Lumen, such as the Alpine image's large ones, are affected). A
    directory removed while open lists as empty (Linux: `ENOENT`,
    `IS_DEADDIR`; a deliberate difference); entries pushed before an `Err`
    are valid),
    `truncate(size)`,
    `set_mode/owner/times(...)` (`set_owner` also leaves the mode
    `lumen_vfs::mode_after_chown` gives: setuid dropped on a non-directory,
    setgid too with group-execute, in the same metadata-lock hold and
    inode write, #254; `set_times` sets each stamp to
    `SetTime::resolve(current, now)`: kept for `Omit`, `now` for `Now`,
    the given time for `Set`, #302), plus `read_page`/`write_page` feeding
    `PageCache` and `sync()`, `append(buf) -> (pos, n)` (#254b: `O_APPEND`'s
    write, the end read and the write made in one hold of the inode's
    Lock 4, so appenders through separate open file descriptions never
    write at the same old end; ext4 and tmpfs implement it, and any
    filesystem with regular files must: the default, `append_without_end`,
    is `-EINVAL` for a regular file and `write_at(0, ..)` at position 0 for
    a device), and `is_tty()` (default `false`; `true` for
    devfs's `tty` and `console`, answered from fixed state without
    locking): the FD layer routes a terminal's descriptor to the process's
    console (reads, writes, the tty `ioctl`s, readiness `poll`, `-ESPIPE`
    on `lseek`) and gives every other `-ENOTTY` for the tty `ioctl`s (#254b). `VfsInode::drop` must never acquire a sleeping
    lock.
  - **Names are byte strings (#268):** a name is any bytes but `/` and
    NUL, as on Linux, and goes through the VFS as `&[u8]` (owned:
    `Box<[u8]>` or `Vec<u8>`): `VfsInode`'s `lookup`, `create`, `mkdir`,
    `unlink`, `rmdir`, `link`, `symlink`, `rename` (a symlink's target is
    any bytes but NUL, `/` included), `DirEntry::name`, the `DentryCache`'s
    key, `resolve_parent`'s `Final::Name`, `compute_cwd_path`'s result, `OpenFile::path`
    (`Arc<[u8]>`: the path `/proc/<pid>/fd` reads back, the base of a
    relative open's, and the cwd's fallback), and the paths
    `/proc/<pid>/maps` names for file mappings. No layer checks a name for
    UTF-8, so an ext4 entry whose name isn't is listed, looked up,
    unlinked and renamed like any other (but see #308 below: on a volume
    whose directory hashes may be unsigned without a flag saying so, only
    listed and looked up), and `getcwd`
    under it gives its bytes. Text shows a name or path through
    `lumen_vfs::Escaped`, never lossily (U+FFFD would name another file):
    UTF-8 as it is, except an ASCII control character and a byte that
    isn't UTF-8 as `\xNN`, `\` as `\\`, and any other character
    `char::escape_debug` escapes as `\u{..}` (C1 controls, bidi overrides,
    and also nonspacing and enclosing marks, the `Grapheme_Extend` set,
    U+00A0 and U+00AD, so a decomposed accent shows apart from its
    precomposed letter; a spacing mark or decomposed Hangul passes as it
    is, still in bytes of its own). Its users: log lines, `vfstest ls` and
    `vfstest readlink` output, and an exec error's `PT_INTERP` path.
    `/proc/<pid>/maps` prints the raw bytes, a newline as `\012` (Linux's
    `seq_path`). Two exceptions stay text: mount paths
    (`VfsTables::mount`, `/proc/mounts`; only the kernel mounts, at `/`,
    `/tmp`, `/dev` and `/proc`), and the paths a binary is loaded by
    (`Exec::from_path`, `ImageFile::path`, so an ELF image's maps lines,
    and `/proc/<pid>/exe`, read from `exe_path`): `linux /path` takes the
    shell's argument, and a `PT_INTERP` path that isn't UTF-8 is refused
    (`ExecError::InterpPathNotUtf8`); there is no `execve` yet.
    **Caveat (#308, #313):** a volume says how its hashed (htree)
    directories hash their names' bytes in `s_flags`:
    `EXT2_FLAGS_UNSIGNED_HASH` (which `mke2fs` sets where `char` is
    unsigned, e.g. aarch64 Linux) or `EXT2_FLAGS_SIGNED_HASH`.
    `ext4plus` 0.1.0-rc.2 always hashed signed, so on an unsigned volume a
    name with a byte of 0x80 or more (UTF-8 or not) in an htree directory
    could be missed by a lookup (when its signed hash picks another leaf
    than its real one), and an entry it added there was filed under the
    signed hash, where Linux won't find it (#308). Lumen builds a patched
    copy (`vendor/ext4plus`, the "Writable `ext4` driver" below) that
    takes the signedness from `s_flags` (#313: unsigned when it has
    `EXT2_FLAGS_UNSIGNED_HASH`, which Linux's `ext4_hash_info_init` checks
    first, else signed; `Superblock::htree_hash_unsigned`) for lookup,
    insert and a leaf split's rehash alike, so such a volume mounts
    writable. Its hashes are pinned against e2fsprogs' `ext2fs_dirhash`
    for half-MD4 and TEA, signed and unsigned
    (`cargo:lumen-vfs`'s `ext4plus_htree_hash` test); `ext4plus` has no
    legacy hash (version 0): its hash refuses that version either way
    (host-tested), and Lumen answers a lookup in such a directory with an
    unlogged `EIO` (not tested in the kernel). An htree lookup has no
    linear fallback. With neither flag on a volume with
    `dir_index`, Lumen can't tell: Linux since 6.2 builds with
    `-funsigned-char` on every architecture, so its `ext4_hash_info_init`
    hashes such a volume unsigned, and it writes the flag only from
    `__ext4_fill_super` on a mount writable from the start, never on a
    read-only mount later remounted read-write (`__ext4_remount`), so it
    may have indexed directories unsigned and left no flag; `e2fsck`
    stamps the flag its own host's `char` picks
    (`e2fsck_fix_dirhash_hint`). `ext4plus` hashes such a volume signed,
    which may be wrong, so the adapter guards it: it mounts it read-only
    (with the forced-read-only warning naming the flags' absence beside
    `dir_index`), and its
    `lookup`, on a miss in an htree directory of such a volume for a name
    with a byte of 0x80 or above (the only bytes the two hashes read
    differently), walks the directory's entries (`scan_dir_for`) before
    saying `ENOENT`. On a sound directory, `lookup` (and so `stat` and
    `open`) and `readdir` then agree (a read error during the walk, or in
    the hash lookup before it, is that error, never `ENOENT`); `unlink`,
    `rmdir`, `rename` and every other change are `EROFS`. Only such a miss
    pays for the walk: an ASCII name's miss and a miss in a linear
    directory stay one lookup, and so does every miss on a volume with
    either flag, which gets no walk (counted in blocks read by
    `ktest:lumen_kernel::vfs::ext4::ext4_an_unsigned_hash_volume_mounts_writable_and_hashes_names_unsigned`,
    over `kernel/src/vfs/testdata/unsigned-hash.ext4`, made by
    `tools/ext4-unsigned-hash-image.py`: with the unsigned flag, the
    three names are found by their hashes alone, created, split, unlinked
    and removed; with neither flag, the walk finds them; with the flag set
    to signed, they are `ENOENT`). `scenario:test_fs::test_ext4_htree_names_with_high_bytes_fsck_clean`
    writes such names into that image's directory re-indexed by each hash,
    signed and unsigned, and `e2fsck -fn` then checks every entry's leaf.
    Lumen's own images say they
    hash signed: the ktests' images built in code
    set `EXT2_FLAGS_SIGNED_HASH` (or lack `dir_index`), and
    `tools/mkrootfs.sh` sets `s_flags` to signed (the same image on every
    host) on an image `mke2fs` made
    with unsigned hashes (on a host where `char` is unsigned), or with
    neither flag, through
    `tools/ext4-signed-hash.sh`, which changes only the flag, in the
    primary superblock, and checks the image's bytes after
    (`harness:test_rootfs::test_signed_hash_*`,
    `harness:test_rootfs::test_mkrootfs_*`). `mke2fs -d` indexes no
    directory; if one ever were indexed, mkrootfs.sh's final `e2fsck -fn`
    fails the build once it holds a name with a byte of 0x80 or above in
    a leaf its signed hash doesn't lead to (an ASCII name hashes alike
    either way).
  - **Read-only mounts and special files (#291):** `open_inode` (a write
    open: `O_WRONLY`, `O_RDWR`, `O_TRUNC`) and `check_access` (`W_OK`)
    skip their `-EROFS` for a special file, by its type alone
    (`InodeType::is_special()`: character and block devices, FIFOs,
    sockets), as Linux's `special_file()` in `do_dentry_open` and
    `do_faccessat` (and `may_open`'s `sb_permission`, which refuses only
    regular files, directories and symlinks); no filesystem can answer
    otherwise. `chmod`/`chown`/`utimensat` keep the mount check whatever
    the type, as Linux's `mnt_want_write` (devfs's mounts are writable,
    section 8, #301). A deviation until pipes and device routing by `rdev` exist: a
    special file stored on ext4 (none in M7's image) opens for writing on
    a read-only mount, as on a writable one, and its reads and writes are
    `-EINVAL` (Linux: a FIFO waits for its peer, a socket node is
    `-ENXIO`, a device node reaches its driver).
  - `InodeCache`: the `(DevId, Ino)` → `Arc<VfsInode>` map above. It evicts
    an entry only on `unlink`/`rmdir`/`rename`, a detach or `umount`, or
    when a lookup or open finds the cached entry dead (detached, `nlink ==
    0`, `stat` saying `ENOENT`, or another `ino`), never for size, so an inode whose existence the VFS can't follow must not
    enter it: `VfsInode::cacheable()` (default `true`; answered from fixed
    state, under Lock 9, without locking) is `false` for those, and every
    `VfsCacheOps` method that would insert one (`publish_lookup_inode`,
    `inode_get_or_insert_if_fresh`, ...: a lookup, `..`, a magic link's
    target, `open(O_CREAT)` of an existing name) hands it back as its
    success value instead, inserted into neither cache (a mounted one is
    never refused as stale, the `Err` that makes the caller retry). A non-cacheable directory's children are
    non-cacheable too: a dentry is keyed by its parent's `(dev, ino)`, so a
    cached child's dentry would outlive its parent (a `debug_assert!` in
    the resolver, on in the kernel's builds, checks it on a plain lookup). `procfs`'s `self`,
    `/proc/<pid>` and everything under it are the only ones (#263): they go
    with their process or fd, and pids only grow. Each lookup of one builds
    a fresh inode, so two opens of the same `/proc/<pid>` file don't share
    advisory locks or an open count.
    **A removed directory's parent (#318):** `InodeCache` also keeps,
    for each directory an `rmdir` or a `rename` over it removed, the
    directory it was removed from, as Linux keeps a deleted dentry's
    `d_parent`: `..` from it asks the filesystem first, as from any
    directory, and only on `-ENOENT` (ext4 and tmpfs give a removed
    directory no `..`; any other error is reported as it is) takes the
    recorded parent (`VfsCacheOps::inode_removed_dir_parent`), so
    `chdir("..")`, `open("..", O_DIRECTORY)` and `open("..", O_CREAT)`
    (`-EISDIR`) from a removed cwd or start directory act as on Linux, and
    `../..` and `../name` go on from there.
    - `ops` records it (`inode_record_removed_dir`) before the
      filesystem's `rmdir` or `rename`, under its rename lock (ext4's and
      tmpfs's: the directory can't move meanwhile, and a `..` from it
      always has an answer, the filesystem's until the removal and the
      record after), and forgets it (`inode_forget_removed_dir`) if that
      fails with the directory still linked; one gone all the same (as
      after ext4 fails to undo its disk changes) keeps it.
    - A record is keyed by the directory's `Arc` (a `Weak` to it, which
      keeps the address from being reused), never `(dev, ino)`: a
      directory removed while open is detached under another number, a
      freed one's is reused. The `Arc` a removal sees is the cached one,
      the same a cwd or a descriptor holds (and ext4 and tmpfs keep one
      `Arc` per live inode, so even an uncached lookup's is that one).
    - It holds the parent's `Arc` strongly, as `d_parent` does, so the
      parent stays reachable if it is removed in turn while held open
      (detached, under a number of its own). The parent is handed back
      without unifying it in `InodeCache` (unlike the filesystem's answer,
      which `lookup_dotdot` unifies): while live it is the one `Arc` its
      filesystem has for it (ext4 and tmpfs keep one per live inode;
      `umount` drops the records with the cache).
    - A record whose directory has no strong reference left is dead: each
      new record prunes its filesystem's dead ones (only its own: the
      caller holds that filesystem's rename lock, and another
      filesystem's inodes never drop under it) and each `..` that falls
      back to the records prunes every filesystem's (again after their
      parents drop, outside the lock, since a parent kept only by such a
      record leaves its own record dead), so a dead record keeps its
      parent until the next of those. `umount` drops the filesystem's
      records with its inodes.
    - `getcwd` (`compute_cwd_path`) checks each directory it names (the
      cwd, then each ancestor) is still linked once named: one removed
      during the walk (the cwd, or an ancestor the cwd moved out of first)
      can have had its number taken by a new entry in the parent just
      read, which the walk would have named. It then gives `-ENOENT` if
      the cwd is gone, or walks again if it lives.
    - Differences from Linux: a parent freed by its own removal, nothing
      holding it open, is `-ENOENT` from `..` (Linux still finds it): its
      number may be another inode's by now, which the dentry cache and the
      mount table, both keyed by numbers, would take it for. After
      `umount`, `..` from a removed directory on it is `-ENOENT`: `umount`
      doesn't refuse a filesystem in use, where Linux says `-EBUSY`. A
      different `Arc` of the same directory (only from a filesystem making
      a fresh one per lookup, uncached) finds no record.
  - `DentryCache`: keyed by `(parent_dev, parent_ino, name)` (the name a `Box<[u8]>`), storing
    child `(DevId, Ino)` with immediate invalidation on `unlink`, `rmdir`,
    `rename`, and `mount`/`umount`, and bounded at 4096 entries (oldest
    first out). A cached name is returned without asking the filesystem,
    so a non-`cacheable()` child is never inserted: every lookup of it asks
    the filesystem again (`/proc/<pid>` is `ENOENT` as soon as the process
    ends).
  - `MountTable`: maps mount-point `(DevId, Ino)` to mounted `Arc<dyn FileSystem>`
    and tracks each mount's parent mountpoint so `..` at a mount root crosses
    cleanly back to the parent filesystem (and `..` at `/` stays at `/`).
    Each mounted filesystem receives a distinct `DevId` encoded with one
    canonical `makedev(major, minor)` used identically by `stat.st_dev`,
    `statx.stx_dev_{major,minor}`, and `/proc/self/maps` (`0:1` for root `ext4`
    or fallback `tmpfs`, `0:2` for `/tmp`, `0:3` for `/dev`, `0:4` for `/proc`;
    from M8, `0:5` for `/dev/pts` and `0:6` for the unmounted `pipefs`,
    section 10), so `st_dev` is unique and cross-mount `rename`/`link` returns `-EXDEV`.
  - **`VfsTables<F: LockFamily>` (#264):** the three tables above, each
    behind its own lock (Lock 9, sub-order `MountTable -> DentryCache ->
    InodeCache`), and the one implementation of `VfsCacheOps` (the short
    probes `resolve` and `ops` make, each releasing its locks before it
    returns; the `cacheable()` checks above), with `mount_root`, `mount`,
    `umount` (the evicted inodes dropped after the locks are released) and
    the snapshots: of the filesystems for `sync`, of the inodes for the
    ktests' cache checks (and the host tests'), and `list_mounts` for
    `/proc/mounts` (each filesystem asked for its device, name and
    read-only flag after the lock is released), or `mounts_with_fs` (#324)
    for `mount`: the same rows, each with its filesystem, which `mount`
    asks a `tmpfs` one's quota of, all from one snapshot. No inode or
    filesystem `Arc` it lets go of drops under the locks (#320):
    `MountTable::umount` hands back the removed mount's record;
    `VfsTables::umount` drops its mountpoint after the guards and returns
    its filesystem; `MountTable::mount_with_ino` and `mount_root_with_ino`
    take the `Arc`s by reference and clone them only into the mount, so a
    refused `mount`'s or `mount_root`'s `Arc`s drop with their caller,
    after the guards; and every `InodeCache` method that
    evicts a stale entry or unifies a handed inode with a cached one hands
    what it let go of back, in its return value or in a `Released` (a
    crate-private holder) declared before the guards, so it drops after
    them. `mount` asks its mountpoint's `inode_type()` before taking the
    locks. The calls into a filesystem it still makes under them,
    `VfsInode::dev_id`, `ino`, `cacheable` and `is_detached` and
    `FileSystem::dev_id` (probing a cached entry or a mount needs them),
    must by their docs answer without locking or blocking (each an atomic
    or fixed state in ext4, tmpfs, devfs and procfs). The host tests
    count, on a filesystem handing out a fresh `Arc` per lookup, each drop
    of its inodes and of the filesystem made while a table is held
    (`*_after_releasing_lock9`). It lives in lumen-vfs, generic
    over a lock family (`trait LockFamily { type Lock<T>: VfsLock<T>; }`),
    so the kernel and the host tests run
    the same code: the kernel's `KernelVfsTables` is
    `VfsTables<KernelLocks>` (`KernelVfsLock`, a `SleepMutex`), and the
    host tests' is `VfsTables<TestLocks>`: `TestLock`, a `std` mutex whose
    guards record which tables their thread holds, which test filesystems
    assert is none in their calls (`assert_test_lock9_free`), and which
    aborts the test binary, its message and a backtrace (the method and
    the test) on stderr, when a thread takes the tables out of their
    sub-order or takes one it already holds (a panic would leave a test
    whose other threads wait on a `Barrier` hanging; the abort loses the
    rest of that run's results).
    `VfsLock::new` isn't `const` (a trait method, on stable Rust), so the
    kernel's `static TABLES` is built by the `const fn
    VfsTables::from_locks(mounts, dentries, inodes)` from locks made by
    `KernelVfsLock::new_const` (`SleepMutex::new`, a `const fn`; for
    `TABLES` only) around empty tables. It and `VfsTables::new`, which
    builds the locks at run time, take the same argument-free constructors
    (`MountTable::new`, `DentryCache::with_default_capacity`,
    `InodeCache::new`), so neither names a capacity. Test-only
    `VfsCacheOps` wrappers (a spy, a hook) delegate to one.
- **Path & symlink resolution (`resolve_path`, host-tested):**
  - Handles absolute paths, `dirfd`/`AT_FDCWD` relative paths, physical
    component-by-component `.` and `..` traversal (never lexical `..` collapse
    before symlink expansion, so `/a/link/..` resolves relative to `link`'s
    target and `/nonexistent/..` returns `-ENOENT`), collapsed `///`,
    `PATH_MAX = 4096`, `NAME_MAX = 255` (`-ENAMETOOLONG`), and empty path `""`
    (`-ENOENT` unless `AT_EMPTY_PATH` is set or `readlinkat(fd, "")`).
  - **Symlink loop protection:** follows at most `MAX_SYMLINK_DEPTH = 40`
    symlinks per lookup, returning `-ELOOP` on cycles or deeper chains.
    Relative symlink targets resolve relative to the symlink's parent directory
    (never `cwd`/`dirfd`).
  - **No-follow and trailing slash rules:** `AT_SYMLINK_NOFOLLOW` and
    `O_NOFOLLOW` suppress following the final component **unless** the path has
    a trailing `/` (matching Linux: a trailing `/` forces directory traversal
    through the symlink, so both `lstat("symlink/")` and
    `open("symlink/", O_RDONLY | O_NOFOLLOW)` follow `symlink` and require the
    target to be a directory, returning `-ENOTDIR` if it resolves to a
    non-directory; `"file/"` returns `-ENOTDIR`). Without a trailing `/`,
    `open("symlink", O_NOFOLLOW)` returns `-ELOOP`, **except**
    `open("symlink", O_PATH | O_NOFOLLOW)` which opens an `O_PATH` descriptor
    to the symlink itself.
  - **The last component (#309):** `resolve_parent` gives a `ParentTarget`
    (the parent directory, the trailing `/`) whose last component,
    `final_component()`, is a `Final`: `Root` (a path of only slashes),
    `Dot`, `DotDot` or `Name(&[u8])` (borrowed from the path; never empty,
    `.` or `..`). A name's bytes come only out of a pattern on it, so each
    namespace operation (`open` with `O_CREAT`, `mkdir`, `unlink`/`rmdir`,
    `link`, `symlink`, `rename`) states its own errno for the other three
    (below; `-EEXIST` for `mkdir`, `link` and `symlink`, and for `O_CREAT |
    O_EXCL`; `O_CREAT` without `O_EXCL` resolves the directory `/`, `.` or
    `..` names, which then gives `-EISDIR` as any directory opened with
    `O_CREAT`, `..` from a removed directory included: its recorded
    parent, #318, under `InodeCache` above; `-ENOENT` if that parent was
    freed), ahead of
    `-EROFS`. In `ops.rs` each reads it with a `match` on
    `final_component()`, so one that leaves a case out doesn't compile;
    clippy lints forbidden in that file and the host test
    `ops_reads_final_only_through_a_direct_match` keep it to that form. The
    comment at the top of `ops.rs` is the one statement of what they refuse
    and what they miss (a shared helper answering the three alike, for
    one); outside `ops.rs` all of it is a convention.
  - **Namespace operations check in Linux's order** (`unlink_at`,
    `rename_at2`, `link_at` as v6.12's `fs/namei.c`; `readlink_at` as
    `fs/stat.c`; #254):
    - `unlink("symlink/")` is `-ENOTDIR`, the link not followed (unlike
      `lstat`); a last component that isn't a name (`/`, `.`, `..`) is
      `-EISDIR` for unlink and `-EBUSY`/`-EINVAL`/`-ENOTEMPTY` for rmdir.
    - `rename` (`do_renameat2`, `vfs_rename`): `-EXDEV`, ahead of the
      `-EBUSY` such a name gives on the old side (`-EEXIST` on the new side
      with `RENAME_NOREPLACE`, else `-EBUSY`); then the old name's
      `-ENOENT`; `RENAME_NOREPLACE`'s `-EEXIST`; `RENAME_EXCHANGE`'s
      missing (`-ENOENT`) or slashed non-directory (`-ENOTDIR`) new name;
      when the source isn't a directory, a trailing `/` on its name or
      (unless exchanging) the new name, `-ENOTDIR`; the source an ancestor of
      the new name's directory, `-EINVAL`, and the new name an ancestor of
      the source's, `-ENOTEMPTY` (`-EINVAL` exchanging); the same inode both
      sides, a no-op; the types (`-ENOTDIR`, `-EISDIR`); a mountpoint either
      side, `-EBUSY`; the filesystem's own errors (`-ENOTEMPTY`).
    - `link` (`do_linkat`, `filename_create`, `vfs_link`): the new name's
      `-EEXIST` (a trailing `/` too) or `-ENOENT` (a trailing `/` on a free
      name), then `-EROFS`, then `-EXDEV`, then a directory's `-EPERM`.
    - `readlink_at` of an empty path on a non-symlink is `-ENOENT`
      (`-EINVAL` for any other path).
- **Open file descriptions vs FD table:**
  - `FdEntry { file: Arc<OpenFile>, cloexec: bool }` in per-process `FdTable`
    (capacity `MAX_FDS = 1024`).
  - `OpenFile { id: OpenFileId, path: Arc<[u8]>, access_mode: AccessMode, is_path: bool, status_flags: AtomicU32, fd_refs: AtomicU32, offset: L, dir_cursor: L, open_guard: OpenGuard }`,
    `L` a `VfsLock<u64>` (the kernel's `KernelVfsLock<u64>`, a `SleepMutex`).
    Its inode is `OpenFile::inode()`, read from its `OpenGuard` (#264): no
    field of its own that could be pointed at an inode it doesn't keep open.
  - **Opening (#262):** `open_inode_guarded` returns an `Opened` (the
    `OpenGuard` it took before its last liveness check, Gate 6 above, the
    path, and the flags it checked) and `OpenFile::from_opened` takes that
    reference over (one count, never two) with those flags, which can't be
    changed in between. `openat` must use `OpenFile::from_opened(open_inode_guarded(..)?)`
    (`openat` does, since #254b). `OpenFile`'s access mode and `O_PATH` flag are
    private (getters only). `open_inode` (its signature unchanged) returns
    the inode with that reference given back: an
    `OpenFile::new_unchecked_flags` built from it (not for `openat`: its
    flags get none of the open's checks) takes a fresh one through
    `OpenGuard::try_new`, refused with `-ENOENT` if an unlink freed the
    inode in between (only an inode reporting `st_nlink >= 1`, or detached,
    can be opened). `OpenGuard::new_unchecked` (named so the risk shows at
    each call) is only for an inode nothing can free before the count goes
    up: one another reference already keeps open (a clone, an inherited
    `cwd`), or a file just created under its filesystem's Lock 3 on a
    filesystem with a rename lock. The kernel's `vfstest` opens through
    `open_inode_guarded` + `OpenFile::from_opened`; the unchecked guards left
    in `kernel/src/vfs/tests.rs` are ktest set-up.
  - `O_PATH` descriptors ignore `O_CREAT`/`O_EXCL`/`O_TRUNC`/`O_APPEND` and
    access mode; I/O, `lseek`, `mmap`, `ftruncate`, `flock`, `F_SETLK*`,
    `fsync`, `fchmod`, `fchown`, and `getdents64` return `-EBADF`, while
    `close`, `fstat`, `*at(AT_EMPTY_PATH)`, `fchdir` (if dir), `readlinkat(fd,
    "")`, and `fcntl(F_GETFD/F_SETFD/F_GETFL/F_DUPFD*)` succeed.
  - `F_DUPFD`/`F_DUPFD_CLOEXEC` share the same `Arc<OpenFile>` (shared offset,
    status flags, and `flock` ownership) while keeping independent `FD_CLOEXEC`.
- **Advisory file locks (`crates/lumen-vfs::locks`, host-tested; the
  kernel's blocking, below, ktest-tested in `syscall/lock.rs`):**
  - `flock(2)` and `fcntl` POSIX record locks are **independent lock
    namespaces** (matching Linux).
  - **`flock(2)` locks:** keyed by `OpenFileId` (`LOCK_SH`, `LOCK_EX`,
    `LOCK_UN`, `LOCK_NB`; allowed on `O_RDONLY` fds, `-EBADF` on `O_PATH`).
    Same `OpenFileId` upgrades/downgrades atomically when nothing
    conflicts; released automatically when the last FD referencing that
    `OpenFile` closes. A blocking request that conflicts drops the lock its
    description holds first, as Linux's `flock_lock_inode`
    (`FlockOutcome::WouldBlock { released }`); a `LOCK_NB` one keeps it and
    fails with `-EWOULDBLOCK` (a deviation: Linux drops it there too).
  - **`fcntl` POSIX record locks (`F_GETLK`/`F_SETLK`/`F_SETLKW`):** keyed by
    `(pid, start..end)` byte ranges (`l_len == 0` means to `i64::MAX`;
    negative `l_len < 0` normalizes to `[base + l_len, base)`). `F_SETLK*`
    checks in Linux's order (`flock64_to_posix_lock`, `assign_type` last,
    then `check_fmode_for_setlk`): the range first (an invalid `l_whence`
    or a computed `start < 0` is `-EINVAL`, a range past `i64::MAX`
    (`OFFSET_MAX`) `-EOVERFLOW`, `VfsError::Overflow`), then an invalid
    `l_type` (`-EINVAL`; `F_GETLK` checks the type first, as Linux's
    `fcntl_getlk`), and only then the open mode (`F_WRLCK` requires
    a writable fd, `F_RDLCK` a readable one, else `-EBADF`; `O_PATH` →
    `-EBADF` in the kernel first). Supports split, coalesce, overwrite, and
    non-conflict within the same `pid`, returns `-EAGAIN` on conflict for
    `F_SETLK`, and releases **all** POSIX record locks held by `pid` on that
    inode when `pid` closes **any** FD referencing that inode (POSIX
    requirement relied on by SQLite, Cargo, and Git).
  - **What a call released, and who blocks it:** `flock` answers
    `FlockOutcome::{Granted, WouldBlock} { released }` and `setlk`
    `RecordLockOutcome::{Granted { released }, WouldBlock { holder }}`.
    `released` is exact: an unlock that removed a lock, an `EX` → `SH`
    downgrade (an `F_RDLCK` over a held write lock), or a blocking `flock`
    dropping its lock; never a new lock, an upgrade or an unlock of
    nothing. `holder` is the pid owning the first conflicting lock (the one
    `F_GETLK` reports).
  - **Blocking (`F_SETLKW`, `flock` without `LOCK_NB`):** on `WouldBlock`
    the kernel's `syscall/lock.rs` waits on one kernel-wide `WaitQueue`
    (`process::wait_for_lock_release`, not one per inode, so `VfsInode`
    needs none) and retries. A release count read before each attempt makes
    a release between the attempt and the wait end the wait at once. Every
    lock call that `released` something and every `close`/exit/`dup2`
    displacement that gave a lock up (`process::release_fd`) bumps it and
    wakes every waiter (`process::locks_released`), after Lock 4 is
    released. `released` and the release functions are `#[must_use]`, and
    `VfsInode::with_locks_mut` is `Err` only when its closure didn't run,
    so no release is dropped on the way. B2 makes these waits interruptible:
    a pending signal ends the wait with `-ERESTARTSYS`, which the
    syscall-exit hook resolves before returning to user space.
  - **`EDEADLK` (`PosixWaitGraph`, host-tested):** before it waits,
    `F_SETLKW` records that its thread `(tgid, tid)` waits for `holder`,
    stamped with the release count it read before its try, in the kernel's
    `POSIX_WAITERS`
    (a `PosixWaitGraph` behind a spinlock, the count passed in as `now`).
    The wait is refused with `-EDEADLK` where following the holder's
    lowest-tid current record, its holder's lowest-tid current record, and
    so on leads back to the caller's tgid, through at most
    `MAX_DEADLOCK_CHAIN` = 11 records: Linux's loop checks
    `i++ > MAX_DEADLK_ITERATIONS` (10) before each comparison, so it
    compares 11 blockers too. A search cut off there says so
    (`WaitSearch::Truncated`) and the kernel warns once a boot: with no
    signals, a longer cycle would hang every process in it. A record is
    stamped with its waiter's `seen` and followed only while that equals
    the caller's `now`: records stamped before the last release are never
    followed (that
    release woke their waiter, which records itself again if it still
    waits), and a caller whose own stamp is stale (a release came after its
    try) is neither checked nor recorded (its old record removed): its
    wait ends at once and it retries with a fresh view. **Approximation:** each waiter records one
    holder, as Linux's `posix_locks_deadlock` follows each waiter's single
    `fl_blocker`, so a cycle through a second holder of the same range
    isn't found (that waiter waits, as on Linux). A cycle through another
    sibling's edge is also unseen: Lumen chooses the lowest current tid;
    Linux follows one blocked lock per owner. `flock` detects no
    deadlock (nor does Linux).

### Pseudo-filesystems (`/tmp`, `/dev`, `/proc`)

- **`/tmp` (`tmpfs`, `dev = 0:2`):** full in-memory writable POSIX filesystem
  (regular files backed by `PageCache` frames so `MAP_SHARED`/`MAP_PRIVATE`
  work identically to `ext4`, directories, symlinks, hard links, native
  unlink-while-open via `Arc`, `renameat2`, `chmod`/`chown`/`utimensat`),
  bounded by a configurable memory quota (default 32 MiB in M7, returning
  `-ENOSPC` instead of exhausting kernel frames or heap). The quota, counted
  in bytes (`QuotaCounter`), pays for the data pages and, since #267, for
  each inode but the root (`tmpfs::charge::INODE_COST`: the inode as
  allocated; a directory's two maps' root nodes, `charge::DIR_COST`), each
  directory entry (`charge::entry_cost`: the name's heap block, kept twice,
  and its share of the maps' other nodes, each holding at least five
  entries) and each symlink's target's heap block: an estimate from above
  of what each takes from the kernel heap, from std's `BTreeMap` layout
  (B = 6) and `lumen_heap`'s 16-byte granule, which a ktest measures
  against the heap. Not charged: the root inode and its maps' root nodes
  (one per mount), a file's page map, advisory locks, and the VFS caches'
  entries. A name is charged while it exists, an inode until its last
  reference goes (an unlinked file still open keeps its charge, as its
  pages; so does a removed directory's parent that the inode cache's
  record for `..` holds (#318), until that record is pruned once nothing
  else holds the removed directory: at the next directory removal on the
  mount, or the next `..` taken from a removed directory). A rename to a new name moves the old name's charge to it,
  reserving only what the new one costs more before anything changes: on
  a full mount a name no costlier than the old one still fits, and the
  count never passes the limit.
- **`/dev` (`devfs`, `dev = 0:3`):** a writable mount (`rw` in `mount`
  and `/proc/mounts`), as Linux's devtmpfs (#301), whose names never
  change. lumen-vfs's own checks answer first, as on any writable mount
  (a missing name to unlink or rename `-ENOENT`, a node that is no
  directory `-ENOTDIR`, a name in use under `RENAME_NOREPLACE` `-EEXIST`,
  a link from another mount `-EXDEV`; a rename onto itself does nothing
  and returns 0), and devfs
  refuses only what reaches it: its namespace operations (`create`,
  `mkdir`, `unlink`, `rmdir`, `link`, `symlink`, `rename`:
  `lumen_vfs::fixed_namespace_ops!`) return `-EROFS`, `mkdir` and
  `symlink` onto a name in use too (a divergence: devtmpfs takes these
  from root, Linux says `-EEXIST` for a name in use, and it never answers
  `-EROFS` on an `rw` mount), as does a `chmod`/`chown`/`utimensat` of
  `/dev` itself (a divergence too: devtmpfs takes those). Its character
  devices (`S_IFCHR`) open for writing and pass `W_OK`. A node's
  `chmod`/`chown`/`utimensat` is taken, as devtmpfs takes it: devfs keeps
  the new mode, owner and times in memory until reboot (#293). The nodes,
  with standard Linux `major:minor` `st_rdev` and these permissions at boot:
  - `/dev/null` (`1:3`, `0o666`): `read` → 0 (EOF), `write` → `count`, `lseek` → 0.
  - `/dev/zero` (`1:5`, `0o666`): `read` → zero bytes, `write` → `count`,
    `lseek` → 0, `mmap` → anonymous zeroed pages.
  - `/dev/random` (`1:8`, `0o666`) and `/dev/urandom` (`1:9`, `0o666`):
    `read` → `crate::rng` (`-EAGAIN` while the pool is unseeded: neither
    RDRAND nor virtio-rng has fed it, never a clock-seeded fallback),
    `write` → `count`, `lseek` → 0.
  - `/dev/tty` (`5:0`, `0o666`) and `/dev/console` (`5:1`, `0o620`): route to
    the calling process's `ConsoleIo` (supporting `read`/`write`/`ioctl`
    `TCGETS`/`TCSETS*`/`TIOCGWINSZ`/`poll`; `/dev/ptmx` and `/dev/pts/*` join
    in M8, and both become ttys of their own: section 10, designed in #336).
- **`/proc` (`procfs`, `dev = 0:4`):** read-only dynamically generated views
  (`st_size == 0` on virtual regular files, matching Linux):
  - `/proc/self` → symlink to `<current_pid>`; `/proc/<pid>` resolves for the
    calling process's PID. None of these per-process nodes is ever in
    `DentryCache` or `InodeCache` (`cacheable()` is `false`), so each lookup
    asks `procfs` whether the process or fd still exists. Nor is `self`,
    which names whichever process asks. Open descriptions retain numeric
    pids, not pinned identities: after pid reuse an old `/proc/<pid>/...`
    descriptor can observe the new process. This differs from Linux's
    pinned `struct pid`; C1 documents the difference rather than changing
    procfs ownership.
  - `/proc/<pid>/exe` → symlink to the reached executable's absolute identity,
    with symlinks and dot components resolved. PT_INTERP uses the same rule.
    It expands its stored path in the caller's resolver, sharing the 40-link
    limit rather than recursively starting a fresh resolver; cycles give ELOOP.
    The stored path is not an inode pin and can go stale on rename/unlink.
    Readlink retains raw identity bytes; an unnamed exec gives ENOENT.
  - `/proc/<pid>/fd` → directory of **magic symlinks** `0`, `1`, `2`, ...:
    `readlink` returns each open FD's path (appending ` (deleted)` if
    unlinked), while traversing `/proc/<pid>/fd/<n>` (e.g. `open`/`stat`)
    resolves directly to `OpenFile::inode()` rather than re-parsing the text
    target.
  - `/proc/<pid>/maps` → strict Linux format
    `{:08x}-{:08x} {}{}{}{} {:08x} {:02x}:{:02x} {}` (followed by the name
    only when named, after 73 characters as Linux's `seq_pad` puts it, omitting the
    trailing space on unnamed anonymous VMAs; `p` for private/anonymous, `s`
    for `MAP_SHARED`, `[heap]`, `[stack]`, or a file mapping's path (the one
    it was opened by, ` (deleted)` once unlinked) with the file's
    `dev_major:dev_minor` and `ino` read from its inode as the listing is
    made; the executable's and interpreter's segments, copied into anonymous
    memory at load, are named by the ranges the loader (`commit_loaded_elf`)
    records (each segment's file bytes, at its file offset; its `.bss`
    past them anonymous), trimmed by any unmap over them; the lines'
    formatting is `lumen_vm::maps`'s, host-tested), enabling `musl`'s `pthread_getattr_np` and runtime stack/mapping
    discovery (empty for a process with no address space, its takedown or
    a zombie, as Linux's). Generated lock-order-safely by cloning
    `Arc<AddressSpace>` under `Process::mm` (found through `PROCS`), releasing
    both, locking `AddressSpace::inner`
    (Lock 1) with no `VfsInode` lock held, formatting into a kernel buffer,
    and copying via `copy_to_user`.
  - `/proc/cpuinfo`, `/proc/meminfo` (live `MemTotal`/`MemFree`/`MemAvailable`/
    `Cached` in kB from `mm::frame` + `PageCache`), `/proc/mounts` (and
    `/proc/self/mounts`), and `/proc/version`.

### Boot mount sequence and shell commands (M7)

- **Boot (`kernel/src/main.rs`, right after `drivers::probe_all()` and `rng::init()`):**
  `vfs::init()` initializes the mount table. Scanning bound `virtio-blk`
  devices in PCI address order, it mounts the first device carrying a valid
  `ext4` superblock at `/` (read-write, or read-only if `VIRTIO_BLK_F_RO` is
  set); if none is present (including all M0–M6 scenarios without a disk
  attached, or a blank scratch disk in `test_blk`), it mounts a `tmpfs` root at
  `/` without warning (a `debug` line) so all prior scenarios remain green. A
  disk whose superblock can't be read, or whose magic matches but which fails
  to mount (an unsupported feature, corruption), is logged at `warn` before
  the fallback.
  `/tmp`, `/dev`, and `/proc` are then mounted, creating their mountpoint
  directories if absent on any writable root (a `tmpfs` root, or a read-write
  `ext4` root, where they persist); a mount that fails is logged at `warn`.
  From M8 (C9, designed in #336), `devpts` is mounted at `/dev/pts` after
  `/dev` (section 10).
- **Shell commands:**
  - `blk`: lists bound block devices (PCI addr, capacity, read-only, flush
    support, `BlockCache` hit/miss/dirty stats).
  - `mount`: lists active mounts as records (target, fs, dev, mode `ro`/`rw`,
    or `ro,errors` for an `ext4` volume errored read-only (#349), which
    `/proc/mounts` shows as `ro,errors=remount-ro`,
    and `used` and `limit`: a `tmpfs` mount's quota in bytes, `-` for any
    other filesystem; #324): with them an `ENOSPC` on `/tmp` can be
    checked against what its files take, a charge far past that pointing
    to a leak. `tmpfs::quota_use` reads them from a `dyn
    FileSystem` through its root inode's `VfsInode::as_any`, for each
    mount that one `VfsTables::mounts_with_fs` snapshot lists;
    `/proc/mounts` keeps
    Linux's columns.
  - `sync`: flushes every mounted filesystem (each flushing its own live
    inodes, then its block cache), continuing past a failing filesystem
    and reporting the first error (each failure logged once at `warn`,
    naming the inode or the block cache). Within an `ext4` volume it stops
    at the first failed device write, which makes the volume read-only
    (#349); an inode failing otherwise (`ENOSPC`) doesn't stop it.
  - `vfstest <op> ...`: test-facing file operations over the VFS (`stat`,
    `cat`/`read`, `ls`, `write`, `write-pattern`/`verify-pattern`,
    `truncate`, `chmod`, `mkdir`, `rmdir`, `unlink`, `rename`, `symlink`,
    `readlink`, `dev-tmp-proc`) from the kernel shell, which the scenarios
    use to set up and inspect files around ring-3 programs.
  - `linux /path/to/bin [args...]` (built in #255): when `args[0]` begins
    with `/`, loads the static or dynamic (`PT_INTERP`) ELF from VFS
    (following symlinks; a regular file with an execute bit, else `EACCES`),
    records the reached file's absolute byte identity, when known, as `exe_path`, which
    `/proc/<pid>/exe` reads, and runs it in ring 3, on a thread named after
    the path's last component as given (not a symlink's target, as Linux's
    `comm` after `execve`), cut and sanitised as any thread's name (section
    6, "Kernel threads"; #276): `linux /usr/local/bin/hello` runs as
    `hello`. Commands without a leading `/` continue to use M6's Limine
    boot-module lookup (`linux hello`, `linux busybox ash -s`), on a thread
    named after the module. `leaked_frames=` is the exact free-frame
    difference around the counted load/run, after one uncounted load and
    teardown establishes cache metadata and heap high-water storage. No
    cache or heap-growth term is subtracted. It cannot detect leaked heap
    objects that fit within already mapped capacity; a loader ktest also
    checks exact live heap bytes after teardown.

### Global lock ordering (M7–M8; M8's locks designed in #336)

Every lock in the VM/storage/VFS stack, and M8's process, signal and IPC
locks, obeys a strict acyclic total order: a lock may be taken while
holding only locks above it.

```text
 0. Locks held across a whole call, user copies included (SleepMutex), one
    at a time: OpenFile::pos (a B8 addition beside offset; a read, write,
    readv, writev, lseek or getdents64 of a regular file, a directory or a
    procfs file: Linux's f_pos_lock) or PipeObject::pipe (a pipe or FIFO,
    lumen-pipe's contract)
 1. AddressSpace::inner (SleepMutex); two only in fork_copy, the parent's
    then the child's (the child is new: nothing else holds it)
 1a. The TLB shootdown board, SHOOTDOWN (SleepMutex): taken only under
    Lock 1, with no lock below it held (2 through 10, 9a included)
    (section 5)
 2. OpenFile::offset / dir_cursor (SleepMutex)
 3. Per-filesystem rename_lock (SleepMutex)
 4. VfsInode metadata / directory / AdvisoryLocks lock (SleepMutex)
    — sub-order: directories strictly before non-directory children;
      multiple directories (under rename_lock) in ascending immutable lock_id;
      multiple non-directory children in ascending immutable lock_id
 5. PageCache lock (SleepMutex)
 6. Ext4 / Tmpfs filesystem lock (SleepMutex)
 7. BlockCache lock (SleepMutex)
 8. BlockDevice request lock (SleepMutex)
 9. Short-held VFS lookup tables (never held across locks 1–8 or I/O):
    MountTable -> DentryCache -> InodeCache
    (the kernel's mounted-ext4 registry, `vfs::EXT4_FILESYSTEMS`, is held
    the same way: cloned out and released before any lock 1-8)
 9a. Tty::lock (SleepMutex): a PTY pair's or a console's line discipline,
    foreground group and output (section 10); never held across a user
    copy or with any lock 0–9
10. IRQ-off SpinLocks, in this sub-order:
    a. PROCS (the process table: lumen-proc's ProcTree, processes and
       threads; not a leaf: Process::signal and the locks below nest
       under it, and ProcTree allocates under it)
    b. Process::signal (dispositions, pending signals, masks, stops,
       timers; not a leaf: TIMERS, the scheduler's locks and the heap nest
       under it)
    c. each a leaf but for the heap, none taken with another of them:
       Process::mm, Process::files and Process::fs (the Option<Arc> slots),
       Process::exe_path, Process::console and Process::exit (B1's M7
       state), Files::table, FsState::inner, TIMERS, an inode's mappers list
       (section 5), FIFO slot and `KernelFile::pipe_end` (section 10), and `syscall/lock.rs`'s POSIX_WAITERS (a
       PosixWaitGraph record's insert and removal allocate and free under
       it; never held with PROCS, an inode's lock or the lock wait queue's)
    d. a futex bucket (a leaf, except that one other bucket may be nested
       under it in ascending index order; nothing allocates or frees
       under it: lumen-futex's rule)
    e. a PollHead's subscriber list (section 10; M9's epoll ready list
       nests inside it); and B3's interim `serial_in::POLL_HEAD` holder
       (cloned and released before calling `notify`, never nested with
       another lock; gone once C10 moves the console to `Tty`)
    f. VirtioHal::TABLE; WaitQueue locks; the scheduler's run queues
       (two in ascending CPU order for stealing, affinity relocation from
       set_affinity/wake/kick/route_ready, or finish_switch, section 6).
       Signal locks may enclose kick; no run queue holder takes a wait
       queue lock
    g. serial_in::RECEIVE (a leaf taken under a 10f wait-queue lock by
       readiness callbacks, and by readers and IRQ 4; the handler releases
       it before wake/notify)
    h. Thread::name (a leaf, no allocation, no further locks; only the
       owner writes after construction; crash readers use try_lock)
    i. heap -> page tables -> frame allocator (section 5);
       frame::REFS is a leaf lock, taken only to copy out its boot-reserved
       table and released before any other lock or count update
```

M8 adds (designed in #336): Lock 0, Lock 1a, Lock 9a and, in level 10,
`PROCS` (built in B1, replacing M7's leaf `ACTIVE_PROCS`),
`Process::signal`, `Process::mm`, `Process::files`, `Process::fs`,
`Process::exe_path`, `Process::console`, `Process::exit`, `Files::table`
and `FsState::inner` (built in B1), `TIMERS`, the mappers lists, the FIFO
slots, the futex buckets and the `PollHead` lists.
Sections 5, 6, 8 and 10 name these locks and cite this list for their
places. Until each lands, M7's order holds
without it.

Lock 2 is held across a regular file's `read_at`/`write_at` (#254b), but
not across procfs's `read_at`: `/proc/<pid>/maps` takes the address
space's Lock 1 there, which comes first, so `read` reads procfs at a
snapshot of the offset and moves it after, under a fresh hold, only if it
is unchanged. `dir_cursor` (Lock 2) is held across every filesystem's
`readdir`, procfs's included (it takes `PROCS` and the process's leaf
locks, M7's leaf `ACTIVE_PROCS` before B1), and moved before it is
released; never across `getdents64`'s copy out. From M8 every such call on a regular file, a
directory or a procfs file also holds Lock 0's `OpenFile::pos` for its
whole length, so its Lock 2 holds are uncontended; they stay as M7's code
takes them.

**The five M8 hazards, resolved (B8).** Each was impossible while an open
file description had one thread (M7) and becomes possible once threads,
`fork` and `dup` share descriptions:

1. **A procfs `read` racing another reader of the same description
   dropped its offset update.** Resolved by `OpenFile::pos` (Lock 0), a
   field B8 adds to lumen-vfs's `OpenFile<L: VfsLock<u64>>` beside M7's
   `offset` and `dir_cursor`: a second `L` (its `u64` unused), so
   `OpenFile` keeps its one lock parameter and the kernel's is a
   `SleepMutex` like the other two. A `read`, `readv`, `write`, `writev`,
   `lseek` or `getdents64` of a regular file, directory or procfs file
   takes it before anything else and holds it across the user copies, so
   no other call on that description moves the offset meanwhile and the
   snapshot-then-move always moves. The `fcntl` record-lock commands
   also read a `SEEK_CUR` offset under it, released before Lock 4 and
   the wait. Linux's `fdget_pos` takes `f_pos_lock` for a file with
   `FMODE_ATOMIC_POS`
   (regular files and directories) when the file is shared, and for a
   directory always (`file_needs_f_pos_lock`); Lumen takes it always (one
   uncontended `SleepMutex` when not shared). Pipes, FIFOs, ttys and
   character devices have no position and don't take it, nor do
   `pread64`/`pwrite64`.
2. **A copy-out fault's give-back undid another reader's move.** Under
   `pos`, nothing moves the offset between a chunk and its give-back, so
   the give-back is exact.
3. **`getdents64`'s put-back undid another listing's move.** The same:
   `getdents64` holds `pos` across its `readdir` and its copy out.
4. **`dup*` looked up and installed in two holds, and `openat` checked for
   a free slot before it opened.** The FD table moved out of
   `ACTIVE_PROCS` into `Files::table` in B1 (a `SpinLock` per table, shared by
   `CLONE_FILES`). B8 makes `dup`, `dup2`, `dup3` and `F_DUPFD*` look `old`
   up and install the new slot in one hold (B1 keeps M7's two holds). A call that drops the table's lock
   between choosing a slot and filling it (`openat`, `pipe2`) first takes
   the slot with a lumen-vfs `FdTable` addition: `reserve(from) ->
   Result<FdReservation, VfsError>` (the lowest free slot at or above
   `from`, marked reserved; `VfsError::MFile` when none), then
   `install(r, file, cloexec)` or `cancel(r)`. A reserved slot is neither
   open nor free:
   `close`, `fstat`, `/proc/<pid>/fd`, the table's copy at `fork`,
   `drain_cloexec`, `drain_all` and `iter_open` all skip it, and `dup2`
   onto it is `-EBUSY`, as Linux's `do_dup2`. `openat` reserves before it opens, so a
   full table is `-EMFILE` before an `O_CREAT` creates anything (Linux's
   `get_unused_fd_flags` comes first too).
   C1's `ReservedFd` owns a shared `files_access` lease until install/cancel,
   so exec/unshare's exclusive lease cannot swap its table mid-open.
   Shared leases nest (`openat` then `with_files`); writers get no priority
   and may starve under continuing readers. Acquire leases only in thread
   context with interrupts enabled and no spinlock held.
5. **`POSIX_WAITERS` and POSIX record locks were keyed by pid.** A record
   lock's owner stays the process (its tgid): Linux's owner is the
   `files_struct`, which a process's threads share; the two differ only
   for `clone(CLONE_FILES)` without `CLONE_THREAD`, a deviation. A close
   or descriptor displacement releases every current Files member's locks
   on that inode. While holding the inode lock, it reads the member set
   under the table's leaf spinlock and releases each nonzero pid without
   allocating. Membership ends before reaping can recycle the pid, and a
   new lock on this inode needs the inode lock already held. After unshare,
   locks still follow the pid into the new table; Linux keeps them with the
   old files_struct (C3 will change the ownership key). The `EDEADLK`
   records move to the waiting thread: B8 changes lumen-vfs's
   `PosixWaitGraph` to key a record by `Waiter { tgid, tid }` and to follow,
   from a holding process, the record of its lowest-numbered waiting
   thread, as Linux's `what_owner_is_waiting_for` follows the first blocked lock of
   an owner it finds (host-tested in `cargo:lumen-vfs`). Two threads of one
   process waiting in `F_SETLKW` keep a record each.

Tested by `ktest:lumen_kernel::linux::process::shared_description_offsets_move_once`,
`ktest:lumen_kernel::linux::process::shared_description_getdents_lists_once`,
`ktest:lumen_kernel::linux::process::dup_and_openat_reserve_their_slot`,
`ktest:lumen_kernel::linux::process::open_reservation_survives_until_install`
(a paused `openat` keeps slot `r`: `dup2` gets `EBUSY`, `F_DUPFD`
gets `r + 1`, and the open returns `r`),
`ktest:lumen_kernel::linux::syscall::lock::fcntl_seek_cur_uses_committed_position`
and `ktest:lumen_kernel::linux::syscall::lock::setlkw_records_each_waiting_thread`
(section 15, "M8's tests", says what each checks).

**Invariants:**
- Never acquire any `SleepMutex` while holding any `SpinLock` (enforced by
  `SleepMutex::lock`'s `irq::enabled() && irq::depth() == 0` assertion).
- **Extract-and-drop outside the `SpinLock`s:** any syscall (`close`,
  `chdir`, `dup*`, `execve`'s close-on-exec, exit) that removes an
  `Arc<OpenFile>` (M7) or a `FileRef` (`Arc<KernelFile>`, M8), a `Cwd`
  or an
  `Arc<VfsInode>` from `Files::table`, `FsState::inner`, `PROCS`
  or `Process::signal` (`ACTIVE_PROCS`'s `ProcessState` before B1), extracts the `Arc` under
  the lock, drops the guard, and only then runs lock cleanup
  (`flock`/POSIX locks) or drops the `Arc`. The same for an
  `Arc<AddressSpace>` (`Process::mm`), an `Arc<Process>` leaving `PROCS`,
  and the `Weak<Process>` entries leaving `TIMERS` (upgraded, and the
  `Arc` dropped, only after `TIMERS` is released).
- Dropping an `IoFile` or the last `FileRef`, or calling `dec_fd_ref`,
  may release a pipe end and take Lock 0. Never do these while holding
  locks 1–10 or `OpenFile::pos`.
- Never hold locks 2–9 or 9a across `copy_from_user` / `copy_to_user`
  (which may trigger `#PF` and acquire Lock 1). Lock 0 is the one
  `SleepMutex` that may be held across them: a fault under it takes Lock 1,
  which comes after it, and nothing holding a lock numbered 1 to 10 ever
  takes Lock 0.
- Never wait for a TLB shootdown's acknowledgements with interrupts off or
  any `SpinLock` held (section 5).
- No interrupt handler sends a signal or takes a level-10 lock above `f`
  (section 8), except B3's interim serial console wake on IRQ 4
  (`serial_in::POLL_HEAD` and its `PollHead` subscriber list, level `e`,
  until C10 moves the notify to the tty's input thread).
- **Snapshot-and-release for level-9 tables:** `sync()` snapshots
  `Vec<Arc<dyn FileSystem>>` from `MountTable` and `Vec<Arc<VfsInode>>` from
  `InodeCache`, releases the table lock, and only then locks `VfsInode` →
  `PageCache` → `Ext4` → `BlockCache` → `BlockDevice`. `resolve_path` probes
  `MountTable`/`DentryCache` briefly and drops them before locking `VfsInode`.

### Snapshots, namespaces & agent undo (M11)

Per-process VFS mount namespaces confine sandboxed processes and agents to
explicitly granted subtrees (`..` cannot escape a mount root). OverlayFS or
block-level CoW snapshots (or an optional LumenFS volume) back `snap`/`undo`,
so an agent session still takes an O(1) snapshot before each step and supports
"Undo step" and "Undo all" (Principle 3).

## 10. IPC (pipes, FIFOs, readiness and PTYs designed in #336, M8; `AF_UNIX` and `epoll` planned, M9)

Instead of a separate custom IPC channel ABI, native ring-3 processes use
standard Linux IPC primitives:

- **Pipes, FIFOs, readiness and PTYs (M8):** below.
- **Unix domain sockets & event multiplexing (M9):** `AF_UNIX` (`SOCK_STREAM`,
  `SOCK_DGRAM`, `socketpair`, and `SCM_RIGHTS` FD passing), plus
  `epoll_create1`/`epoll_ctl`/`epoll_wait` (level- and edge-triggered, on
  the readiness interface below), `eventfd2`, and `timerfd_*`.
- **Desktop & automation bridge (M11):** userspace agents and desktop apps
  reach Lumen's automation tree (section 11) over a dedicated `AF_UNIX` socket
  (or `/dev/lumen-ui` device) carrying the existing postcard wire protocol
  (`lumen-app-api`), gated by the process's `AppActions` policy.

### Pipes and FIFOs (C7)

- **The object:** `pipe2` makes one `PipeObject { pipe: SleepMutex<lumen_pipe::Pipe>,
  read_seq: AtomicU64, write_seq: AtomicU64, r_opens: AtomicU64,
  w_opens: AtomicU64, readers: WaitQueue, writers: WaitQueue, poll: KernelPollHead }`
  (`read_seq`/`write_seq`: B2's `ready` rule) and two open
  file descriptions on an anonymous inode of `pipefs` (an internal
  filesystem, never mounted, dev `0:6`; `InodeType::Fifo`, a fresh `ino`
  each, so `fstat` says `S_IFIFO` and `/proc/<pid>/fd/N` reads
  `pipe:[ino]`, as Linux): the read end `O_RDONLY`, the write end
  `O_WRONLY`, each with `O_NONBLOCK` from `pipe2`'s flags. `Pipe::try_new`
  (64 KiB, `DEFAULT_CAPACITY`) failing is `-ENOMEM`. `PipeObject::pipe` is
  section 9's Lock 0, held across user copies as lumen-pipe's contract has
  it (`peek`, the copy out, `commit`; the non-blocking write loop).
- **Ends:** `KernelFile::end_refs` is the combined count of descriptors
  and in-flight references. Its transition from 1 to 0 releases the end
  once. `inc_fd_ref` raises it for `dup` and `fork`; every `Fd::file`
  caller, and `poll`/`ppoll`/`select`/`pselect6`, obtains an `IoFile`
  under `Files::table`, raising it before the table lock is released.
  Dropping the `IoFile` lowers it; ktest-only `io_refs` counts just the
  in-flight references, excluding descriptors. End release takes Lock 0 in
  thread context, never under locks 1–10 or `OpenFile::pos`. A never-installed description's `Drop`
  releases its attached end too, asserting that only its initial reference
  remains. A writer blocked on a full pipe therefore keeps its end across
  another thread's `close`, as Linux's in-flight `fget` reference keeps
  `pipe_release` from running until I/O completes. Metadata-only `FileRef`
  clones keep the description allocated but do not count in `end_refs`.
  `Released::Last` wakes every waiter on the other end and every poller.
- **`read` / `readv`:** one `read_with` holds the pipe lock across every
  span and chunk, so competing readers cannot interleave the spans. `peek`
  into a 4 KiB kernel buffer, copy out, then `commit_prefix` only the bytes
  copied (including a prefix before a fault within a span). A fault leaves
  the uncopied bytes queued and returns the partial count, or `-EFAULT`
  if no byte was copied. Repeat under the same lock until
  the count is met or the pipe is empty; empty with writers: `-EAGAIN`
  with `O_NONBLOCK`, else B2's interruptible wait on `readers` with the
  lock released (interrupted `-ERESTARTSYS`); empty with no writers: 0.
  Wakes follow lumen-pipe's rules (`Consumed::wake_writers`, the next
  reader).
- **`write`:** lumen-pipe's blocking and non-blocking loops as its docs
  give them (`WriteProgress`, `PIPE_BUF` atomicity, writer waits woken all
  at once), with `-ERESTARTSYS` for an interrupted wait that wrote
  nothing; no readers is `-EPIPE` and `SIGPIPE` to the calling thread, as
  Linux's `pipe_write` sends it (`send_sig(SIGPIPE, current, 0)`), checked
  before the first copy so a bad pointer with no readers is still
  `-EPIPE`. Siginfo is `SI_USER`, with the caller's tgid and uid.
  Blocking pipe `write`/`writev` are not capped at `MAX_SINGLE_IO`; they
  transfer in bounded chunks and sleep for space. `writev` uses the sum
  of all spans for the atomic/partial decision, filling an atomic buffer
  completely before queuing it (a fault is `EFAULT` with no torn prefix).
  `lseek`, `pread64`, `pwrite64` `-ESPIPE`. `fsync` `-EINVAL`
  (M7's rule).
- **FIFOs:** `mknodat(S_IFIFO)` stores an inode of type FIFO on tmpfs or
  ext4 (ext4 writes the type as e2fsprogs does; a FIFO in M7's image would
  have opened as M7's special-file deviation, section 9). The inode keeps
  `fifo: Option<Box<FifoSlot>>`, present only for FIFOs. `FifoSlot`
  contains `inner: SpinLock<FifoSlotInner { pipe: Option<Arc<PipeObject>>,
  opens: usize }>` (a level-10c leaf). An open reuses an existing pipe
  and raises `opens` under the lock. If absent, it allocates `Pipe::fifo`
  (no ends open) outside the spinlock, then rechecks and raises `opens`
  under the lock. An attaching competitor wins; the unused
  allocation is dropped after releasing the lock. Each end's release (the
  Ends rule above, not the last `dec_fd_ref`) lowers `opens`. Reaching 0
  detaches the `PipeObject` under that lock; its bytes are dropped outside
  the lock, as Linux frees a FIFO's pipe at the description's last `fput`. So a
  read blocked on a FIFO whose last descriptor another thread closed keeps
  the slot attached, and a reopen of the FIFO meanwhile joins that same
  pipe: its writes reach and wake the blocked reader. The
  FIFO inode's `pollable` delegates to the attached `PipeObject`; descriptor
  polling uses the description's writer generation. The
  open resolves the inode and
  releases every VFS lock before it takes the pipe's lock. Linux's
  `fifo_open` rules: `O_RDONLY` opens a reader and, without `O_NONBLOCK`,
  waits (interruptible, `-ERESTARTSYS`) until a writer has opened;
  `O_RDONLY | O_NONBLOCK` returns at once (make's jobserver read end);
  `O_WRONLY | O_NONBLOCK` with no reader is `-ENXIO`, without
  `O_NONBLOCK` it waits for a reader; `O_RDWR` opens both ends and never
  waits. Blocking opens wait for a change of `w_opens` or `r_opens`,
  monotonic generations raised under the pipe lock: a peer opening and
  immediately closing still completes the handshake. A nonblocking reader
  opening with no writers snapshots `w_opens` in its description; it reports
  `POLLHUP` only after that generation changes and writers return to zero.
  Readers joining an existing writer see its eventual departure too.
  Anonymous `PipeInode` reopens through `/proc/<pid>/fd/N` skip FIFO partner
  checks: no wait or `ENXIO`. Reads and writes then go as a pipe's.
- **`fcntl` and `ioctl`:** `F_GETPIPE_SZ`, `F_SETPIPE_SZ` and `FIONREAD`
  (section 8's table).
- Tested by `cargo:lumen-pipe`, `ktest:lumen_kernel::linux::syscall::fd::pipe_end_outlives_its_close_while_a_write_is_in_flight`,
  `ktest:lumen_kernel::linux::syscall::fd::fifo_reopen_during_a_blocked_read_reaches_that_reader` and `scenario:test_linux_io::test_pipes_and_poll`
  (section 15), plus these ktests:
  `ktest:lumen_kernel::linux::syscall::fd::pipe_readv_keeps_spans_in_one_read`
  (one read-lock acquisition across spans, `AB` read and `CD` left queued,
  empty/nonblocking, blocking short read and EOF),
  `ktest:lumen_kernel::linux::syscall::fd::pipe_readv_fault_commits_only_the_copied_prefix`
  (faults on the first byte, between spans and within a span),
  `ktest:lumen_kernel::linux::syscall::fd::pipe_atomic_writer_waits_for_the_entire_record`,
  `ktest:lumen_kernel::linux::syscall::fd::pipe_blocked_writer_gets_epipe_when_reader_closes`,
  `ktest:lumen_kernel::linux::syscall::fd::fifo_blocking_opens_wait_for_peers_and_interrupt_detaches`,
  and `ktest:lumen_kernel::vfs::ext4::ext4_fifo_mknod_descriptor_fault_never_frees_foreign_blocks`.
  D2's scenario also runs `make -j2` on a two-rule Makefile
  of `true` commands after its build: GNU make 4.4's jobserver FIFO
  (`mkfifo`, then `pselect6` on its read end), with nothing on stderr (make
  prints `jobserver mkfifo:` and an errno when `mkfifo` fails, then falls
  back to a pipe).

### The readiness interface (B3), for `poll` now and `epoll` in M9

Pure types in lumen-vfs (host-tested), implemented by each file kind:

```rust
// crates/lumen-vfs/src/poll.rs
pub struct PollMask(u16);           // Linux's bits: IN 0x1, PRI 0x2, OUT 0x4, ERR 0x8,
                                    // HUP 0x10, NVAL 0x20, RDNORM 0x40, RDBAND 0x80,
                                    // WRNORM 0x100, WRBAND 0x200, RDHUP 0x2000
pub trait PollWaker: Send + Sync {
    /// Called under the source's PollHead lock with the bits that may have
    /// turned on. May only set atomics and wake a thread (or, M9, queue an
    /// epoll entry under a leaf lock); never allocates or blocks.
    fn wake(&self, ready: PollMask);
}
pub trait Pollable: Send + Sync {
    /// The readiness of one open description now (`mode`: its access mode,
    /// since a pipe's two ends share one inode and an O_RDWR FIFO answers
    /// for both). With a waker, return a guard unless it has no wake source
    /// and the caller re-polls it on a timer (`console_step`); subscribe no
    /// later than the mask is read, so no change falls between.
    fn poll(&self, mode: AccessMode, waker: Option<&Arc<dyn PollWaker>>)
        -> (PollMask, Option<PollGuard>);
}
pub struct PollHead<F: LockFamily> { /* subscribers behind F's level-10e spin lock */ }
// PollHead::subscribe(Arc<dyn PollWaker>) -> PollGuard; PollHead::notify(PollMask)
```

- An `O_PATH` descriptor answers `POLLNVAL` before any `pollable` is
  asked (`poll`/`ppoll`: `POLLNVAL`; `select`/`pselect6`: mask `POLLNVAL`,
  which is in `POLLIN_SET`, `POLLOUT_SET` and `POLLEX_SET`; only a closed
  fd is `-EBADF`).
- `VfsInode::pollable(&self) -> Option<&dyn Pollable>`, `None` by default:
  regular files, directories, procfs and devfs's always-ready devices
  answer `POLLIN | POLLOUT | POLLRDNORM | POLLWRNORM` (M7's rule). Pipes,
  FIFOs, both PTY ends and the console tty implement it; M9's sockets,
  `eventfd` and `timerfd` will. `LockFamily` gains a level-10e spin lock
  (`LeafLock`: the kernel's `SpinLock`, the host tests' `std` mutex) for
  `PollHead`'s subscriber list. `subscribe` allocates its slot before
  taking that lock, and `PollGuard`'s `Drop` removes the waker and frees
  the slot after releasing it.
- **Interim console readiness (B3, replaced by C10):** devfs's `Console`
  and `Tty` nodes implement `Pollable`, reading `process::stdin_ready` and
  (on `ConsoleIo::Serial`) subscribing to `serial_in::POLL_HEAD`, which
  `serial_in::queue_waiting` notifies from IRQ 4 after releasing the
  holder lock. Waiters on a readable console take a 5 ms step for VT
  consoles and polled serial, and a 100 ms safety-net step under IRQ 4
  (any hit that finds input without an IRQ 4 wake is counted, logged once,
  and shown by `serial poll`). C10 replaces all of this with the console's
  `Tty` and input thread.
- **A pipe's mask** (Linux's `pipe_poll`): the read end `POLLIN |
  POLLRDNORM` when `!is_empty()`, `POLLHUP` when `reader_hung_up()`; the
  write end `POLLOUT | POLLWRNORM` when `writable()`, `POLLERR` when
  `writer_error()`; an `O_RDWR` FIFO description, the union. A tty's:
  "PTYs" below.
- **`poll`, `ppoll`, `select`, `pselect6`** (section 8's table) share one
  loop: a zero or already-expired timeout runs one unsubscribed pass and
  returns immediately. Otherwise a `PollWait` (a `PollWaker` holding an
  `AtomicBool`, `fired`, and a per-call `WaitQueue`) subscribes to every
  source on pass 1; if any is ready, it drops the guards and returns.
  Otherwise it waits with B2's `wait_interruptible(deadline, || fired)`,
  and after each wake clears `fired` before re-polling every source
  without re-subscribing, then waits again: a change during or after the
  re-poll sets `fired` again, so none is missed. The guards are kept until
  the call returns, by any way out (an interrupted wait included).
  `select` reads a mask through `fs/select.c`'s `POLLIN_SET`, `POLLOUT_SET`
  and `POLLEX_SET`.
- **M9's `epoll`** reuses it unchanged: an interest is a `PollGuard` held
  as long as the descriptor is in the set, and the epoll instance's waker
  queues the descriptor on its ready list; the `ready` bits passed to
  `wake` give edge-triggered mode its edge.
- Tested by `cargo:lumen-vfs` (`PollMask`, `PollHead`: no change lost
  between a poll and its subscription, a dropped guard unsubscribing),
  `ktest:lumen_kernel::linux::syscall::poll::poll_masks_of_each_file_kind`
  (B3: M7's file kinds and a test `Pollable` whose readiness the ktest
  sets; C7, C9 and C10 add rows for pipes, PTYs and the console),
  `ktest:lumen_kernel::linux::syscall::poll::poll_wakes_on_a_pipe_write`
  (C7) and `scenario:test_linux_io::test_poll_and_select_on_console`
  (section 15).

### PTYs (C9)

- **Nodes:** `devpts` mounted at `/dev/pts` at boot (dev `0:5`, after the
  `/dev` mount: section 9's boot mount sequence), listing one `N` per open
  pair (character device `136:N`,
  mode `0620`); `/dev/ptmx` in devfs (`5:2`, mode `0666`). Opening
  `/dev/ptmx` allocates the lowest free number below `PTY_MAX` (256;
  `-ENOSPC` past it) and a `PtyPair`, and returns the master's description;
  `/dev/pts/N` opens the slave (`-EIO` while the pair is locked, which it
  is from creation until `TIOCSPTLCK` with 0, musl's `unlockpt`, and after
  the master's last close; Linux's rules).
- **The pair:** `Tty { lock: SleepMutex<TtyInner>, seq: AtomicU64,
  readers, writers, poll }` with `TtyInner { ld: lumen_tty::LineDiscipline,
  to_master: a byte ring (the slave's output and its echo, 4096 bytes),
  session: Option<u32>, foreground: Option<u32>, locked: bool,
  slave_opens: usize, hung_up: bool }` (`seq`: B2's `ready` rule).
  `Tty::lock` is section 9's Lock 9a.
  Reads and writes go through a 4 KiB kernel buffer: a slave read whose
  copy out faults after the discipline consumed the bytes loses them and
  returns `-EFAULT`, as Linux's tty reads through a kernel buffer do.
- **Data flow:** a master `write` feeds each byte through
  `LineDiscipline::receive` while `accepts` holds (else waits, or
  `-EAGAIN` with `O_NONBLOCK`), its echo going to `to_master` through the
  `Output` sink and an `ISIG` character's `JobSignal` to the foreground
  group (`kill(-foreground)`). A slave `write` goes through
  `LineDiscipline::write` into `to_master` (`OPOST`), waiting while it is
  full. A master's `write` and a console's input thread drop any bytes not
  yet received when `flush_input` runs (`TCFLSH`, `TCSETSF`, a signal
  character without `NOFLSH`), lumen-tty's flush duty for its driver. A
  master `read` drains `to_master`; once the slave's last
  descriptor has closed (not before its first open, so a master read
  waiting for a child to open the slave just waits), with nothing queued,
  it is `-EIO`, as Linux's master read after `pty_close` sets
  `TTY_OTHER_CLOSED` (a later slave open clears it). A slave `read` asks
  `read_decision` with the call's `ReadTimer` (`VMIN`/`VTIME`) and then
  `read`: `Decision::WaitWithTimer { deciseconds, .. }` arms a deadline
  that many tenths of a second from now for B2's wait, `Decision::Wait`
  waits with the deadline already armed (or
  none), and a `TimedOut` answer calls `timer.expire()` before the next
  decision; interrupted `-ERESTARTSYS`.
  Every change notifies the waiters `Effects::wake` names and the poll
  head.
- **Readiness:** the slave `POLLIN | POLLRDNORM` when `poll_readable()`,
  `POLLOUT | POLLWRNORM` while `to_master` has room, `POLLHUP` once hung
  up; the master `POLLIN | POLLRDNORM` while `to_master` holds bytes,
  `POLLOUT | POLLWRNORM` while the discipline `accepts`, `POLLHUP` after
  the slave's last close (until a slave opens again). `POLLPRI` is never
  set (no packet mode), so `select`'s exception set never fires on a PTY
  (the census's vim passes one).
- **Hangup:** the master's last close hangs the slave up, as Linux's
  `pty_close` → `tty_vhangup`: the slave's reads return 0 and its writes
  `-EIO`, every process of the session loses it as its controlling
  terminal, the session's leader gets `SIGHUP` then `SIGCONT`
  (`tty_signal_session_leader`), and `/dev/pts/N` goes away (a later open
  `-ENOENT`, as Linux's `devpts_pty_kill`). A session leader's exit (section 8,
  "Exit", step 3; C9 for PTYs, C10 for the console) sends `SIGHUP` to its
  controlling terminal's foreground group, clears the terminal's session
  and foreground group under `Tty::lock`, and clears the controlling
  terminal of every process of the session (Linux's `disassociate_ctty(1)`
  and its `session_clear_tty`), so a later open of `/dev/tty` by any of
  them is `-ENXIO`. For its console Linux also hangs up the open
  descriptors (`tty_vhangup_session`); Lumen doesn't, a deviation. A
  second deviation: a leader that exits with no controlling terminal left
  sends nothing, where Linux sends `SIGHUP` and `SIGCONT` to the group it
  last had in the foreground (`tty_old_pgrp`).
- **The ioctls** (on the slave, and on the master for the pair): `TCGETS`;
  `TCSETS`, `TCSETSW`, `TCSETSF` (`set_termios`, `Flush::Input` for
  `TCSETSF`; neither waits for the master to read, a deviation where Linux
  waits for output to drain); `TCFLSH` (`TCIFLUSH` `flush_input`,
  `TCOFLUSH` empties `to_master`, `TCIOFLUSH` both, another argument
  `-EINVAL`; the census's one call); `TCSBRK` (0, `tcdrain`); `TCXONC`
  (accepted, no effect: `IXON` is outside lumen-tty's scope);
  `TIOCGWINSZ`, `TIOCSWINSZ` (`SIGWINCH` to the foreground group when
  `set_winsize` reports a change); `TIOCGPGRP`, `TIOCSPGRP`, `TIOCGSID`,
  `TIOCSCTTY`, `TIOCNOTTY` (below); `FIONREAD`/`TIOCINQ`
  (`readable_bytes`), `TIOCOUTQ` (the bytes in `to_master` for the slave);
  on the master only, `TIOCGPTN`, `TIOCSPTLCK`, `TIOCGPTLCK`; M7's
  `FIONBIO`/`FIOCLEX`/`FIONCLEX`. `TIOCPKT` is `-EINVAL` (no packet mode,
  a deviation); every other request `-ENOTTY`.
- **Controlling terminals and job control** (C8's tree, lumen-tty's
  checks, Linux's `drivers/tty/tty_jobctrl.c`):
  - a session leader with no controlling terminal that opens a tty without
    `O_NOCTTY`, when the tty has no session, takes it (its session and
    foreground group become the leader's), except a PTY master and
    `/dev/console`, which never become one (Linux's `tty_open`), and only
    through a descriptor open for reading (`tty_open_proc_set_tty`'s
    `FMODE_READ`). `TIOCSCTTY` does the same explicitly (Linux's
    `tiocsctty`): 0 at once if the caller already leads the tty's session;
    `-EPERM` unless the caller leads its session and has no controlling
    terminal, or if the tty is another session's (an argument of 1 takes it
    away, as root may); its "not open for reading" `-EPERM` is waived for
    `CAP_SYS_ADMIN`, which every Lumen process has;
  - `TIOCNOTTY` on the caller's controlling terminal drops it (a session
    leader's also sends `SIGHUP` then `SIGCONT` to the foreground group, as
    `disassociate_ctty(0)`), else `-ENOTTY`;
  - `TIOCGPGRP` and `TIOCGSID` on a slave (or the console) that isn't the
    caller's controlling terminal are `-ENOTTY`, and `TIOCGSID` on a tty
    with no session is `-ENOTTY` too (Linux's `tiocgsid`); on a master they report
    the slave's foreground group and session to any caller, as Linux's
    `tiocgpgrp`/`tiocgsid` check only `tty == real_tty`; `TIOCSPGRP` is
    `ProcTree::tcsetpgrp` with the
    tty's `Controlling` and the caller's `SIGTTOU` disposition, taken
    under `Tty::lock` then `PROCS` (`Tcsetpgrp::Sigttou`: `SIGTTOU` to the
    caller's group and `-ERESTARTSYS`);
  - a read by a background process on its controlling terminal: `-EIO` if
    `SIGTTIN` is blocked or ignored or its group is orphaned, else
    `SIGTTIN` to its group and `-ERESTARTSYS` (Linux's
    `__tty_check_change(tty, SIGTTIN)`); a background write with `TOSTOP`,
    and a background `TCSETS*` whatever `TOSTOP` says,
    `background_write_signal`'s answer (`SIGTTOU` and `-ERESTARTSYS`,
    `-EIO`, or go ahead).
- Tested by `cargo:lumen-tty`, `scenario:test_linux_io::test_pty_termios_and_job_control`
  and D3's `scenario:test_alpine::test_vim_edits_a_file_over_pty`, which
  runs vim under `tools/linux-m8/ptyrun` (section 15).

### The console as a tty (C10)

- Each console (the serial line, each virtual terminal) gets a `Tty`
  whose output goes to that console (the serial writer, the VT's
  terminal) and whose termios starts as `Termios::console_default()` (M6's
  `default_termios`). The kernel shell keeps the console while no Linux
  process holds it.
- `linux /path` makes its process a session leader (its pid its pgid and
  sid, at `spawn_root`) with the console's `Tty` as its controlling
  terminal and its group in the foreground, and starts the console's input
  thread (`tty-serial`, `tty-vt1` ... `tty-vt4`), which feeds the
  console's input (`serial_in`'s queue, the VT's key ring) through
  `LineDiscipline::receive` (leaving refused bytes in the queue until a
  reader wakes it), echoing to the console and sending Ctrl-C's `SIGINT`,
  Ctrl-\'s `SIGQUIT` and Ctrl-Z's `SIGTSTP` to the foreground group. The
  `linux` command, which waits for its root's `ExitAction::Released`,
  stops the thread once the root (the session's leader) has exited (a
  stop flag and a wake of the thread's input wait), and the kernel shell
  has the console back.
- `/dev/tty` is the caller's controlling terminal (`-ENXIO` without one,
  as Linux); `/dev/console` is the console the `linux` command ran on.
  M7's `ConsoleIo` routing (`VfsInode::is_tty`) becomes the `Tty`'s;
  `usertest`'s capture mode keeps M7's path.
- Tested by `scenario:test_linux_io::test_console_ash_job_control`
  (section 15).

## 11. GUI and the automation API (built in M4: designed in #146, built by #147 to #157)

The desktop compositor and built-in toolkit windows run in the kernel on kernel
threads (`kernel/src/desktop.rs`, `kernel/src/ui.rs`), and M5 WASM apps render
into windows through the `lumen` host interface. In M11, M4's terminal windows
(`kernel/src/termwin.rs`) attach to real `/dev/ptmx` PTYs so Linux shells and
tools (`bash`, `vim`, `cargo`, `node`) run directly inside Lumen windows; an
optional Wayland bridge (`wl_shm`, then `virtio-gpu` 3D / Mesa) in M12 allows
unmodified Linux GUI apps to render through the same compositor. Each heading
below names the issue that built M4.

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
  | `windows` | windows open (not the panels: the taskbar, #157) |
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
  | `taskbar` | `yes` if the taskbar is open (#157; each `desktop` start opens it if not) |
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
  flag), its owner has been told of the last theme switch, no other
  thread marks it busy (`desktop::Waker::set_busy`: a terminal window's
  shell running a command, #155) or has woken it without its owner having
  looked yet (`Waker::wake`), and it is blocked in `wait_event` or a
  `wait_event_until` that isn't animating (an idle bit per window, read
  after the others, so a look never takes an owner mid-redraw for a
  waiting one; an animating wait, `desktop::Animating::Yes`, such as a
  focused terminal window's cursor blink, never sets it); and
  no `ui invoke` request is outstanding (from its push to its asker's
  end, answered or not; the `desktop check`/`screenshot` slot doesn't
  count). The taskbar (#157) is a window like the rest: its clock polls
  the RTC each second without animating, so a read can catch its owner
  between two waits (`settled no` for that moment), and a new minute
  presents one rectangle, the clock's; an open or a close pokes it under
  the desktop's lock, so `settled` waits for its buttons to catch up.
  A test first polls for the effect it expects (a `windows` row,
  a `ui tree` value), then calls the harness's `wait_settled(m)`
  (`tools/harness/lumen_harness/gui.py`, #153), and brackets each
  screendump between two `desktop status` reads with the same `frames`.
  Windows that animate (a blinking cursor, `desktop spin`, the running
  Life app) never idle: tests close them first, mask them, (a terminal
  window) unfocus them or stop them (the Life app's `--stopped` and Stop), and their
  goldens leave them out. A scenario that only checks state ends with `desktop check`
  reporting 0 and 0.

### Windows and who draws (#153)

- **Opening:** `desktop::open_window(title, w, h) -> Result<WindowHandle,
  OpenError>`. `OpenError::{NoDesktop, TooMany, TooBig, OutOfMemory}`: at
  most `MAX_WINDOWS` (32) windows (the taskbar takes one, #157); a client
  no bigger than the screen; the
  surfaces allocated with `try_reserve`, so a failed allocation is an
  error, not a panic. Window ids count from 1, never reused in a boot.
  An open or a close pokes the taskbar's owner (#157).
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
- **Lent bands (#157):** the Mandelbrot window draws with every CPU:
  `h.surface_mut().bands_mut(rows)` lends its surface as disjoint row
  bands (`lumen_gfx::BandMut`: `row_mut`, `impl lumen_gfx::Target`), and
  `sched::scope` joins every thread it started before returning
  (`run_sized`'s argument applied to several threads); see "The desktop
  shell". Because the bands borrow the handle, as `surface_mut` and
  `commit` take `&mut self`, the compiler refuses a commit while bands
  are lent. A minimized window's commits queue no damage (#157).
- **Who may draw where:** an owner draws only into its own surface (its
  client area); the frame (title bar, border, close and minimize boxes)
  is drawn by the compositor from `lumen-wm`'s geometry and the theme. The
  compositor alone writes the back buffer and, while the desktop is shown,
  the screen.

### Window events (#153)

`WindowHandle::next_event()` (non-blocking), `wait_event()` (which sets
the window's idle bit while it blocks until the owner has an event:
`has_event()`, below; built on the next) and `wait_event_until(deadline,
animating) -> desktop::Wait` (#155: `Event` with an event; `Woken` when a
`desktop::Waker` of the window, from `WindowHandle::waker()`, woke it from
another thread, such as a terminal window's shell with output to draw,
meaning only "look at your own state again"; `Deadline` once `deadline`,
`time::now_ns`, passed; `#[must_use]`; idle while it waits unless
`animating`, `desktop::Animating::Yes`, which needs a deadline: the
`settled` rule above. `wait_event` takes a `Waker`'s pokes and ignores
them, so an owner that waits there hands none out) return
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
  owners' redraws too. Only windows hear a switch, so #157's taskbar is
  a window (a panel).

- **Requests are never dropped under a desktop lock** (a dropped one
  answers and wakes its asker): `EventQueue::push_request` hands a refused
  request back (`lumen_wm::Refused::Busy` past the ration, `Refused::Dead`
  once the queue is dead), `desktop::push_request` does the same for a
  window no longer on the list and, #207, `Refused::Busy` for an app's
  window whose one request slot is held (section 8; `ui invoke: a request
  is still queued or running in window <id>`), and `EventQueue::kill` returns the
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
Moves with no button held aren't delivered (no hover in M4). A left
press on the minimize box and its left release over it minimize the
window (#157). A panel (#157, the taskbar) is frameless, docked along the
bottom, in front of every other window, never focused, raised or moved; a
press on it is a grab without a focus change, which a pause or a pointer
gap ends (the panel told `Lost`) and a key gap leaves alone (a panel has
no keys); "the front window" in the focus rules is the front one that is
neither a panel nor minimized.

- **API:** `Wm::new(w, h, Metrics::DESKTOP)`; `open` (in front of the
  other windows, behind the panels, focused,
  clamped like a move, by the frame's whole width), `close`, `focus`
  (raise and focus: the taskbar, `ui invoke <root> focus`; it restores
  a minimized window), `move_to`, `minimize` and `restore` (#157; each
  `None` for a window not open, a panel, or not in that state),
  `open_panel(title, rows)` (#157: docked at the bottom, the screen's
  width; every window clamped above it, their damage returned),
  `pointer(Pointer)`,
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
  16-pixel close box 4 pixels in from the right border, the minimize box
  (#157) as big, 4 pixels left of it; a moved frame
  keeps its title bar's rows on the screen (above the panels' rows) and at
  least 48 pixels of its width (all of it, if narrower).

A model check runs random operations (opening, closing, focusing,
moving, minimizing and restoring, pointer events and clicks aimed at
title bars, close and minimize boxes, clients and, on half its seeds, a
panel along the bottom, pauses, pointer and key gaps, `after_overrun`)
and checks after each: repainting
only the reported damage equals repainting everything (painter's
algorithm, each pixel knowing its window, part and whether that window is
focused; minimized windows not drawn, the panel frameless); the focused
window is the front one neither minimized nor a panel, and holds any grab
but the panel's (a press on the panel grabs without focusing); the panel
stays in front; hit testing agrees with the picture; a drag follows the
pointer, clamped, and only while the left button is held; a first press
on a client is that window's (unless its event follows an overrun: then
it went down in the frames lost); and, through each window's
`EventQueue`, only the grabbing window believes buttons are held, and no
stray release reaches a queue.
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
| a window's lock | its committed surface, its event queue, its flags, its toolkit's published tree | 16 rows of a commit or a blit, one queue operation, putting a tree in place, copying a tree for `ui tree` or scanning it for a node |
| a `ui invoke` reply's lock (`ui::ReplyCell`, #156) | the reply's state | one transition (a leaf: taken under its wait queue's lock by the asker's wake-up check, and never held across a wake) |
| `vt::CUT` | the cut | a read or a write (a leaf) |
| `termwin::OPEN` (#155) | the terminal windows open, weakly, for `term text <window>` | a push or a look-up (a leaf) |
| a terminal window's grid (#155) | its `Terminal` | a write of the shell's output, a drawing of what changed, a copy of its cells (a leaf: nothing taken or allocated under it) |
| a terminal window's keys (#155) | the keys for its shell (at most 128) | a push or a pop (a leaf) |
| a terminal window's `key_wait` | the shell waiting for a key | a wait or a wake (its predicate takes the keys' lock) |
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

`terminal` (from any shell, while the desktop is shown; refused with
`desktop: paused (`desktop` resumes it)` or `desktop: not started` otherwise)
opens one: the M2 terminal and a shell of its own in a desktop window,
titled `Terminal-<n>` (`n` counting the ones opened in the boot), an 80x24
grid with 100 lines of scrollback, placed side by side (left, then right, a
step lower each pair: `desktop::open_window_at`). `kernel/src/termwin.rs`.

- **One renderer.** `lumen_term::render::Renderer` (host-tested) draws a
  `Terminal` on any `CellTarget`, only the cells that changed (the shadow
  diff), a scroll moving what was drawn, the cursor solid or hollow
  (`CursorLook`) and never while the view is scrolled back. `term.rs`'s
  `term::Cells` gives a cell its pixels (colours, the console font through
  `fb::glyph_pixels`, the hollow cursor's outline) over `term::Pixels`: the
  framebuffer for the text console (which renders exactly as before: its
  goldens are unchanged, and `term::scrolled_pixels_match_a_full_redraw`
  guards its scrolls), a window's surface for a terminal window
  (`lumen_gfx::Surface::move_rows` scrolls it). A window's cells look as
  the console's, pixel for pixel, in the console's colours (`fb::TEXT` on
  `fb::BG`) whatever the theme: its owner takes `WindowEvent::Theme` and
  draws nothing for it.
- **Two threads per window.** Its owner, `terminal`, holds the
  `WindowHandle`, draws the grid onto the window's surface and commits
  what changed; its shell, `term-shell`, runs the screen shells' line
  editor and commands on the window's console (`shell::run_in_window`,
  `term::Place::Window`). The `Terminal` is behind a lock of its own (a
  leaf: the shell's output feeds it, the owner draws it, `term text
  <window>` copies its cells out into a vector allocated first), and so
  are the keys for the shell, at most 128 (past that a key is dropped,
  counted, and the window's first drop logged as `WARN termwin: terminal
  window <id>: its shell isn't reading: dropping keys`). The shell's output
  wakes the owner through a `desktop::Waker`, which holds the window
  weakly.
- **Keys** are the window's key presses (the compositor's, to the focused
  window) through `lumen_shell::Key::from_press` (section 7);
  Shift+PageUp/PageDown scroll the window's view by half a screen, as on a
  virtual terminal (not tested), and any other key the line editor takes
  brings it back.
- **Busy, one writer at a time:** the owner sets its window busy
  (`Waker::live` then `set_busy`, under the keys' lock) as it queues a key
  for the shell; the shell clears it under that lock once it finds the
  queue empty, waiting for a key; so "a key queued or the shell running"
  and "busy" agree at every instant, and `settled` reads busy after the
  idle bit.
- **The cursor and settling:** only the focused terminal window's cursor
  blinks (every 500 ms; output shows it solid at once), its owner waiting
  with the blink as its deadline in an animating wait (`wait_event_until
  (deadline, Animating::Yes)`), so a focused terminal window never
  settles; an unfocused one's is drawn hollow and steady, its owner
  waiting with no deadline, idle. The shell marks its window busy except
  while it waits for a key with none queued, so a command running in an
  unfocused window (`top`) keeps `settled` at no, and an unfocused window
  whose shell waits at its prompt settles.
- **No canvas.** A command in a terminal window gets none, also at the head
  of a pipeline (`term::Place::Window`, which every console says and a
  pipeline's capture passes on; `CanvasError::InWindow`: "a terminal window
  has no canvas: run it on a virtual terminal (Alt+F1)"), so `mandelbrot`,
  `life` and a script's drawing refuse there with that line, and
  `screenshot` refuses as on a virtual terminal; `top` writes text, and
  runs. (The demos get windows of their own with #157.)
- **Closing** (the close box): the owner closes the shell's console (from
  then on every key it asks for is `lumen_shell::Key::Closed`, never typed:
  the line editor ends the line with `Event::Closed`; the shell, `rhai`'s
  REPL, `top`, `play` (not tested: it needs the sound device) and a
  script's Ctrl-C check stop on it, `Key::interrupts()` being Ctrl-C or
  `Closed`, where Ctrl-C would only restart a prompt;
  `shell::Console::closed`), drops its `WindowHandle` (the window goes at
  once, its surfaces freed with it), joins the shell, frees the terminal
  and ends. A command that never asks for a key keeps the shell, its grid
  and the owner's join until it returns (not tested).
- `term text <window>` prints a terminal window's rows as `term text` does
  the screen's (the harness's `screen.read_window`/`expect_window`).
- **Requests:** a terminal window has no toolkit and publishes no nodes, so
  `ui invoke` reaches only its root (the compositor answers `focus`;
  `close` comes as `WindowEvent::Close`); a request that still reaches its
  owner is answered "no node" (`ui::refuse`, as the demo windows do),
  never dropped.
- Not yet: the mouse in a terminal window (pointer events are ignored),
  theme colours, resizing. Not tested: a failed spawn of either thread.

Tests: `scenario:test_gui::test_terminal_window_runs_shell` (two windows,
focus by clicking; `echo hello` typed lands in the second only, by `term
text <window>` and by its pixels; `life` refused there; the focused
cursor's both looks), `scenario:test_gui::test_terminal_windows_settle_only_when_idle`
(unfocused and idle: settled, nothing presented for over a second, the
cursor hollow; focused, or running `top`: never settled),
`scenario:test_gui::test_closing_a_terminal_window_frees_it` (four windows
opened and closed: both threads gone each time, the heap back within 2
KiB, `desktop check` 0 and 0),
`scenario:test_gui::test_closing_a_terminal_window_ends_what_runs_in_it`
(`rhai`'s REPL, `top`, a script's loop: every thread gone),
`scenario:test_gui::test_a_terminal_window_scrolls`,
`scenario:test_gui::test_terminal_is_refused_unless_the_desktop_is_shown`,
`scenario:test_gui::test_a_terminal_window_refuses_screenshot_and_keeps_its_colours`
(and a theme switch), `cargo:lumen-term` (the renderer), `cargo:lumen-gfx`
(`Surface::move_rows`), `cargo:lumen-shell` (`Key::Closed`,
`Key::interrupts`), `harness:test_screen_text::*`,
`ktest:lumen_kernel::termwin::*`,
`ktest:lumen_kernel::desktop::wait_event_until_ends_at_its_deadline_or_a_wake`,
`ktest:lumen_kernel::desktop::settled_waits_for_a_busy_or_woken_window`,
`ktest:lumen_kernel::term::a_terminal_window_s_commands_get_no_canvas`,
`ktest:lumen_kernel::shell::pipe::a_pipeline_in_a_terminal_window_gets_no_canvas`,
and the `test_term` goldens with `ktest:lumen_kernel::term::scrolled_pixels_match_a_full_redraw`.

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
  `desktop.rs` included, distinguishing it from VFS inode numbers in
  section 9). `NodeId::fresh()` is one counter
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
  | `window` (a root) | none | `focus`, `close`, `minimize`, `restore` (#157) | `focused` | `focus`, `minimize`, `restore`: the compositor; `close`: the owner (a `Close` event) |
  | `group` (a stack; the taskbar's root, #157) | none | none | `disabled`, `hidden` | |
  | `label` | `Text` | none | `disabled`, `hidden` | |
  | `button` | none | `press`, `focus` | `focused`, `disabled`, `hidden` | the owner |
  | `checkbox` | `Checked` | `press` (toggles), `focus` | `focused`, `disabled`, `hidden` | the owner |
  | `textfield` (a text field; or a text area, `Ui::text_area`, #203: its text drawn and edited by its app, the tree painting only its field, border and ring; its value is what the app sets, which for the editor is a lossy view of its buffer) | `Text` | `set_value <text>` (replaces the text, as one edit, not keystrokes), `focus` | `focused`, `disabled`, `hidden` | the owner |
  | `list` | `Selected` | `focus` | `focused`, `disabled`, `hidden` | the owner |
  | `listitem` | none | `select`, `press` | `selected`, `disabled`, `hidden` | the owner |
  | `menu` | none | `press`, `focus` | `focused`, `disabled`, `expanded`, `hidden` | the owner |
  | `menuitem` | none | `press` | `disabled`, `hidden` | the owner |

  Every widget may carry `hidden` (#157: laid out past a bar's pinned
  widget: no bounds, not hit, and `ui invoke` refuses it, `not shown: it
  doesn't fit`); a window's root never does.

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
  deadline); `Pending` to `Done` (its window closed, or it was dropped,
  before it ran); `Running` to `Done(result)`; `Done` to `Taken` (the
  asker takes it). Each answer (`Done`) wakes the asker. The owner skips a
  cancelled request; the asker counts it (`cancelled`). If the asker's
  cancel fails because the owner already runs it, the asker waits a second
  1 s: a result then is printed with "ran late" above its row or message
  (`late` counts it), and none gives "still running" (its answer, if it comes,
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
  after each change (`WindowHandle::publish`: built by the owner, then
  put in place under the window's lock), so `ui tree` reads it without
  asking the owner. A
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
  `Ui::invalidate()` repaints the whole tree): Counter's first event,
  `Theme`, is its first draw (`desktop spin`'s window draws a frame of its
  bar first, then reads its events). The harness's `ui_tree`, `node`, `wait_ui`,
  `invoke`, `parse_invoke` and `desktop_counter` are in `gui.py`.

**The commands** print records, so the pipelines work on them (`where`
compares with `=`). **Records quote** (every records command, from the
shared printer, `lumen_shell::pipe::quoted`): a text cell that is empty,
is exactly `-`, or holds whitespace, a quote (`'`, `"`), a backslash, a
`#` or a control character prints double-quoted, with `"` and `\` escaped
by a backslash (the only escapes both the shell's splitter and POSIX
`shlex` take inside double quotes) and each control character printed as
U+FFFD (neither has an escape for one). So every row reads back into its
cells, as the shell's words or `shlex` split them (a randomized host test;
the harness's `parse_records` uses `shlex`), and `where name = "File
name"` works; a bare `-` is always an absent value. The cells it quotes
today: `cpus`' "not started", `lspci`'s names, classes and drivers,
`irqs`' uses ("local APIC timer", "msix <addr> #0"), `mouse`'s buttons
("left right"), `sound`'s state, and `kbd events`' text (`"'a'"`); a `-`
meaning none (`kbd events`' mods) is an absent value, unquoted.

- `windows`, one row per window, front to back:

  | Column | Meaning |
  |---|---|
  | `id`, `title` | the window |
  | `x`, `y`, `w`, `h` | its frame, decoration included, in screen pixels |
  | `z` | 0 is the front |
  | `focused`, `minimized` | `yes` or `no` |
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
  invoke: ran; node <n> is gone` (a launcher item's: the menu folds);
  `ui invoke: window <id> is minimized already` and `... isn't
  minimized` and `ui invoke: window <id> is the taskbar` (#157); `ui
  invoke: <why>` (an action that failed: a launcher item's, `<item>:
  <why>`, or a press on a just-closed window's button, `window <id>
  closed`, #157); `ui invoke: not shown: it doesn't fit` (a hidden node,
  #157); `ui invoke: window <id> busy (try
  again)`; `ui invoke: window <id> closed` (a widget's window closed as
  it was asked); `ui invoke: no node <n>` (none, or its window closed,
  from the asker or the owner); `ui invoke: no desktop`; `ui invoke:
  cancelled: window <id> didn't take it in 1 s (try again)`; `ui invoke:
  still running (window <id>)`; `ui invoke: window <id> closed before it
  ran`; `ui invoke: window <id> closed while running it (it may have
  run)` (an app's window, section 8's "Apps": `Answer::OwnerEnded`); `ui invoke: dropped while running (window <id>): a bug`; and the
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
commands are for tests and people. Agents (M11) use the protocol below and
hold an `AppActions` policy grant (section 8; threat model, section 13).
Tests may still drive QEMU's real input devices over QMP (#149's
confirming steps): that path is the user's, and the guest can't tell it
from a person.

**Later (M11):** the same operations are exposed over an `AF_UNIX` automation
socket: `get_tree(window)`, `invoke(node, action, args) -> Result`,
`subscribe(changes)`, and app-level semantic actions (`files.copy(src,
dst)`, `editor.replace(range, text)`) an app may declare beside widget
actions; the `AppActions` capability names the app and the allowed
actions. The same tree powers accessibility (a screen reader) and GUI
tests (golden screenshots plus tree assertions). **X-ray mode** is an app
(and an overlay) that reads the `Trace` capability.

### The desktop shell (#157)

The layer that makes the desktop something to play with: a taskbar along
the bottom (a launcher, a button per window, a clock), minimize beside
close, and the demos in windows of their own. All of it is built with
`lumen-ui`, so it is in the automation tree too.

- **The taskbar is a window, a panel.** `Wm::open_panel(title, rows)`
  opens a frameless window (its frame is its client area: no title bar,
  no border, no close box) docked along the bottom, the screen's width,
  that the window manager keeps in front of every other window, never
  focuses, raises or moves (`focus`, `minimize`, `restore` and `move_to`
  give `None` for it): a press on it is an implicit grab without a focus
  change (so clicking a taskbar button doesn't take the focus from the
  window it is about), ended with `Lost` by a pause or a pointer gap and
  left alone by a key gap; keys never go to it. It reserves its rows:
  windows already open are clamped above them as it opens, and windows
  are opened and moved (clamped) so their title bar stays above it. Being a window, it gets what every
  window gets: its first event and every switch's `Theme`, requests
  (`ui invoke` on its buttons), its tree published, `settled`'s
  conditions, `desktop check`. The desktop's first start opens it, after
  the demo windows, and each later start if there is none (a failure is
  warned about; `desktop status`'s `taskbar` says whether it is open, and
  `desktop tick` fails with `desktop tick: no taskbar`): as high as a
  button plus 4
  pixels above and below (40 rows with the UI font), across the
  screen's whole width, at its bottom, titled `Taskbar`. `windows`
  doesn't list it (it is the desktop's, like the frames) and `desktop
  status`'s `windows` doesn't count it; `ui tree` lists it, its root a
  `group` named `Taskbar` (no actions) under its own window number. It
  takes one of the `MAX_WINDOWS` (32) slots. A `Desktop::without_screen`
  (the ktests' desktop) has none.
- **What is on it** (left to right; the taskbar's colours, `taskbar` and
  `taskbar_text`, through the toolkit's palette choice):
  - a `menu` named `Launcher`: `press` (or a click) toggles it
    (`expanded`); expanded, its `menuitem`s follow it: `Terminal`,
    `Mandelbrot`, `Life` and `Counter`, each opening that window (`Life`
    starting the Life app since #202, section 8) and folding the menu
    again. Each is refused while the desktop is paused,
    as `terminal` is; a launch that fails is warned about, and a `ui
    invoke` that chose it prints `ui invoke: <item>: <why>` and fails
    (`lumen_ui::Then::Failed`, answered `Served::Failed`), e.g. `ui
    invoke: Mandelbrot: desktop: paused (`desktop` resumes it)`;
  - one `button` per window (named by its title, in the order they were
    opened): a click or `press` restores it if minimized, then raises and
    focuses it (a window closed meanwhile: its button goes, before the
    press is answered); a button that doesn't fit before the clock is
    hidden (listed `hidden`, refused by `ui invoke`: `not shown: it doesn't
    fit`);
  - at its right end a `label` named `Clock`, of a fixed size (its
    neighbours never move when it changes), showing the RTC's time as
    `HH:MM`.
- **Its owner**, the `taskbar` thread, waits with
  `wait_event_until(the next second, Animating::No)` (so it settles
  between polls). The desktop wakes it through its `Waker` when its
  windows change (one opened or closed), and it brings its buttons in
  line (and reads the clock): a button for a window gone is removed from
  its tree (its node id names nothing after), one for a new window added.
  A launcher item opening a kernel window (Terminal, Mandelbrot, Counter)
  brings them in line itself, before its request is answered, so `ui
  tree` after `ui invoke` shows the new window's button
  (`lumen.taskbar_wake_delay_ms=<ms>` holds back its handling of a poke,
  so a test can tell the two apart). An item starting an app (Life since
  #202, the bundled apps since #206) is answered once the app is started,
  without waiting for its window: the app's thread opens it, and its
  button comes with it, before or after the answer (`INFO  taskbar:
  <item>: app <id>` logs the start).
- **The clock** polls the RTC without animating (the desktop settles
  between polls; `rtc::now_quiet`) and redraws the label when the
  minute changes, damaging only its bounds (the toolkit's per-widget
  damage: a minute's change adds the label's rectangle to `desktop
  status`'s `rects` and its pixels to `pixels`, a test checks).
  `desktop tick` (for the tests) moves the clock's minute ahead by one now
  (a skew on its readings from then on), so a test needn't wait for a
  real minute. A failed read keeps the last good reading (`--:--` with
  none, or after 3 failures in a row), warned about once when reads start
  failing and said (INFO) when they recover. Desktop goldens mask its
  bounds (from `ui tree`); `test_taskbar_clock_matches_rtc` (no skew)
  polls until the label and `date` agree.
- **Minimize:** a minimize box (`Metrics::minimize_box`: the close box's
  size, `close_margin` left of it, drawn as a bar) beside the close box. A
  left press on it and its left release over it minimize the window
  (`Wm::minimize`): it leaves the screen (not drawn, not hit), keeps its
  place and size, and loses the focus (the front window that isn't
  minimized gets it). `Wm::restore` brings it back raised and focused, and
  `Wm::focus` on a minimized window restores it. `windows` shows it
  (`minimized yes`); its root takes `minimize` and `restore` (answered by
  the compositor, printing the root's row; `ui invoke: window <id> is
  minimized already` and `ui invoke: window <id> isn't minimized` when
  there is nothing to do). A minimized window's commits queue no damage
  (the Life app presents nothing while minimized).
- **`lumen-ui` for the shell:** `Stack`, the handle stacks are (only a
  stack takes children); `Ui::remove` (a widget and all in it; its node
  ids name nothing after, focus and a press on it dropped, what it covered
  repainted); `Widget` is a slot and its generation: a freed slot is taken
  again by a later widget under a new generation and a new node id (so
  the taskbar's tree doesn't grow with every window opened, and an old
  handle reads removed: `id` is `None`, its setters do nothing), the
  layout sizing in reversed tree order rather than by slot; `menu` (its
  items are sub-rows with node ids of their own, as a list's are, listed
  only while it is expanded); `fixed_label`; `Ui::bar(pad)` (top widgets
  across, centred in the bar's height, one `pin_right`: a top widget of a
  bar only; past it a leaf is hidden, its bounds empty, listed with the
  `hidden` state flag, not hit and refused by `invoke`, a hidden menu
  folded; a stack is cut to what shows of it); per-widget damage
  (`take_damage`: `Nothing`, `All` after a move or a new palette, or the
  rectangles of the widgets whose look changed, focus rings included;
  `paint_in(clip)` paints exactly what `paint` does there; a randomized
  host test repaints only the damage against a full repaint). The kernel's
  `Toolkit::refresh` commits only those rectangles, and the app's own
  (`take_drawn`; none for the kernel's apps); `Toolkit::frame` (an
  animation) commits everything; the palette is made from each theme by
  the function handed to `Toolkit::with_palette(.., choose)`
  (`ui::palette` for `Toolkit::new`, `ui::taskbar_palette` for the
  taskbar).
- **The demos in windows:** the launcher's `Mandelbrot` opens a 480x320
  window drawn once by every CPU (`sched::scope` and
  `Surface::bands_mut`, below; its rows by CPU logged as `INFO
  demo: mandelbrot window <id>: <n> CPUs in <ms> ms, rows by CPU: cpu0=<rows>
  ...`), then waiting for its close, which stops it. `Life` was a
  kernel window stepping the `gun` pattern until #202 made it an app
  (section 8, "What #202 adds"). `mandelbrot` and `life` typed in a shell
  are as before (a canvas; refused in a terminal window).
- **Lent bands:** `lumen_gfx::Surface::bands_mut(rows)` splits a surface
  into disjoint bands of `rows` rows (`BandMut`: `row_mut`, `impl
  Target`), on the owner's surface (`h.surface_mut().bands_mut`), and
  `sched::scope(|s| ..)` runs threads (`Scope::spawn_on(cpu, name, f)`,
  `f` borrowing from the caller) and joins every one before it returns.
  The Mandelbrot window deals the bands round-robin to the CPUs whose
  scheduler runs (`demo::draw_bands` takes the list; the caller draws
  slot 0, and the slot of a CPU whose thread couldn't be spawned, warned
  about), then commits the whole surface once they
  are joined; because the bands borrow the handle, no commit can happen
  while one is lent.

### Screenshot (#150)

`screenshot` on the serial shell sends the screen as a PNG over serial (to
disk once there are files, M7). Capture and encoding are separate steps
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

## 12. The Claude service (planned, M11 — userspace)

Rather than implementing TLS and HTTP in ring 0 with soft-float crypto, the
Claude service runs as a native ring-3 userspace service once the Alpine
networking and runtime stack is up (M9–M10):

- **Keychain service:** holds the API key encrypted on disk; never in source,
  logs, crash reports, or agent memory. Only the keychain and API proxy process
  can read it.
- **API client:** native userspace HTTPS (TLS 1.3 via OpenSSL/rustls with the
  system CA store) to the Anthropic Messages API, streaming responses.
- **Ask Claude bar:** a system-wide input on the desktop (section 11) and `ask`
  in the shell.
- **Agent runtime and lanes:** each agent session runs inside its own
  capability-scoped Linux sandbox (per-process VFS mount namespace + network
  allowlist + OverlayFS/CoW snapshot chain + audit-log stream), with live
  status, pause/resume, per-step undo, and "Undo all".
- **Capability prompts:** when an agent needs a grant it lacks, the system
  renders a readable prompt outside the agent's sandbox ("Allow *Tidy
  Downloads* to move files inside `~/Downloads` for this session?") and can
  grant it narrowed, time-limited, and revocable.

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
- **Least privilege by construction (M11):** an agent process starts in an
  isolated VFS mount namespace and network policy with *no* grants except its
  control socket to the agent runtime. Each grant is narrow (a mounted subtree,
  one host allowlist entry, one app's named automation actions), time-boxed to
  the session, and revocable.
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
  handles (not the timer, spurious interrupts or local APIC errors; I/O
  APIC and MSI interrupts a handler takes go to the X-ray rings, below), log
  lines, thread switches (`switch from=<id> to=<id> (<state>)`, the
  previous thread's state; an idle boot CPU records about 20 a second,
  the serial shell's 100 ms safety-net wakes: section 4, step 4). Later (M11):
  Linux syscalls, `#PF`/CoW faults, `futex` waits, and cross-CPU thread
  migrations in X-ray. Cost per event is printed by
  `ktest:lumen_kernel::trace::trace_overhead_is_small` (tens of TSC ticks
  under TCG locally; TCG's TSC rate depends on the host, so this is not
  comparable across machines or to cycles). Timestamps are raw TSC ticks
  (the boot timeline converts its phases; the shell and crash reports show them
  raw). The shell's `trace [n]` prints the newest events of this CPU's ring (the
  serial shell runs on the boot CPU)
  (a gap line says why an event is missing), and the serial crash report
  includes the last 16 of the reporting CPU's (tested for a page fault and a
  panic, and on an AP).
- **X-ray rings (built, #198):** what X-ray shows goes into a second set of
  rings, one 4096-event ring per CPU of the same kind
  (`trace::xray_ring`), never into the main ones, so however much the
  scheduler, the devices and the locks record, the boot timeline and the
  crash report's last 16 events read what they did
  (`ktest:lumen_kernel::trace::xray_events_leave_the_main_ring_alone`
  records each kind for real, fills a ring two laps over, and scans the
  main rings). They are always on: nothing in them comes at the timer's
  rate (a thread yielding with nothing else ready records nothing), so,
  as estimates (not tested), an idle AP records about none, the boot CPU
  a few dozen events a second, and a busy CPU a few hundred (two per
  yield for a thread polling by yielding while another is ready), plus
  one per device interrupt and lock wait; a reader can start at any
  moment with the history the ring still holds.

  Which ring each kind goes to (numbers, layouts and `stream_of` in
  `lumen_trace::kind`, shared with apps and host-tested; every event's
  `pid` is 0, the kernel's):

  | Kind | Ring | Recorded |
  |---|---|---|
  | `boot-phase`, `exception`, `log`, `panic`, `switch` (`Switch`: from, to, the previous thread's `ThreadState`) | main, the recording CPU's | as above |
  | `runq` (`RunQueue`: queue's CPU, length after, thread, enqueue or dequeue) | X-ray, *the queue's* CPU's | under that queue's lock, by whichever CPU changed it (a wake from another CPU queues there): the ring holds its queue's changes in order, each length one step from the last, and its newest is the queue's length while the lock is held (`sched::run_queue`) |
  | `irq` (`Irq`: vector, interrupted rip) | X-ray, the CPU that took it | for each I/O APIC input or MSI vector a handler takes (`idt::handled`); the timer's ticks are counts (`cpus`, `ticks`), not events |
  | `lock` (`LockWait`: address, looks that found it held, TSC ticks waited, the `lock` call's line, no file) | X-ray, the waiting CPU's | holding the lock, out of line, taking no lock, allocating and logging nothing, so it can't recurse; an uncontended `lock` records nothing |

  The kernel's recorders are typed (`trace::record_run_queue`,
  `record_irq`, `record_lock_wait`, `record_switch`), so the routing
  above holds by construction: `ktrace!` refuses an X-ray kind at compile
  time (`Kind::is_xray`), and the main ring's `record_at` asserts it in
  debug builds. The reader is `trace::read(stream, cpu, since, out) ->
  Result<Since, ReadError>`, either ring (`Stream::Main`, `Stream::Xray`)
  of an online CPU (`trace::cpus()` is one past the highest online index;
  indices can have holes), over `TraceRing::read_since` (host-tested): the events
  from sequence number `since` on, oldest first, as many as fit, and
  `Since { copied, missed, next }`, in which every number from `since` to
  `next` is copied or counted missed exactly once. Missed means out of
  the ring already (more than a lap behind its head: a first read from 0
  of a wrapped ring counts every number it lost), overwritten, or never
  written. An event not readable yet (being written, or its number
  claimed but not its slot) ends the read there and the next read gets
  it; one that never lands (dropped because its slot was busy or held a
  newer event, or its writer stopped mid-write) can't be told apart from
  that until the head passes its number by the ring's size (it then
  leaves the window), and is counted missed then, so a real drop holds a
  reader there for a lap. A CPU
  that isn't online and a `since` past the ring's next number are
  refused. That is the shape of the `Trace` capability's read (section 8;
  its host functions came in #199), which X-ray (#208) polls with the
  last `next`. The serial
  shell's `trace xray <cpu> [since]` prints an X-ray ring that way;
  `trace runq` each queue's length both ways (`MISMATCH` if they differ);
  `trace runq park <n>` holds the shell's CPU still (its interrupts off:
  no tick, its tick count shown unchanged) while `n` threads wait on its
  queue, for `scenario:test_xray::test_run_queue_matches_trace`;
  `trace runq spin <n>` starts `n` threads spinning on the last CPU with
  a scheduler (interrupts on: the timer takes turns among them, so its
  queue stays `n - 1` long apart from what else wakes there) until `trace
  runq spin stop` joins them, one set in the slot at a time, for
  `scenario:test_xray::test_xray_matches_the_scheduler` and
  `test_xray_pause_stops_its_reads` (#208).
- **Boot timeline:** boot phases emit trace events, printed as a timeline at
  boot complete (section 4). The fast-boot target is under 1 s from kernel
  entry to the shell, measured in the guest from TSC timestamps (never by
  host wall-clock time in tests). About 140 ms under TCG at M1, most of
  it calibrating the TSC and the timer.
- **Logger:** `log` facade with level and subsystem tags, and on serial a
  timestamp since kernel entry (built). Every emitted log line also records
  a `Log` trace event (level, tag).
- **Sampling profiler (built, #205):** `perf top [n] [--samples N]
  [--threads]`.
  - **Sampling.** While a profile runs, each CPU's local APIC timer
    interrupt records what it interrupted into that CPU's sample buffer:
    the instruction pointer, the running thread's id, and whether that was
    the CPU's idle thread (read lock-free, `sched::running_id_here`). The
    buffer (`lumen_perf::Buffer`) has one writer, the CPU's own interrupt.
    It is lock-free, never waits, allocates nothing, is armed for a count
    and refuses samples past it. Each arming has a new generation in the
    buffer's state word, so a record raced by a re-arm never lands in the
    later arming. Otherwise the tick pays a call and one atomic load
    (`perf::on_tick`).
  - **Counts, not time.** A profile is a number of samples per CPU on
    every CPU whose scheduler runs (default 1000, at most 2048; `perf top`
    refuses more). It is never a duration, so a loaded host makes it
    slower, not smaller. `q` or Ctrl-C stops it early, and the header then
    says how many each CPU took. One profile runs at a time.
  - **Silent CPUs.** A CPU that takes no tick across 100 polls in a row
    while another CPU ticks (`lumen_perf::Silence`: about a second of the
    other CPUs' polls) is reported once: "cpu N has stopped taking samples
    (interrupts off?); q stops". It is marked `(silent)` in a stopped
    profile's header. This is a hint for a person, not a verdict: a host
    that stalls one vCPU that long makes a healthy CPU look silent, so the
    profile keeps waiting and the ktests don't fail on it.
  - **Ranking.** The busy samples are ranked by function or, with
    `--threads`, by thread (name and id; `-` for a thread the scheduler no
    longer has a record of). Idle samples are counted in the header and in
    no row; they include `smp::run_on` jobs, which an AP runs on its idle
    thread. The share column is a row's part of the busy samples.
  - **Symbols.** Functions are found through a sorted index of the
    kernel's function symbols (`lumen_perf::FnIndex`, binary search). It is
    built for each ranking from the symbol table, several thousand
    functions, and gets one lookup per distinct address. An address no
    symbol covers is its own `<unknown 0x…>` row. Without a symbol table
    every row is one, and a note line says so.
  - **Not in pipelines.** `perf top` refuses to run in a pipeline: it
    waits for its samples, and its header makes the table one the filters
    can't read.
  - **`perf burn`.** `perf burn [cpu]` spins a thread (`perf-burn`) in
    `perf::burn_loop` on a CPU (the last one by default) until `perf burn
    stop`, something known to profile. Each burn has its own stop flag.
    A stopping burn keeps its slot and its thread's handle until a stop
    sees the thread finished (`JoinHandle::is_finished`), so a start
    meanwhile is refused. Both waits stop on `q` or Ctrl-C and leave a
    burn that can still be stopped: the wait for the thread to be in its
    loop, and the wait for it to end. Neither blocks on a CPU that never
    runs the thread.
  - **Limits.**
    - A sample is wherever the tick landed. Code that runs with
      interrupts off is charged to the instruction after it turns them
      back on (often `IrqOff::drop`, or the scheduler function that did).
      Ticks don't queue, so a long stretch with interrupts off counts
      once.
    - Nothing unwinds, so a function's callers aren't credited.
    - WASM apps (#199) show up by function as `wasmi`'s interpreter, not as
      their own functions; by thread they show up by their thread's name.

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
| Linux ABI conformance (M7+) | upstream `musl` `libc-test` (M8), gVisor syscall suites (M9–M10), and targeted LTP cases plus unmodified Alpine `ext4` runtime tests | `tests/scenarios/test_linux_*`, `test_alpine.py`, `test_conformance.py`, `test_runtimes.py` |

**CI tiers, `status.py` feature gate, and deterministic disk images (M7–M10, #230, #246).**
Starting in M7, Linux-userspace tests run against a deterministic, pre-baked
Alpine Linux `x86_64` (`musl`) `ext4` disk image assembled on the host under
untracked `/build/` (never committed to the MIT repo)—CI scenarios never depend
on live external package mirrors; M9's `apk`/HTTPS test talks to a local host
test server:

- **Pinned Alpine minirootfs (`tools/fetch-deps.sh`, `make deps`):** a
  dedicated `fetch_alpine_minirootfs` helper downloads an exact sha256-pinned
  `alpine-minirootfs-<version>-x86_64.tar.gz` into
  `build/deps/alpine-minirootfs/` (cached by `ci.yml`'s `build/deps` cache
  keyed on `tools/fetch-deps.sh` and preserved by `make clean`). Because the
  tarball has no single top-level wrapper directory and contains `/dev/*`
  device nodes (which fail non-root `mknod`), extraction runs **without**
  `--strip-components=1`, excludes device nodes (`--exclude='dev/*'` /
  `--exclude='./dev/*'`), passes `--no-same-owner` (and avoids macOS resource
  forks/xattrs via `COPYFILE_DISABLE=1`), and ensures empty `dev/`, `proc/`,
  and `tmp/` mountpoint directories. It supplies unmodified
  `/lib/ld-musl-x86_64.so.1`, dynamically linked `/bin/busybox`, standard
  symlinks (`/bin/ls`, `/bin/cp`, `/bin/mv`, `/bin/grep`, `/bin/sed`,
  `/bin/awk`, `/bin/tar`, `/bin/sh`), and `/etc/*`.
- **Host-built M7 dynamic test binaries (`tools/linux-m7/`):** a `#![no_std]` +
  `#![no_main]` + `-C panic=abort` dynamically linked executable (exposing
  `#[unsafe(no_mangle)] pub extern "C" fn main(...) -> i32` called by musl's
  `Scrt1.o` → `__libc_start_main`) and a `#![no_std]` + `-C panic=abort`
  `cdylib` shared `.so` library (exercising `dlopen`/`dlsym`, file-backed
  `MAP_SHARED`/`MAP_PRIVATE` CoW, `SIGBUS` past EOF, `flock`/`F_SETLK`,
  `renameat2`, and POSIX VFS edge cases), cross-compiled on both macOS and
  Ubuntu CI into `build/linux/` with the pinned
  `rustc --target x86_64-unknown-linux-musl` + `rust-lld`—avoiding
  `libgcc_s.so.1` (absent from both `alpine-minirootfs` and rustup's musl
  sysroot) and requiring **zero external C cross-compiler**. Using
  `SYSROOT=$(rustc --print sysroot)` and `HOST=$(rustc --print host-tuple)`
  (matching `Makefile`'s `build/linux/hello` recipe), both targets pass
  `-C target-feature=-crt-static` (required to disable static-CRT linking on
  `x86_64-unknown-linux-musl` and allow `cdylib` output),
  `-C linker="$SYSROOT/lib/rustlib/$HOST/bin/rust-lld" -C linker-flavor=ld.lld -C link-self-contained=no -C default-linker-libraries=no`,
  explicit `$SYSROOT/lib/rustlib/x86_64-unknown-linux-musl/lib/self-contained/{Scrt1.o,crti.o,crtn.o}`
  (`crti.o` before `crtn.o`, omitting `Scrt1.o` for `cdylib`), direct linking
  against `build/deps/alpine-minirootfs/lib/ld-musl-x86_64.so.1`, and
  `-C link-arg=--dynamic-linker=/lib/ld-musl-x86_64.so.1` for the executable.
  Build-time verification via `"$SYSROOT/lib/rustlib/$HOST/bin/llvm-readobj" -l -d`
  asserts `PT_INTERP == /lib/ld-musl-x86_64.so.1` and that every `DT_NEEDED`
  entry resolves inside the staged rootfs (no stray `libgcc_s.so.1` or
  `libc.so.6`). The M7 sources in `tools/linux-m7/` are formatted and checked
  by `Makefile`'s `fmt` and `fmt-check` targets alongside
  `tools/linux-hello/hello.rs`. `m7_vfs` (#254) is the POSIX VFS exit
  scenarios' program (`tests/scenarios/test_fs.py`): it calls the kernel by
  syscall number (inline `syscall`, raw `-errno`), so each check is about the
  syscall it names, not a libc wrapper's choice; musl only starts it and
  ends it (`exit` once `main` returns). Its modes: `posix` and `locks` (one `ok <check>` line per check, the
  first failure named, exiting with its number), `keep` (a small tree left
  on the disk for fsck), and `lock-hold` (record locks taken a step per
  read of its terminal, one key) with `lock-probe` (`F_GETLK`/`F_SETLK`
  from a second process meanwhile).
- **Rootless deterministic `ext4` assembly (`tools/mkrootfs.sh`, `make rootfs`):**
  assembles `build/deps/alpine-minirootfs/` + `build/linux/*` into
  **`build/alpine-rootfs.ext4`** (kept outside `build/deps/` so `actions/cache`
  is never poisoned by stale compiled test binaries and `make clean` removes
  it; `make test-scenarios` depends on `iso rootfs`, and `make run ROOTFS=1`
  attaches it interactively). Uses **`e2fsprogs`** (`mke2fs` and `e2fsck`;
  `apt-get install e2fsprogs` in `.github/workflows/ci.yml` and Homebrew
  `brew install e2fsprogs` on macOS, noting that Android SDK `mke2fs` lacks
  `e2fsck` and Homebrew `e2fsprogs` is keg-only). Both `tools/mkrootfs.sh` and
  `tests/scenarios/test_fs.py` locate a **matched `(mke2fs, e2fsck)` pair from
  a single directory** checking `LUMEN_E2FSPROGS_SBIN` →
  `/opt/homebrew/opt/e2fsprogs/sbin` → `/usr/local/opt/e2fsprogs/sbin` →
  `$PATH` (accepting a `$PATH` directory only when both `mke2fs` and `e2fsck`
  exist side-by-side, so Android SDK's `mke2fs` cannot hijack Homebrew
  `e2fsprogs`), failing loudly with an install hint if absent. To eliminate
  host `mke2fs.conf` drift between macOS and Ubuntu 24.04 CI,
  `tools/mkrootfs.sh` passes explicit `-t ext4 -b 4096 -I 256 -m 0 -L
  lumen-rootfs -U <fixed-uuid> -E hash_seed=<fixed-uuid>,root_owner=0:0,lazy_itable_init=0,no_copy_xattrs`,
  `E2FSPROGS_FAKE_TIME=<fixed-epoch>`, normalized staging mtimes, and the explicit
  pinned `-O` feature mask **`^orphan_file,^metadata_csum_seed,^metadata_csum,^has_journal`**
  (`-t ext4 -b 4096 -I 256 -O ^orphan_file,^metadata_csum_seed,^metadata_csum,^has_journal`,
  eliminating `mke2fs.conf` drift between macOS and Ubuntu 24.04 CI; `^metadata_csum`
  avoids `ext4plus 0.1.0-rc.2`'s `ExtentNode::to_bytes` checksum-offset bug on
  `depth >= 1` extent trees and lets cross-dir `rename` update `..` in block 0
  without private checksum helpers, while `^has_journal` avoids read-only
  `INCOMPAT_RECOVER` fallback because `ext4plus` only replays journals read-only
  and never writes journal transactions; note `-E root_owner=0:0` owns `/` as
  `0:0` while `-d` staged files inherit host UID/GID, so tests should not assert
  `st_uid == 0` on pre-populated staging files unless explicitly normalized). `e2fsck -fn` validates every freshly
  built image and every guest-mutated disk image in `test_fs.py`,
  `test_linux_user.py::test_dynamic_musl_binary_and_dlopen` and
  `test_alpine.py` after QEMU exits.
- **Parallel scenario isolation:** `test_blk.py` uses per-test blank scratch
  disks via `Devices(blk_mib=...)` (with `VIRTIO_BLK_F_RO` host-tested in
  `cargo:lumen-bcache` and optionally scenario-tested via a `Devices.blk_readonly`
  option). `test_fs.py`, `test_linux_user.py`, and `test_alpine.py` each copy
  the immutable master `build/alpine-rootfs.ext4` into their own per-test
  artifact/temp path before passing `Devices(blk_image=...)` to QEMU, so
  parallel `pytest-xdist` workers never collide.

Because M10's full compiler/runtime tests (multi-file C++20 builds, `go build`,
`rustc`/`cargo` with proc-macros, `node` V8 + `npm`, 2–4 GiB page-cache
reclaim, and full gVisor/LTP suites) require 2–4 GiB guests and would take tens
of minutes under TCG software emulation (which `ci.yml` uses on failure re-runs
and macOS uses locally), CI and `status.py` split into two tiers before M10
issues are filed while keeping `status.py` strictly stateless (Hard Rules 1
and 10—never carrying forward stale `"passing"` rows from an older `STATUS.md`):

1. **Separate JUnit outputs & feature-gate schema (`tests/features/*.toml` +
   `status.py`):** every feature defaults to `tier = "pr"`; M10 heavy features
   declare `tier = "heavy"` and their scenarios carry `@pytest.mark.heavy`.
   `make test-scenarios` runs `-m "not heavy"` and writes
   `build/test-results/scenarios.xml`; a new `make test-heavy` target runs
   `-m heavy` (`SCENARIO_JOBS=1`, KVM-only) and writes
   `build/test-results/scenarios-heavy.xml`, with neither target deleting the
   other. `status.py::collect_results()` merges both XML files when present,
   and `status.py` gains `--tier pr|heavy|all`.
2. **PR gate (fast, KVM, `ci.yml`):** runs `make test-scenarios` (`-m "not
   heavy"`), `audit:flags`, host tests, ktests, M0–M9 scenarios, and
   lightweight M10 smoke tests (`test_go_run_smoke`, `test_rustc_tiny_smoke`,
   `test_node_eval_smoke`), gated by `make status-check`
   (`status.py --check --tier pr`), keeping the 10-minute TCG failure re-run.
3. **Heavy runtime gate (separate workflow / merge-queue / nightly, KVM-only):**
   runs `make test-heavy` (`SCENARIO_JOBS=1`, avoiding 4×4 GiB OOM on 16 GiB
   runners), fails fast if `/dev/kvm` is absent (never silently falling back to
   TCG), omits the TCG failure re-run, and gates with
   `status.py --check --tier heavy`. Refreshing `docs/STATUS.md`
   (`status.py --write --tier all`) runs strictly over fresh merged PR + heavy
   results (either on a KVM host running both targets or a `main` CI job
   merging both artifacts). Both tiers obey the busy-host rule (no tight
   wall-clock ceilings inside scenarios; only outer CI job timeouts).

**M8's tests (designed in #336).**

- **Scenario files.** B0 adds `tests/scenarios/test_linux_proc.py`
  (processes, threads, signals, `futex`, timers, sessions, the small
  syscalls), `test_linux_io.py` (pipes, FIFOs, readiness, PTYs, the
  console as a tty), `test_linux_vm.py` (cross-CPU shootdowns) and
  `test_conformance.py` (libc-test), each empty until its issue's first
  test; the gates extend `test_alpine.py`, and C11 extends `test_smp.py`.
- **Feature files.** Each feature goes in its area's file:
  `process.toml` (B1, C1, C2, C3, C8, C12), `signal.toml` (C5, C6, C13),
  `futex.toml` (C4), `pipe.toml` (B3, C7), `tty.toml` (C9, C10; these five
  created by Phase A), `mm.toml` (B4, B5, B7), `sched.toml` (B2, B6),
  `smp.toml` (C11), `elf.toml` (C2's lumen-elf changes), `fs.toml` (B8,
  `mknod`), `user.toml` (B0's ABI and census tests, and the gates beside
  M7's `test_alpine` rows).
- **`tools/linux-m8/` test programs** (B0's skeleton): `m8_proc`
  (processes, threads, signals, faults, `futex`, timers, sessions, the
  small calls), `m8_io` (pipes, FIFOs, `poll`/`select`, PTYs), `m8_vm`
  (shootdowns) and `ptyrun` (D3: opens `/dev/ptmx`, starts its argument
  on the slave as a session leader with it as its controlling terminal,
  puts its own terminal (the console) in raw mode without echo, and relays
  bytes between its own stdin/stdout and the master, setting the PTY's
  window size from its own). Dynamically linked against musl,
  `#![no_std]`, built with the pinned `rustc --target
  x86_64-unknown-linux-musl` and `rust-lld` exactly as `tools/linux-m7/`
  (above), into `build/linux/`, so both images stage them. Each mode
  prints one `ok <check>` line per check and names the first failure,
  exiting with its number (`m7_vfs`'s convention). A check about a syscall
  calls it by number (inline `syscall`, raw `-errno`); a check about
  libc's use of the kernel (`pthread_create`, `posix_spawn`,
  `pthread_mutex_*`, `sigaction`, `alarm`, `timer_create` with
  `SIGEV_THREAD`) calls musl, which is the point there.
- **Timed checks** obey the busy-host rule (CLAUDE.md): a sleep, a futex
  timeout, a `poll` or `select` deadline, an interval timer and a balance
  pass are each checked to have lasted at least their time (a floor),
  never at most. A time left written back (`nanosleep`'s `rem`,
  `select`'s `tv`, `ppoll`'s and `pselect6`'s `timespec`) is checked only
  to be at most the time given and at least 0. A tick count is checked to
  be at least one tick, never a fraction of the time spun (KVM coalesces
  ticks).
- **Nothing left to chance.** A cross-CPU check pins its threads with
  `sched_setaffinity`, confirms each placement with `getcpu`, and has
  every reader report one successful read before the event it watches
  (an unmap, a `fork`); it fails, rather than passes, on a guest with
  fewer CPUs than it needs. A test of a signal interrupting a wait first
  confirms the waiter is blocked (its state in the `ps` command) and then
  resends the signal until it sees the `EINTR` or the restart; a futex
  wake or requeue test loops until the expected count of waiters is
  queued. A race ktest has a ktest-only delay knob at the exact point it
  races and names the mutant it kills. A negative check has a positive
  control that shows the check could have seen the effect.
- **Leak counts.** The scenarios that count leaked frames (`linux` prints
  the frames a run kept; `rootfs.run_busybox`) first confirm no process
  the run started is still live (a root's children outlive the `linux`
  command under pid 1, section 8): a program that leaves children kills
  and reaps them (`wait4`) before it exits, and the scenario polls the
  `ps` shell command (B1: one row per process in `PROCS`, with its pid,
  tids and state) until only pid 1 is left, under an outer limit.
- **Shared scenarios grow by issue.** A scenario several issues extend
  tags each check with the issue that adds it, and an issue adds its
  checks only once the issues they need have merged; every tag below names
  an issue that depends, in the issue table, on everything its checks
  need. So C3 comes after C4 (`pthread_join` needs C4's `clear_child_tid`
  wake), C5 and C6 (`de_thread`'s `SIGKILL` and its ktests' group
  stops); C8 after C3 (non-leader tids); C11 after C5 and C12 (`SIGSEGV`,
  `sched_setaffinity`, `getcpu`); and the checks of C1's scenario that
  need signals, `rusage` or groups are tagged C6, C12, C8 and C9, not C1.

**What each test checks** (each check tagged with the issue that adds it;
every scenario check is one `ok` line of a `tools/linux-m8` mode):

- `ktest:lumen_kernel::linux::process::procs_lookup_and_lifetime` (B1):
  `PROCS` finds processes and threads by pid and tid; a process's `mm`,
  `files` and `fs` are `None` when its last `Arc` drops (the `Drop`
  assertion, pid 1 aside); every M6 and M7 scenario passes unchanged but
  for `test_mm`'s boot heap ceiling (pid 1's allocations).
- `ktest:lumen_kernel::linux::process::procs_leave_no_heap_behind` (B1): a
  process's run gives back every heap allocation it made (live count and
  bytes).
- `ktest:lumen_kernel::linux::syscall::task::identity_syscalls_name_the_process_and_thread`
  (B1): `set_tid_address`, `gettid`, `getpid`, `getppid`, `getpgrp`
  through `dispatch`.
- `ktest:lumen_kernel::linux::syscall::signal::sigaction_and_sigprocmask_round_trip`
  (B1): M6's `rt_sigaction` and `rt_sigprocmask` over the split state, a
  mask per thread and an action per process.
- `ktest:lumen_kernel::mm::frame::named_reservations_are_complete` (it
  exists; B4 extends it): `frame::REFS` is reserved under its name.
- `ktest:lumen_kernel::mm::frame::refcount_and_cow_split` (B4): counts
  through `fork_copy`; a split at count 2 and the reuse at 1; a shared
  anonymous page staying shared, and writable in both, after `fork`;
  `AddressSpace::users`, raised and lowered through `add_user`/`drop_user` as a
  `CLONE_VM` pair would (the first lowering doesn't tear the space down;
  an extra `Arc` held by the test doesn't delay the teardown at 0);
  `teardown` releasing its references and freeing frames only at zero.
- `ktest:lumen_kernel::mm::user_vm::mprotect_keeps_a_shared_frame_read_only`
  (B4): `mprotect(PROT_WRITE)` on a private leaf whose count is 2 leaves it
  read-only, and the next write splits it (including a PROT_NONE page);
  shared anonymous PROT_NONE pages stay inaccessible in both copies until
  upgraded, when `mprotect` sets `WRITABLE`.
- `ktest:lumen_kernel::mm::frame::fork_copy_out_of_frames_rolls_back` (B4):
  exhaustion during a split leaves the old leaf unchanged; a partial fork
  releases the child's references and tables and leaves the parent reusable.
- `ktest:lumen_kernel::mm::user_vm::fork_copy_preserves_files_images_and_private_copies`
  (B4): preserves the break, image names and mapped-file references; skips
  cache leaves and private copies at or past EOF, including nonzero file
  offsets, while sharing private copies below EOF and splitting on writes.
- `ktest:lumen_kernel::user::nofault_access_answers_fault_without_paging_in`
  (B2): unmapped and not-yet-faulted pages return `Fault` for both accesses;
  read-only and copy-on-write pages return `Fault` for the exchange only,
  each under a held `SpinLock`, nothing mapped; a mapped page's read and
  exchange; `fault_in_writable` on a not-yet-faulted page and on a
  copy-on-write page leaves the leaf writable with its Dirty bit still
  clear (nothing stored), and, with an incrementer on another CPU racing
  it, the counter ends exact.
  `harness:test_audit::test_nofault_fixup_section_is_read_only_and_in_rodata`
  (B2) checks the section.
- `ktest:lumen_kernel::mm::tlb::shootdown_across_cpus` (B5): a test address
  space loaded on three target CPUs by threads pinned there (not
  `run_on`, whose jobs run on `kernel_root()`), each first reporting one
  successful `read_u32_nofault` of the page, then probing it in a loop;
  one CPU unmaps it: once `shoot_down` returns, every probe answers
  `Fault`. Also a full flush past 32 pages; narrowing `mprotect`
  (`PROT_NONE`) across the three target CPUs; a target spinning with an
  IRQ-off `SpinLock` held, released only after the request is published
  (its `ACKED` still behind and the unmapped frame's refcount still >= 1),
  answering once it lets go (refcount then 0); a break-before-make
  copy-on-write split on a `fork_copy` page (during the shootdown window,
  the leaf is unmapped and the old frame's refcount is still 2; after
  release, the reader sees the copy's bytes); a target that switched away
  answering through `active`; `teardown` waiting for `active` to empty (a
  pinned thread's switch delayed by `allow_switch`). It fails on fewer
  than 4 CPUs.
- `ktest:lumen_kernel::mm::user_vm::truncate_unmaps_every_address_space`
  (B7): on both tmpfs and ext4 (requires at least 2 online CPUs), two spaces
  mapping one file (the second loaded and actively probed via
  `read_u32_nofault` on another CPU) and a third forked from the first;
  after `truncate` + `invalidate_file_mappings`, checks `unmapped == 5`,
  zero stale reads and immediate faults on the remote CPU (while page 0
  and page 1's zeroed tail stay readable), every space's page-2 leaves gone
  and refaulting into `BusError`, the shared private CoW copy freed,
  `writable_maps == 0`, and `mappers == 0` after teardown. Unregistration
  across `munmap`, `MAP_FIXED` (both anonymous replacement and same-file
  replacement keeping registration), `brk`, `teardown` and `Drop` is
  asserted on both tmpfs and ext4 in
  `ktest:lumen_kernel::mm::user_vm::file_vm_writable_maps_balance_through_mprotect_unmap_and_teardown`,
  and the real `truncate`, `ftruncate` and `openat(O_TRUNC)` syscalls
  reaching a second address space are checked by
  `ktest:lumen_kernel::linux::syscall::meta::tests::meta_truncate_and_open_trunc_unmap_a_second_address_space`.
- `ktest:lumen_kernel::mm::user_vm::fork_racing_a_truncate_copies_nothing_past_eof`
  (B7): on both tmpfs and ext4, a parent with private (copied) and
  page-cache (writable and read-only) leaves of a file past the new end;
  phase 1 takes the truncate's mapper snapshot through `before_register`
  while `fork_copy` holds both Lock 1s before registering the child
  (asserting `mappers == 1` at the truncate seam and `mappers == 2` at
  `before_sizes`, before `file_sizes` runs), proving the child skips leaves
  at or past the new EOF (`BusError`, not the old page, killing both the
  missing-size-check and register-after-`file_sizes` mutants); phase 2
  takes the snapshot through `after_precount` after registration
  (`mappers == 2`) and after `file_sizes` saw the old size, proving the
  snapshot reaches the child and unmaps the copied leaf once `fork_copy`
  releases its Lock 1.
- `scenario:test_linux_vm::test_cross_cpu_munmap_and_mprotect_shootdown`
  (C11): readers pinned to every CPU, each confirming a read first, then
  reading a page another thread unmaps or protects: each gets `SIGSEGV`
  afterwards and none reads stale data; `fork` from a threaded parent
  while a sibling, confirmed running, writes on another CPU (`fork_copy`'s
  full flush: the child never sees a write made after the fork); a
  copy-on-write split's single-page shootdown with a sibling reading the
  page.
- `scenario:test_linux_proc::test_fork_wait_and_exit_status`: status
  words for `exit` codes with no core bit, `WNOHANG` returning 0, `ECHILD`
  with no children, `waitid`'s `siginfo` and its zeroed `infop` with
  `WNOHANG`, a zombie until reaped, an orphan's `getppid` becoming 1,
  `SIG_IGN` and `SA_NOCLDWAIT` reaping automatically (`wait` then
  `ECHILD`), a raw `clone(0x4111)` (`posix_spawn`'s flags) whose child
  calls `_exit`, and `vfork` (C1). C1 also checks WNOWAIT, reap-before-copyout
  faults, untouched waitid bytes and zero fields on ECHILD, inherited
  registers/FPU/TLS/cwd/mask/dispositions, shared files/umask, TID stores
  and clear-before-vfork-release, ignored high clone flag bits, thread-clone
  ENOSYS, and a stackless child exiting after IRET with noncanonical RSP.
  Adoption uses a handshake: the grandchild reads its new ppid only after the root has
  waited for its parent. The root polls `/proc/<gpid>` until ENOENT before
  exiting, so the scenario checks an exact leaked-frame count. Later checks:
  `rusage`'s `ru_utime` at least one tick
  after a spinning child and `RUSAGE_CHILDREN` equal to the sum of the
  `wait4` values (C12); fatal-signal statuses, `kill(1, sig)` doing
  nothing and `kill(-1, sig)` skipping pid 1 (C6); `wait4` with `pid` 0
  and `-pgid` (C8); `HangupThenContinue` (a stopped process group
  orphaned by its parent's exit gets `SIGHUP` then `SIGCONT`) (C9); `exit`
  ending one thread, `exit_group` all, and `pthread_create`'s flag set
  (C3).
- C1's kernel tests (`lumen_kernel::linux::process`):
  `fork_refusal_releases_unlinked_resources` checks copied/shared Files and
  mm users on refusal, capture ENOSYS, and cancellation of a parked child;
  `fork_files_count_slots_and_skip_reservations` checks FD references,
  cloexec and reservation exclusion; `files_unshare_waits_for_open_reservations`
  checks install/cancel ordering against replacement;
  `replace_mm_keeps_process_and_thread_in_agreement` checks both mm references,
  retirement and the loaded mappings; `wait_child_interrupts_and_wakes_on_exit`
  checks ERESTARTSYS and a blocked wait's exit wake (a rescue wake must find
  zero waiters, making a missing wake fail cleanly);
  `shared_files_exit_releases_its_posix_locks` checks a CLONE_FILES child's
  F_SETLK goes away at its exit while the parent's table stays open;
  `shared_files_close_releases_all_members_posix_locks` checks that close,
  dup displacement and slot replacement drop the child's lock before it exits;
  `shared_files_leave_waits_for_inflight_close` holds an inode lock to force
  removal before the exit sweep, checks leave blocks with both members still
  present, and checks no record lock survives after both operations finish.
- `scenario:test_linux_proc::test_fork_cow_and_exec_shebang`: parent and
  child writing the same heap, stack and file-private (`MAP_PRIVATE`)
  page after `fork`, each seeing only its own; shared anonymous memory
  staying shared; `#!` (a script, a nested script, `-ELOOP`, an empty
  interpreter `-EACCES`, exactly five scripts accepted and six refused),
  exact nested argv and two ordered envp entries, `/proc/self/exe` the
  interpreter or canonical executable, and byte-path exec whose raw-byte
  readlink and subsequent open succeed. Each absolute, relative and nested
  script tail selects exactly one argv shape. Self-reexec via `/proc/self/exe`
  and fd aliases leaves a real binary identity that can be opened. A renamed
  fd still execs, clearing the old identity to ENOENT; AT_RANDOM differs
  between two exec'd children.
  `comm` is checked
  by the exec frame ktest. C2 checks CLOEXEC, ignored signals, handled
  disposition reset, a cleared alternate stack, FPU reset, E2BIG and the
  MAX_ARG_STRLEN boundary; pending signals are checked only by the ktest;
  a `vfork` child's `execve` releasing its parent before the child exits
  (the exec'd image confirms its CLOEXEC check with SIGUSR1 before its parent kills it); a binary over 16 MiB
  (built by `tools/linux-m8`, padded) (C2); from a process with threads (C3): a
  non-leader's `execve` taking the pid, the new image running, every
  other thread gone and the process not killed (its later exit status the
  new image's); two threads `execve`ing at once with exactly one image
  running, whichever wins; POSIX timers deleted and an `alarm` surviving
  (C13). A scenario checks only what user space can force: the orderings
  of a stop or an `exit_group` against `de_thread` are the C3 ktests
  below, since a stopped sibling stops its whole group, the `execve`
  caller too, and nothing in user space can place an `exit_group` inside
  `de_thread`.
- C2 ktests: `procfs_exe_cycles_share_the_symlink_limit` checks direct and
  indirect stored exe cycles return ELOOP and ordinary resolution recovers;
  `exec_pointer_count_is_bounded_before_allocation` checks E2BIG
  without mapped heap growth and EFAULT after a long pointer walk;
  `exec_argument_walk_observes_fatal_signal` checks EINTR during the walk;
  `execve_replaces_frame_and_preserves_failed_call` checks script comm,
  registers/TLS/mm/identity, including ENOMEM after loading while preparing
  shared Files; `exec_signal_reset_preserves_pending_and_refuses_threads`
  checks both pending queues, the mask, and thread/group-exit refusal;
  `exec_name_reaches_crash_snapshot_without_blocking` checks the shared name
  and busy-lock fallback; `exec_load_releases_heap_and_frames` checks exact
  live allocations and frames; `a_binary_the_heap_cannot_hold_is_enomem`
  refuses a 64 KiB header table and then succeeds; and
  `streaming_exec_keeps_sources_and_uses_sized_stack` checks unlink retention,
  distant payloads, literal stack limits and short-read rollback.
  `cargo:lumen-elf` checks split-header sizes, interpreter bounds and fallible
  allocation; `cargo:lumen-vm` checks StackSize; `cargo:lumen-vfs` checks
  prepared descriptor capacity and next-CLOEXEC selection, plus the host tests
  `named_resolution_follows_links_dotdot_and_magic_identities` and
  `named_resolution_preserves_inode_when_identity_fails`: canonical names,
  bounded aliases, equal-ino cross-device rejection and preservation of the
  same resolved Arc even when naming fails.
- `scenario:test_linux_proc::test_exec_uses_unseeded_random_fallback` (C2):
  without a seeded RNG, shell loading and execve both succeed, emit one
  fallback warning per boot, AT_RANDOM differs between two exec'd children,
  and a second shell load leaks zero frames.
- `scenario:test_linux_proc::test_pthread_create_and_join` (C3): the clone
  flag matrix's `EINVAL` order, `CLONE_THREAD` without `CLONE_FILES` or
  `CLONE_FS` included; `CLONE_SETTLS` with `tls >= USER_MAX` `EPERM`;
  `CLONE_FS` and `CLONE_FILES` without `CLONE_THREAD` sharing what they
  name; the exit-signal deviation; `CLONE_PARENT_SETTID` (the child's
  first action reads its own tid from the word); `pthread_create`/`join`,
  thread exit waking the joiner through `clear_child_tid`; `MAX_STACKS`
  exhaustion giving `EAGAIN` and leaking no frames, after which a kernel
  thread still starts (a `run_sized` from the shell). With it,
  `ktest:lumen_kernel::linux::process::clone_racing_a_group_exit_never_escapes`
  (a delay knob between the clone's allocations and its hold, while
  another thread calls `exit_group`: the clone is `-EINTR` or its thread
  is killed; and `-ERESTARTNOINTR` with a `group_stop` and with an
  `exec_waiter`), and three ktests whose `execve` stalls at
  `process::EXEC_DELAY`, a ktest-only knob between step 5 and
  `de_thread`'s hold:
  - `ktest:lumen_kernel::linux::process::de_thread_refuses_a_racing_group_stop`:
    a sibling's `SIGTSTP` stalled in its two-hold gap (a second knob,
    `signal::STOP_GAP_DELAY`, between the orphan check and the hold that
    starts the stop) while `de_thread` runs: the stop refused, the sibling
    ended and the new image not stopped; a `SIGCONT` sent in the gap
    instead: no stop; a stop completed before `de_thread`: cleared by it,
    the new image not stopped. It kills the mutants without the
    `exec_waiter` refusal, without the `stop_dequeued` check, and with
    `de_thread` leaving `group_stop` set.
  - `ktest:lumen_kernel::linux::process::exit_group_during_de_thread_ends_only_itself`:
    exit step 1 run by a sibling with `exec_waiter` set directly (as a
    fatal signal dequeued before `de_thread`'s hold, or an `exit_group`,
    reaches it): no `group_exit` recorded, only the sibling ended, the
    caller's `execve` succeeding; a sibling in a `vfork` wait ended by
    `de_thread`'s kick, the caller's wait ending under an outer limit. It
    kills the mutants without step 1's re-check and without `de_thread`'s
    `fatal_pending`.
  - `ktest:lumen_kernel::linux::process::exit_retargets_signals_and_completes_a_stop`:
    a process-directed signal `select_target` aimed at a thread that then
    ends (through `exit`, and killed by `de_thread`) taken by another
    thread, the `execve` caller included once in its new image; the last
    unparked thread of a group stop exiting, completing the stop (the
    parent's `CLD_STOPPED`). It kills the mutants without step 2's
    retarget and without its `stop_pending` count.
- `scenario:test_linux_proc::test_futex_timeouts_and_nanosleep` (C4):
  timed waits lasting at least their timeout; wakes, requeues and bitsets,
  each looped until the expected count; private and shared ops on one
  word not meeting; `File`, `SharedFrame` and `Anon` keys across `fork` (a
  shared file page and a shared anonymous page wake across processes; a
  private one doesn't, with the waker `WAKE`-ing through the child's whole
  timed wait, which ends `ETIMEDOUT`); a `WAIT` on an untouched private
  file page, then a write, then a `WAKE` finding the waiter; `WAIT` on an
  untouched `mmap` page and on a not yet faulted file page giving `EAGAIN`
  or blocking, never `EFAULT`; `CMP_REQUEUE` with `uaddr1` untouched;
  `FUTEX_CLOCK_REALTIME` with plain `WAIT` `ENOSYS`; robust lists: a
  process-shared robust mutex in `MAP_SHARED` memory whose holder
  *process* exits gives the next locker `EOWNERDEAD`, and
  `set_robust_list` with `len` other than 24 is `EINVAL`; `nanosleep` and
  `clock_nanosleep` floors, `TIMER_ABSTIME` included. Requeue wakes one and
  moves one of two confirmed waiters; zero requeue count leaves the second
  waiter on the source; CMP_REQUEUE compares a source distinct from its
  destination word; high requeue-count bits are ignored. Private WAIT and
  CMP_REQUEUE on unmapped and PROT_NONE pages return EFAULT. Error ordering
  covers alignment/range and timeout/bitset; `get_robust_list` round-trips
  a registered head and rejects an absent tid. Unsupported futex operations
  and `futex_waitv` return ENOSYS. Known nonsleeping clocks return EOPNOTSUPP;
  Process CPU-time, TAI, alarm and invalid clocks return EINVAL. A `CLONE_VM | SIGCHLD`
  child registers `set_tid_address`, then exits or execs; a second process
  sharing its mm waits on the word and returns 0 after the store/wake.
  The robust walk at exec marks the old shared word OWNER_DIED.
  Four kernel tests supplement the scenario:
  - `ktest:lumen_kernel::linux::futex::futex_cancel_follows_requeue_and_wake_wins`:
    cancellation follows same- and different-bucket requeues; a claimed wake
    wins over ERESTARTSYS.
  - `ktest:lumen_kernel::linux::futex::robust_walk_checks_owners_pending_and_faults`:
    ownership, pending once-only handling, PI death without ordinary wake,
    next-pointer faults, negative offsets and bounded cyclic lists.
  - `ktest:lumen_kernel::linux::futex::futex_file_keys_and_private_cow_before_wait`:
    file aliases/offsets and a writable private-file WAIT confirmed blocked
    before the parent resolves its key or writes the page.
  - `ktest:lumen_kernel::linux::syscall::futex::sleep_and_futex_restart_preserve_deadlines`:
    saved deadlines, interrupted relative sleep completion, absolute-sleep
    restart codes, failed copyout, and remaining-time floors and ceilings.
- `scenario:test_linux_proc::test_futex_and_pthread_mutex_stress` (C11):
  threads pinned across CPUs incrementing a shared counter under a
  `pthread_mutex` and handing turns through a condition variable, a fixed
  number of iterations each, the total checked exact.
- `scenario:test_linux_proc::test_signals_and_sigaltstack[*]` (C5):
  - handlers with `SA_SIGINFO` on and off an alternate stack,
    `SA_RESTART`, `SA_RESETHAND`, `SA_NODEFER`, masks,
    `kill`/`tkill`/`tgkill`, `raise`, and `rt_sigqueueinfo`'s `EPERM` for
    a positive `si_code` sent to another process (two `linux` programs at
    once);
  - `m8_proc faults`, one `ok` per row of section 6's table that ring 3
    can reach, each read by an `SA_SIGINFO` handler (`si_signo`,
    `si_code`, `si_addr`, `REG_TRAPNO`, `REG_ERR`, `REG_CR2`):
    `#DE`, `#UD`, `#BP`, `#GP` from `hlt`,
    `SEGV_MAPERR` against `SEGV_ACCERR`, `SIGBUS` past a file's end; a
    blocked or ignored fault signal killing; a ring-3 `#DB` (`pushf`/`popf`
    setting `TF`) giving `TRAP_TRACE`;
  - `force_sigsegv`: a `SIGSEGV` handler on the alternate stack catching a
    `SIGUSR1` frame that couldn't be written;
  - `rt_sigreturn`'s refusals, one check each: an `MXCSR` outside
    `MXCSR_MASK`, `XSTATE_BV` outside `XCR0`, `XCOMP_BV` non-zero, a bad
    `CS`, a non-canonical `rip`, and bad software magic falling back
    to FX-only (the frame's legacy XMM registers restored, the extended
    state zeroed: YMM's upper halves read back 0);
  - A bad `SS` is replaced by the user selector; XMM registers and `MXCSR`
    survive a handler. A process spinning without syscalls receives
    `SIGUSR1` on an interrupt return and prints `ok async_delivery`.
  - The scenario runs twice, parametrized: with QEMU's default CPU (no
    `XSAVE`: the FX-only path) and with `Machine(cpu="max")`, which first
    checks the boot log says `XSAVE` is on.
  - Not tested, stated as gaps: `#OF` and `#BR` (`into` and `bound` are
    `#UD` in 64-bit mode, so ring 3 can't raise them), `#NP`, `#AC` (needs
    `CR0.AM`, which Lumen doesn't set), `#MF` (an x87 exception with
    `CR0.NE`) and the out-of-memory `SIGKILL`. Real `#SS` delivery is not
    tested under TCG (the non-canonical push raises `#GP`), nor real `#XM`
    delivery (TCG sets its status without trapping). `ok ss` accepts TCG's
    #GP fallback and `ok xm` accepts its checked unmasked-status fallback;
    neither line claims that TCG delivered #SS/#XM.
    `ktest:lumen_kernel::linux::signal::frame::stack_and_simd_exceptions_map_to_signals`
    checks these mappings and that a spurious `#MF`/`#XM` sends nothing.
  - `ktest:lumen_kernel::linux::process::fork_child_fault_is_delivered_and_reaped`
    checks a fork child's `#UD` signal and wait status;
    `ktest:lumen_kernel::linux::process::kill_wakes_fork_child_and_wins_over_exit`
    checks a killable fork child wakes on `fatal_pending` and SIGKILL wins
    over a later exit. `recorded_exit_wins_over_later_signals` checks the
    reverse ordering and discarding sends to ending processes/zombies.
    `fork_pending_signal_precedes_first_user_instruction` checks the fork
    return hook delivers before entry. The guest's `fork_altstack`,
    `vfork_altstack` and `clone_vm_clears_altstack` query inherited stack
    state and check where the child's SA_ONSTACK handler actually runs.
    `ktest:lumen_kernel::linux::signal::frame::deferred_debug_trap_keeps_original_address`
    gives the interrupt hook distinct trap and interrupt RIPs and checks the
    copied siginfo/context. The guest DB loop checks its recorded trap address,
    but TCG may deliver the self-IPI before that loop advances.
    `deferred_user_debug_trap_keeps_syscall_result` and
    `deferred_user_debug_trap_keeps_syscall_restart` drive the syscall dispatcher
    with a distinct user trap address, checking the copied frame and sigreturn
    for successful and restartable results. `deferred_lstar_debug_trap_reexecutes_syscall`
    checks the LSTAR origin skips execution, rewinds once and drops restart context.
- `scenario:test_linux_proc::test_sigchld_and_sigint_interrupt_wait` (C6):
  `EINTR` and restarts from a blocking `read`, `wait4`, `nanosleep` and
  `pselect6`, each waiter confirmed blocked first and each signal resent
  until the outcome is seen; `SIGCHLD` waking `rt_sigsuspend`;
  `rt_sigtimedwait` (a wanted signal, `EAGAIN` at its timeout),
  `rt_sigpending`, `pause`; stop and continue with
  `WUNTRACED`/`WCONTINUED`; `pselect6`'s mask swap, deterministically
  (`SIGCHLD` blocked, a child exits, `pselect6` with it unblocked is
  interrupted); an interrupted `poll` leaving no `PollHead` subscriber
  (the console's subscriber count, through a test-only count in `serial`,
  is 1 during the wait, then 0). The `restart_syscall` path is the
  ktest's (`restart_with_no_handler_restarts_or_resumes`, B2: `rax == 219`
  after the hook, the restart block's deadline unchanged), since a
  scenario can't tell a restart from a long wait.
- `scenario:test_linux_proc::test_interval_and_posix_timers` (C13):
  `alarm` and `setitimer` floors; a periodic `ITIMER_REAL` delivering a
  third `SIGALRM` after its first two were handled; `timer_create` with
  each notification (`SIGEV_SIGNAL`, `SIGEV_THREAD_ID`, `SIGEV_NONE`, and
  musl's `SIGEV_THREAD`, the process surviving each expiry); a
  `SIGEV_NONE` timer sending nothing while a `SIGEV_SIGNAL` timer with the
  same deadline fired (the positive control), its `timer_gettime`
  reporting 0 once a one-shot has expired and, periodic, an `it_value`
  above 0 and at most its interval, and `timer_getoverrun` 0;
  `timer_settime` absolute and relative, a re-armed timer firing no
  earlier than its new deadline; a new timer taking a deleted one's id
  firing no earlier than its own deadline; the 33rd timer `EAGAIN`;
  `timer_getoverrun` after a blocked signal; a periodic timer whose queued
  signal a `SIG_IGN` disposition discarded firing again once a handler is
  set; `timer_delete` discarding the pending signal, and a deleted timer
  sending nothing for twice its old interval; `fork` inheriting no timer.
- `scenario:test_linux_proc::test_sessions_and_process_groups` (C8): each
  call's success and errno cases, `getpgid` and `kill` with a non-leader
  tid `ESRCH`, `kill` of a group. `is_busy` counting tids (and so
  `tcsetpgrp(tid)` giving `EPERM`) is host-tested in `cargo:lumen-proc`
  (B1).
- `scenario:test_linux_proc::test_small_syscalls` (C12): `getcpu` after
  pinning; `getpriority` returning `20 - nice` after `setpriority`; the
  `prctl` name round trip; `statfs`'s magic for ext4, tmpfs and procfs;
  `getrusage` at least one tick; `sysinfo` (`totalram` equal to `mem`'s
  total, `uptime` no less than a reading taken before); `prlimit64(RLIMIT_STACK)`
  and then `execve`, checking the `[stack]` line's size in
  `/proc/self/maps` and the argument budget the new limit gives;
  `gettid` and `set_tid_address` on the new model.
- `ktest:lumen_kernel::sched::nice_scales_slices` (B6) checks the
  scheduler uses the incoming thread's `lumen_time::slice_ticks(nice)`
  after yielding to a nice-0 peer, with automatic balancing paused. The
  pure Linux-weight calculation is host-tested by `cargo:lumen-time`;
  `threads_migrate_to_idle_cpus` and `pinned_threads_never_migrate` drive
  balance passes directly through a ktest-only call, counting passes,
  never time, the second with an unpinned thread that the same pass does
  move; `ktest:lumen_kernel::sched::migration_keeps_user_state` moves a
  user thread between CPUs between its `syscall`s and checks its XMM
  registers, `fs_base` and GPRs after each.
  `automatic_balancer_moves_runnable_threads` keeps the runner and three
  migratable workers runnable and checks all three workers leave the
  original CPU, without explicit balance calls or timing ratios.
  `sleeping_deadline_follows_affinity` and `wait_deadline_follows_affinity`
  change a sleeping/blocked thread's mask, then check its timer wake runs
  on the allowed CPU (automatic balancing paused); a late observation
  retries rather than asserting a timing bound.
  `affinity_refuses_immutable_pins` checks kernel, idle and user-probe pins.
  `balancing_skips_pinned_donor_and_moves_user_roots` skips a busiest queue
  containing only pins and migrates an ordinary user root from the next donor.
  `queue_reservations_fail_and_roll_back` refuses a real queue allocation
  after an earlier queue reserved successfully, then checks both counts
  rolled back, and checks dropping a successful unpublished reservation.
  `ktest:lumen_kernel::linux::process::fork_inherits_affinity_and_nice`
  gives a separate migratable parent a two-CPU mask and nice 5, verifies
  both in the child, then moves it within that mask.
  `ktest:lumen_kernel::serial_in::reads_queued_input_on_non_boot_cpu`
  consumes queued input and checks the empty UART fallback on an AP.
  `scenario:test_smp::test_multithreaded_workload_uses_all_cpus`
  (C11): one process's threads, pinned one to each CPU and confirmed by
  `getcpu`, each make progress.
- `ktest:lumen_kernel::linux::process::shared_description_offsets_move_once`
  (B8): two threads reading one description, regular and procfs, every
  byte read once, with a delay knob at the procfs snapshot (it kills the
  mutant that drops `pos`); a copy-out fault's give-back under contention.
  `ktest:lumen_kernel::linux::process::shared_description_getdents_lists_once`:
  two threads listing one directory through one description, every name
  listed once, with a delay knob between `readdir` and the copy out (it
  kills the mutant that releases `pos` there).
  `ktest:lumen_kernel::linux::process::dup_and_openat_reserve_their_slot`:
  the kernel's `dup2` helper, driven directly onto a slot the ktest holds
  reserved, is `-EBUSY`; a full table refuses an `O_CREAT` before it
  creates.
  `ktest:lumen_kernel::linux::process::open_reservation_survives_until_install`:
  an `openat(O_CREAT)` paused after opening and before installation keeps
  its reserved slot `r`: `dup2` onto it is `EBUSY`, `F_DUPFD` gets
  `r + 1`, and the open returns exactly `r`. This check runs separately
  from `dup_and_openat_reserve_their_slot` so neither hides the other's
  failures.
  `ktest:lumen_kernel::linux::syscall::lock::fcntl_seek_cur_uses_committed_position`:
  a `SEEK_CUR` context read waits under contention until a tentative
  offset is rolled back and then sees the committed offset; `SEEK_SET`
  does not wait for Lock 0. This models the offset publication and rollback
  at `lock_context`; it does not test Lock 0's absence across a
  `F_SETLKW` wait.
  `ktest:lumen_kernel::linux::syscall::lock::setlkw_records_each_waiting_thread`:
  two threads of one process blocked in `F_SETLKW` keep a record each, and
  a cycle through either is `EDEADLK`.
- `ktest:lumen_kernel::linux::syscall::poll::poll_rechecks_after_a_wake`
  (B3): with B3's test `Pollable`, a delay knob between clearing `fired`
  and the re-poll, and a change landing there, the wait still ends; a
  no-timeout (`-1`) re-poll hook that flips the source ready and raises
  `USR1` during the re-poll still returns `1` (killing the mutant that
  clears `fired` after re-polling, which loses `fired` and returns
  `-ERESTART_RESTARTBLOCK`); a waiter whose `ready` reads a `seq` counter
  wakes for a change made between its read and its block; and interrupted
  waits (`poll`, `ppoll`, `select`, `pselect6`, through `raise_for_test`)
  drop their `PollGuard`s (subscriber count back to 0) and write back
  `revents` on `poll` (`-EFAULT` with no `RestartBlock` on read-only
  `fds`). Beside it, `poll_resume_honours_the_saved_deadline` invokes
  `resume_poll` with a past deadline and a pending signal and checks it
  returns `0` (killing the mutant that turns the saved deadline into
  `Forever`), and `ppoll_installs_its_mask_and_writes_back_when_interrupted`
  checks that `ppoll`/`pselect6` install their temporary mask during the
  call (its first pass), restore it on `-ERESTARTNOHAND`, write remaining time
  back (`rem < asked`, `rem + elapsed >= asked`, for `select` too), and turn
  `-ERESTARTNOHAND` into `-EINTR` when the timeout pointer is read-only.
- `scenario:test_linux_io::test_pipes_and_poll` (C7): a busybox pipeline;
  two writers' 4000-byte records into 8192 bytes with record-sized reads;
  deterministic nonblocking writes into a 4096-byte pipe containing 100
  bytes (4096 returns `EAGAIN`, leaves 100; 4097 returns 3996); `EOF`,
  `FIONREAD`, default/ignored/caught/blocked `SIGPIPE`, `SI_USER` sender
  fields, pending bit 13 and termination on unblock; `EPIPE` before
  `EFAULT`, for blocking and nonblocking writes, and readerless `writev`.
  `pipe_writev_and_faults` checks total-length partial progress, crossing
  spans, atomic faults leaving nothing queued, and direct pipe `readv`.
  `proc_pipe_reopen_and_large_write` checks partnerless anonymous pipe
  reopens and a blocking write over 1 MiB. Also: pipe `fstat`, proc link,
  `ESPIPE` for positioning, `fsync` `EINVAL`, capacity resize errors,
  `dup`/`dup2`/`dup3` flags, validation order and failed-target absence;
  FIFO open rules and per-description HUP generations; `posix_spawn`
  success and missing-executable `ENOENT` through its `O_CLOEXEC` pipe;
  inherited pipe/flock descriptor counts across `fork`; `mknodat` types
  (`S_IFDIR`, `S_IFCHR`, `S_IFBLK`, `S_IFSOCK`), invalid type before
  lookup, `EEXIST`, devfs `EROFS`; an ext4 FIFO whose type, cleared
  `EXTENTS`, zero `i_block` and deleted sibling are inspected using
  `debugfs`, and whose image passes `e2fsck -fn`.
- `ktest:lumen_kernel::linux::syscall::poll::poll_wakes_on_a_pipe_write`:
  a confirmed blocked subscriber wakes on data and then on writer close;
  each return releases its subscription.
- `ktest:lumen_kernel::linux::syscall::fd::pipe_atomic_writer_waits_for_the_entire_record`:
  a 4000-byte writer on a 4096-byte pipe containing 100 bytes is confirmed
  blocked, with exactly 100 still queued; draining those 100 lets it finish
  and queues all 4000 unchanged bytes.
- `ktest:lumen_kernel::linux::syscall::fd::pipe_blocked_writer_gets_epipe_when_reader_closes`:
  a confirmed blocked writer on a full pipe wakes with `EPIPE` and blocked
  `SIGPIPE` pending when its reader closes, exercising `WriteStep::Broken`.
- `ktest:lumen_kernel::linux::syscall::fd::fifo_blocking_opens_wait_for_peers_and_interrupt_detaches`:
  both blocking open modes are confirmed asleep before the peer opens;
  interrupting either mode returns `ERESTARTSYS` without leaking end counts
  while a second same-side description keeps the slot attached. Closing that
  description then detaches the slot.
- `ktest:lumen_kernel::vfs::ext4::ext4_unlinking_a_device_with_xattr_preserves_foreign_blocks`:
  an imported character device has an external xattr block (`i_blocks != 0`)
  and an rdev numerically equal to another file's data block. Unlinking it
  preserves the block allocation and the other file's contents after remount.
- `ktest:lumen_kernel::vfs::ext4::ext4_fifo_mknod_descriptor_fault_never_frees_foreign_blocks`:
  descriptor-read fault injection inspects the inode at the failure
  boundary (either `EXTENTS` or zero `i_block`), verifies unchanged block
  allocation and reclaimed inode accounting, and remounts with no FIFO.
- `ktest:lumen_kernel::linux::syscall::fd::pipe_end_outlives_its_close_while_a_write_is_in_flight`
  (C7): a writer blocked on a full pipe (its wait confirmed before the
  close) keeps its end across another thread's `close` of the last
  descriptor; polling the reader still returns `IN` without `HUP`;
  the reader drains, receives every byte the writer then
  completes, and sees `EOF` only after the write returns. It kills the
  mutant that releases the end at the last `dec_fd_ref` alone.
- `ktest:lumen_kernel::linux::syscall::fd::fifo_reopen_during_a_blocked_read_reaches_that_reader`
  (C7): a read blocked on an empty `O_RDWR` FIFO (its wait confirmed
  before the close) while another thread closes the last descriptor; a
  fresh `O_WRONLY` open of the same FIFO then writes, and the blocked read
  returns those bytes, so the reopen joined the same `PipeObject`. It
  kills the mutant that lowers `opens` at the last `dec_fd_ref`.
- `scenario:test_linux_io::test_poll_and_select_on_console` (B3): `poll`,
  `ppoll`, `select` and `pselect6` on the console's stdin with and without
  typed input; timeouts at least their time; the time left written back;
  `POLLNVAL`; and `serial poll` confirming blocked waits woke via IRQ 4
  (`woken >= 1`), `safety-step hits: 0`, and `subscribers: 0` on exit.
- `scenario:test_linux_io::test_pty_termios_and_job_control` (C9): a
  `tools/linux-m8` program opening `/dev/ptmx`, a child on the slave as a
  session leader: canonical and raw reads, echo, `VMIN`/`VTIME`, Ctrl-C as
  `SIGINT` to the foreground group, `TIOCSPGRP` and a background read's
  `SIGTTIN`, a background `TCSETS`'s `SIGTTOU`, an orphaned group's read
  `EIO`, `TIOCSCTTY` and `TIOCNOTTY`, `SIGWINCH`, the session leader's
  exit sending `SIGHUP`, the master's close hanging the slave up and
  removing `/dev/pts/N`.
- `scenario:test_linux_io::test_console_ash_job_control` (C10):
  interactive `ash -i` on the serial console: a pipeline, a background
  job with `&` and `jobs`, Ctrl-Z stopping a foreground job and `fg`
  continuing it, Ctrl-C ending one without ending the shell, `stty`
  changing the discipline. Ctrl-Z and Ctrl-C are sent only after the job
  has printed a "ready" line. The VT consoles' ttys get one check: a line
  typed on VT 1 through `gui.type_confirmed` is read back through its
  tty.
- The gates (D1–D4):
  - D1: `scenario:test_alpine::test_ash_scripts_and_pipelines` (busybox
    `ash` scripts with pipelines, redirections, subshells, `$(...)`,
    traps, `wait` and background jobs);
  - D2: `scenario:test_alpine::test_gcc_gpp_compile_cpp_and_shared_lib`
    (the dev image, 512 MiB: `make -j1` builds C++ with exceptions and a
    `.so` and runs them, then `make -j2` on two trivial rules). D2 adds a
    low-water mark of free frames to `mem` (with a ktest,
    `ktest:lumen_kernel::mm::frame::low_water_mark_tracks_the_minimum`) and
    checks after the build that it never fell below 64 MiB;
  - D3: `scenario:test_alpine::test_local_git_and_sqlite3` (git
    init/commit/log/diff/merge; sqlite3 with WAL and POSIX locks) and
    `scenario:test_alpine::test_vim_edits_a_file_over_pty` (vim `-u NONE
    -N -c 'set ttimeoutlen=0 showmode'` under `ptyrun`, keys typed over the
    console, each step waiting in the byte stream for `-- INSERT --` or
    the `written` message, the file checked after);
  - D4: `scenario:test_conformance::test_musl_libc_test_core` (below).
- C5 updated `tests/scenarios/test_linux_user.py` to check signal exits,
  including `exit=135 signal=7` for file-backed bus errors.

- **The developer image (#344, merged in #366).** `make deps-dev`
  fetches the 34 packages `tools/alpine-packages.lock` pins (gcc, g++,
  make, musl-dev, binutils, git, sqlite, vim and their closure; 95.3 MiB
  of `.apk` files, sha256-checked, 94.8 MiB once zstd-compressed in a
  cache) into `build/deps/alpine-packages/`, and `make rootfs-dev`
  assembles `build/alpine-dev.ext4` deterministically (353 MiB, 289 MiB
  used, 3.6–4.8 s). Each gate scenario copies it with
  `rootfs.copy_dev_rootfs` (a clone on APFS, btrfs or XFS: 3 ms on macOS;
  a full copy on CI's ext4: 0.14–0.20 s) and boots it with 512 MiB of
  guest RAM (`Machine(memory="512M")`; the harness's default is 256 MiB).
- **Guest RAM for D2: 512 MiB**, replacing the M8 plan's 1 GiB (the old
  ROADMAP row gave a range, 512 MiB–1 GiB). #344's costs of a `make -j1`
  build of its C++ demo at its peak (`cc1plus -O2` running), with section
  8's loader (segments copied into private frames, no whole-file heap
  buffer, so #344's 40 MiB heap high-water row is gone):

  | Consumer | MiB |
  |---|---|
  | `cc1plus`'s anonymous memory (heap, stack) | 84 (172 for a file including `<bits/stdc++.h>`) |
  | `cc1plus`'s image, copied into private frames at `execve` | 41 |
  | `ext4` page cache (32 after reclaim, up to the 64 MiB hard limit) | 32–64 |
  | block cache | 4 |
  | the `g++` driver, `make`, `sh` | about 6 |
  | tmpfs `/tmp` (gcc's temporaries) | 0.2 |
  | **user and cache total** | **about 167–199 (255–287 for the header file)** |

  The firmware keeps some of the 512 MiB, leaving about 450–480 MiB
  usable, and the kernel's own memory (image, frame bitmap and reference
  counts, page tables, stacks, heap) is taken as 30 MiB: a margin of at
  least 220 MiB for the demo and at least 130 MiB for the header file
  (both from 450 MiB). The `make -j2` step adds no compile (its rules run
  `true`); a `-j2` build of two demo files would add about 125 MiB (a
  second `cc1plus`'s 84 and its own 41 MiB image copy) and still fit, but
  the gate doesn't run one. 1 GiB would be needed only for parallel builds
  of heavy files. The numbers are #344's host measurements; D2's free-frame
  low-water mark (at least 64 MiB, above) checks them in the guest.
- **How CI caches it.** The fetch is the one slow step (33–44 s from
  dl-cdn on #344's Linux host, 49 minutes on a slow route from macOS).
  Since #369, `ci.yml` caches the pinned zig and libc-test sources in an
  entry of their own, in four steps: restore; fetch; save only after a
  successful fetch on a miss; build. Since #399 the developer image's
  packages have their own entry the same way, in five steps: the durable
  mirror's check first, before the restore (below), then restore, `make
  deps-dev`, save and `make rootfs-dev`. Its key is
  `hashFiles('tools/alpine-packages.lock')`, with `alpine-packages-` as a
  restore key: the fetch keeps the verified files and prunes the rest, so
  a lock bump downloads only what changed. The fetch step sets
  `LUMEN_APK_MAX_TIME` (300 s), each try's total time, so a dl-cdn that
  trickles still gives way to the durable mirror. The existing entry's
  path is `build/deps/*` less the others' (`!build/deps/zig`,
  `!build/deps/zig.lock`, `!build/deps/zig.tmp.*`, `!build/deps/libc-test`,
  `!build/deps/alpine-packages`), so no file sits in two entries; a bare
  `build/deps` would be one match, archived whole, which no exclusion
  trims. The image itself is not cached: it is rebuilt from the packages
  on every run in about 5 s, so `test_rootfs_dev`'s tests of the built
  image run in CI too (and fail there without it, rather than skip). The
  cache alone is not durable: Alpine drops a superseded `-rN` package from
  dl-cdn, and an evicted cache entry would then fail the fetch. The
  durable mirror is the repository's `alpine-apk-mirror` release (a
  prerelease), every locked `.apk` an asset named `<sha256>.apk`;
  `tools/mirror-alpine-packages.sh`'s policy is never to replace or delete
  an asset, so an older lock's files stay fetchable.
  `tools/fetch-alpine-packages.sh` tries `LUMEN_APK_MIRROR` (none unless
  set), then dl-cdn, then `LUMEN_APK_FALLBACK` (unset: the release), every
  file sha256-checked whichever source served it.
  `tools/mirror-alpine-packages.sh upload` fills the release from
  `build/deps/alpine-packages/` (the lead runs it in a lock-changing PR),
  checking every missing file against the lock before it uploads any. CI
  runs its `check` first: it reads the release's asset list, failing on
  each locked file the release lacks or holds only as a partial or
  mismatched upload, then downloads the first locked file's headers
  anonymously at the fetch's URL. So CI fails a lock change until its
  files are mirrored. The check's failure doesn't hide the job's other
  results: the step continues on error, and the job's last step fails the
  job. CI checks only the current lock, so an older lock's files are
  mirrored best-effort, and a lock bump from a fork stays red until the
  lead uploads its files. The release has two limits: GitHub caps a
  release at 1000 assets, and the repository's "immutable releases"
  setting would refuse new uploads to a published release. Either way the
  remedy is a new tag, and a change to the fetch so that its durable
  mirror names both releases (today `DURABLE_MIRROR` is one URL, which
  the advice and the 403 rule compare against).
- **libc-test (D4; zig pinned by #368, the build and harness by #369).**
  `make libc-test` builds libc-test's `src/functional` and `src/regression`,
  static and dynamic, with the pinned zig into `build/libc-test/` (284
  tests). D4 stages that directory into an image as `/libc-test` (D4
  decides which image), boots it, and runs each test under libc-test's own
  `runtest` (which forks, `execve`s, and times each out with
  `sigtimedwait` and `kill`, so D4 needs C1, C2, C5 and C6), printing an
  `EXIT <path> <code>` line after each, as `lumen_harness.libctest`'s
  driver does natively (`runtest -t 30`: 30 s is that harness's choice,
  runtest's own default is 5 s). The harness parses the transcript into
  pass, fail and error, and compares it with
  `tests/conformance/libc-test-xfail.txt` (each entry a test's path, a
  reason and an issue; a runner error, which `lumen_harness.libctest`
  lists, is never covered by one; a listed test that passes fails the
  run; a test with no `EXIT` line counts as not run). The list is empty
  today. 274 of the 284 pass natively on CI's x86-64 Linux (`NATIVE_PASS`
  in `tests/harness/test_libctest.py`). D4 adds to the list, each with an
  issue: the 10 that fail natively too (`mntent`, `strptime`, `wordexp`,
  `regex-backref-oob` and `regexec-backref`, each static and dynamic:
  most likely libc-test's tip checking musl fixes newer than 1.2.5;
  tracked by a musl-bump issue); `pthread_mutex_pi` and
  `pthread_mutex_pi-static` (PI futexes stay `-ENOSYS`, section 8; a
  backlog issue); and each test Lumen fails. `runtest -t 30` is a ceiling
  on the guest's clock, which a busy host stretches, so D4 extends the
  harness: a timed-out test is its own class and is run once more before
  it counts as failed, and an entry may be marked `flaky` (it covers a
  fail or a timeout, never an error, and is exempt from the
  listed-pass-fails rule), each with its issue; its self-tests are
  `harness:test_libctest::test_a_timed_out_test_is_run_once_more` and
  `harness:test_libctest::test_a_flaky_entry_covers_a_fail_or_timeout_never_an_error`.
- **Which gates need the heavy tier: none in M8.** Every M8 gate runs in
  the PR tier under KVM: D1 (busybox scripts), D2 (one `-j1` C++ build of
  #344's demo, 0.85 s on the host, and a `.so`), D3 (git, sqlite3, vim)
  and D4 (284 short tests). #344 measured host times only: no guest
  timing exists yet, so D2's PR records its KVM CI time in its
  description, and if it passes a third of the job's 30-minute limit, the
  move to section 15's heavy tier (a `tier = "heavy"` feature,
  `make test-heavy`, which don't exist yet) becomes an issue of its own.
  The gates' outer timeouts are generous (the busy-host rule allows only
  outer limits); the TCG re-run's 10-minute step may not finish D2 or D4
  after a failure, which it annotates and doesn't fail on.

QEMU exit codes: 33 = tests passed, 35 = a test failed (or `lumen.ktest=` selected none), 37 = crash report.
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
  src/arch/        HAL; x86_64/ has gdt, idt, irq, context (thread switch), fpu (FXSAVE64/XSAVE64), syscall (SWAPGS, LSTAR, sysretq), apic, pic, pit, i8042 and cmos (ports), reset (reset and PM1 registers), serial, stacks (registry), cpu, qemu, paging, percpu (GS base)
  src/acpi.rs      ACPI discovery (MADT, HPET, MCFG, FADT and the DSDT's _S5_) through lumen-acpi
  src/pci/         PCI discovery over ECAM (lumen-pci): pci::devices, the registry (BARs, claims), pci::claim, DeviceResources (mod.rs); MSI-X, DeviceResources::msix (msix.rs)
  src/drivers.rs   the driver table and probe_all
  src/blk/         (M7, designed in #246) virtio-blk driver implementing BlockDevice, block-device registry, `blk` shell command
  src/vfs/         (M7, designed in #246) POSIX VFS kernel integration, unified PageCache, writable ext4 adapter, tmpfs, devfs, procfs, `mount`/`sync` commands
  src/linux/       Linux x86-64 ABI (M6 built #232–#238; M7 extends #246): UAPI re-exports (mod.rs), Limine boot-module loader (modules.rs), Process/UserThread + PROCS + FD table + console bridge (process.rs), pid 1 (init.rs), syscall dispatcher (syscall/mod.rs; handlers by cluster in syscall/{fd,stat,ns,dir,meta,io,lock,mem,proc}.rs; the file syscalls' shared argument decoding, path copy and stat fill in syscall/at.rs)
  src/user.rs      ring-3 entry/exit runner, `.lumen_fixup` safe user copies (`copy_from_user`/`copy_to_user`/`copy_cstr_from_user`), user exception handling (M6)
  src/sound.rs     the virtio-sound driver (its thread owns the device), beep, play, volume, sound
  src/virtio.rs    the adapter for virtio-drivers: the Hal (DMA runs, bounce buffers), transport, VirtioTransport
  src/power.rs     reboot (0xCF9, 8042, triple fault) and ACPI S5 power-off
  src/interrupts.rs  interrupt controllers: 8259s masked, local APIC, I/O APICs, routing ISA IRQs
  src/serial_in.rs COM1's input by interrupt (IRQ 4) into the queue the serial shell blocks on
  src/keyboard.rs  the PS/2 keyboard: 8042 init, IRQ 1, decoding (pc-keyboard), lumen.keymap
  src/time.rs      TSC and local APIC timer calibration, the boot origin, now_ns, sleep, ticks
  src/smp.rs       starting the other CPUs, run_on
  src/sched.rs     kernel threads: per-CPU run queues (each change an X-ray runq event), preemption, sleep, wait queues
  src/boot.rs      Limine requests
  src/cmdline.rs   kernel command line (flags, key=value)
  src/sync.rs      SpinLock (interrupts off while held; a wait an X-ray lock event) and SleepMutex (interrupt-enabled sleeping mutex, M6)
  src/klog.rs      logger (levels, subsystem tags, serial timestamps, the ring dmesg reads)
  src/rtc.rs       the CMOS RTC: date and time (taken as UTC) for `date`
  src/fb.rs        framebuffer: pixels, glyphs, the hello screen (draw_header), the crash screen
  src/desktop.rs   the desktop (#153, #154): the compositor thread, windows (open_window, WindowHandle), desktop, windows, the themes (theme), the wallpaper
  src/app.rs       WASM apps (M5): the embedded modules (app::BUNDLED, app::TEST_APPS, from kernel/build.rs); app/runtime.rs the runtime (#199: the table, loading on the app's thread, fuel slices, end reasons, kill, the reaper, `apps`/`run`/`kill`), app/host.rs the `lumen` host functions, app/tests.rs its ktests; wasmi in fuel slices (run_in_slices), `wasm` and its ktests over the hand-assembled modules in app/modules.rs (#197)
  src/font.rs      the desktop's anti-aliased text (lumen-font): the UI font parsed once and shared (font::face), font::Text (a thread's glyph cache, drawing glyph by glyph), the golden sample for `font` and its ktest
  src/screenshot.rs  `screenshot`: the screen as a PNG (lumen-png) in a base64 frame on serial
  src/term.rs      the virtual terminals on the framebuffer (lumen-term): their grids, drawing the shown one's changed cells, log lines, the cursor, the canvas
  src/vt.rs        the keyboard's keys to the shown virtual terminal's shell, Alt+F1 to F4
  src/symbols.rs   kernel symbol table (symbolized crash reports)
  src/crash.rs     crash reporter
  src/ktest.rs     in-kernel test registry and runner
  src/shell/       the shells (lumen-shell): serial and the virtual terminals' consoles, the commands, system commands (sysinfo.rs), deliberate faults (crash.rs), trace runq/xray (xray.rs)
  src/mm/          memory management: frame allocator (frame.rs), page tables (paging.rs), heap (heap.rs), kernel stacks (stack.rs), window mapping (window.rs), user AddressSpace & demand paging (user_vm.rs, M6)
  linker.ld        layout at -2 GiB, .ktests section, .lumen_fixup in :rodata (M6)
crates/            no_std, host-testable libraries (lumen-acpi, lumen-app-api, lumen-calc, lumen-clock, lumen-demo, lumen-edit, lumen-elf, lumen-font, lumen-gfx, lumen-heap, lumen-input, lumen-mm, lumen-pci, lumen-perf, lumen-png, lumen-queue, lumen-rand, lumen-shell, lumen-snake, lumen-synth, lumen-term, lumen-time, lumen-trace, lumen-uart, lumen-ui, lumen-view, lumen-vm, lumen-wm, lumen-xray; M7 adds lumen-bcache and lumen-vfs; M8 adds lumen-futex, lumen-pipe, lumen-proc, lumen-signal and lumen-tty)
assets/fonts/      the desktop's UI font (Atkinson Hyperlegible, SIL OFL 1.1) with its licence and source
assets/wallpaper/  the desktop's wallpaper (original, made by tools/wallpaper.py; README.md)
boot/limine.conf   bootloader config template
tools/             fetch-deps.sh, mkiso.sh, mkrootfs.sh (M7), mkrootfs-dev.sh and fetch-alpine-packages.sh (the developer image, #344), mirror-alpine-packages.sh (its packages' durable mirror, #399), fetch-zig.sh and fetch-libc-test.sh/build-libc-test.sh (libc-test, #368, #369), linux-hello/ (M6), linux-m7/ (M7), linux-m8/ (M8, designed in #336), ext4-signed-hash.sh (mkrootfs.sh's #308 switch), ext4-unsigned-hash-image.py (makes kernel/src/vfs/testdata/unsigned-hash.ext4, #308), wallpaper.py (makes the wallpaper), harness/ (Python package)
vendor/            patched copies of crates.io dependencies, via [patch.crates-io] (ext4plus, #313)
tests/             scenarios/, harness/, golden/, features/*.toml
docs/              ARCHITECTURE.md, ROADMAP.md, STATUS.md (generated)
apps/              the WASM apps (M5): their own cargo workspace, no_std cdylibs for wasm32-unknown-unknown, built by `make build` into build/apps/<name>.wasm (the runtime's test apps, test-<name>, into build/apps/tests/<name>.wasm; testkit, the library they share)
```

**The apps' target (#196).** `wasm32-unknown-unknown` is in the pinned
toolchain (`rust-toolchain.toml`). `apps/` is a separate workspace (every
directory in it but dot ones a member, each named after its crate; the
root workspace excludes it; output in `target-apps/`), so the host
crates' `cargo test` and clippy and the kernel builds never see it; `make
lint` and `make fmt` cover it. `make build` builds the apps, so `make
audit` (which runs `build`) does too, but the audit's checks look only at
the kernels. The default release profile keeps the name section (function
names for trap reports). The harness validates each built module with
`wasmtime` with the proposals past rustc's defaults that wasmtime-py can
switch off switched off (SIMD, threads, tail calls, memory64, ...;
extended-const has no switch, so only the next check catches it), checks
the app interface's shape (`lumen` imports only, `memory` fixed and
`lumen_main: [] -> [i32]` exported, a manifest), and checks that each module's `target_features`
section lists exactly rustc 1.99.1's default features for this target: the
list is `DEFAULT_FEATURES` in `tests/harness/test_wasm_module.py` (six that
`rustc --print cfg --target wasm32-unknown-unknown` shows, and two more
that LLVM's generic CPU enables). The kernel's `wasmi` must accept every one: wasmi 2.0.1's default `Config` enables them all, and ran a rustc-built module on the host in #197's probe; in the kernel, #197's hand-assembled modules use only nontrapping-fptoint and bulk memory (`memory.fill`), and the apps-built modules come with #199. A
toolchain bump that changes the list fails that test, so it gets checked
again.
