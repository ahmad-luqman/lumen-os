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
| Third-party crates | Prefer a maintained `no_std` crate over writing our own when it does the job: pinned with `=` in the workspace `Cargo.toml` (`Cargo.lock` committed), a PR adding only the crates its issue names (with their own dependencies), each checked first to build for `x86_64-unknown-none` on the pinned stable toolchain, and `make audit` must pass with it linked (no SSE/AVX, no red zone, the kernel code model). Kernel crates so far: `limine`, `rustc-demangle`, `noto-sans-mono-bitmap` and `log` (from M0, before this policy; `log` is still `0.4`, to be pinned in a PR of its own), `pc-keyboard` (scancode decoding and layouts, #58, the first under it), `vte` (terminal escape parsing; brings `arrayvec` and `memchr`) and `unicode-width` (character widths), both #59. `rhai` (scripting, #67; integers only, no modules, no time, no closures; brings `ahash`, `hashbrown`, `allocator-api2`, `equivalent`, `foldhash`, `smallvec`, `smartstring`, `thin-vec`, `num-traits`, `libm`, `bitflags`, `once_cell`, `portable-atomic`, `zerocopy`, `static_assertions`, `cfg-if`, `no-std-compat` and `core-error`, plus `const-random`, whose compile-time random seed stays linked but unused (the kernel sets a fixed hashing seed before any script runs), and the proc macros `rhai_codegen` and `const-random-macro`, which run only at build time).  `shlex` (#62) is only a dev-dependency of `lumen-shell`, the word splitter's test oracle; `noline`, which #62 names, isn't used (no completion hook, escape codes always, and it queries the terminal for the cursor on every prompt). | Reach working, playable features sooner; crates with many users are better tested than a first version of our own. | Writing everything ourselves (more to learn, much slower); unpinned versions (a minor release could break the build or bring in SSE). |
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
Drivers are reached only through their trait (section 7). Pure logic that does
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
5. Logger (level from `lumen.log=`), Limine responses checked, the symbol table
   located in our own ELF, a memory-map summary, the frame allocator
   (`INFO  mem: frames: N usable, M reserved`), the kernel's own page tables
   (`INFO  mem: paging: kernel page tables active ...`; the boot CPU no longer
   uses Limine's after this, the APs until they switch, and CPUs past the
   cap for good) and the heap.
6. Kernel stacks from the allocator, each with a guard page: the three IST
   stacks are installed, then `kmain` switches from Limine's boot stack to
   the 128 KiB main stack (`INFO  mem: stacks: ...`) and continues in
   `kmain_on_kernel_stack`: ACPI discovery (below); the interrupt
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
   framebuffer hello screen and the terminal below it (`INFO  term:
   terminal 176x41, 500 lines of scrollback`, section 6, "Terminal"; log
   lines before it are on serial only), the PS/2 keyboard (`INFO  kbd: PS/2 keyboard
   on ISA IRQ 1 ...`, section 6, "Keyboard"), then
   `boot complete`, then the boot timeline (below).
7. Either the in-kernel test runner (`--features ktest`) or the shells
   (`shell::run`, section 6, "Shell"): the screen's in a thread of its own,
   then the serial one on the `main` thread (`INFO  shell: shells on serial
   and the screen; type 'help'`). They run in the kernel until the shell
   moves to user mode in M9.

### Boot timeline and log timestamps (built in M1)

`kmain`'s first statement reads the TSC (a bare `rdtsc`, which can't fault,
so GDT and IDT setup still come first in effect): the origin of both the
boot timeline and the log's timestamps. The pinned Limine crate has no
bootloader-performance request, so the bootloader's own time isn't counted.
Entry is recorded with the origin's TSC; every other phase records a
`BootPhase` trace event when it ends: arch-init, logger, symbols, memory, stacks, acpi, interrupts,
time (calibration, interrupts on, the first-tick check: 10 ms, up to 100 ms while no tick has come), smp (the
wait until the other CPUs are online, then the scheduler started on every CPU), framebuffer, complete (which includes setting up the keyboard). After `boot complete` the kernel prints them from the ring,
converted with the calibrated TSC:

```
[    0.139423] INFO  boot: timeline: entry +0.000 ms, arch-init +0.145 ms, ..., complete +139.382 ms (total 139.382 ms)
```

A phase with no event is named as missing, one stamped before entry or
before an earlier phase says so, and the total is `complete`'s; an
incomplete timeline is logged as a warning (not tested: every boot's
timeline is complete; the scenario fails on the warning). Serial log lines
start with the time since that origin, `[seconds.microseconds]`; every line
before the TSC is calibrated (before `time: tsc ...`) shows `[    ?.??????]`, not a
zero that would read as a measurement. The screen's terminal shows lines
up to `info` without timestamps, so screenshots stay comparable
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
1.0 firmware, or when the XSDT address is 0), then reads the MADT and the
HPET table. The parsing is in the host-tested `lumen-acpi` crate:
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

Kernel command-line flags: `lumen.qemu_exit` (allow ending QEMU through
`isa-debug-exit`, used by tests), `lumen.log=<error|warn|info|debug|trace>`
and `lumen.keymap=<us|uk|de|fr|no|se|jp|dvorak|colemak>` (the keyboard's
layout, section 6; `us` by default and for a name it doesn't know, with a
warning) and `lumen.ansi` (the serial shell edits with escape codes, its
terminal taken to be 80 columns wide; plain text without it, section 6,
"Shell").

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
  arrive); 0x30-0x3e unused (a ktest raises 0x30); 0x3f the `WAKEUP` IPI
  (section 4, "Other CPUs"); 0x40 the local APIC
  timer; 0x50 + n global system interrupt n (up to 0xef), once a driver
  routes it (`interrupts::route_isa_irq`; every other I/O APIC input stays
  masked); 0xfe local APIC
  errors; 0xff the local APIC's spurious vector; the rest unused.
- **Dispatch:** `#BP` is counted and resumes (a ktest proves registers
  survive the round trip); every other exception is fatal and goes to
  `crash::exception`. Exceptions are traced. Device interrupts are never
  fatal: the timer counts a tick (per CPU; `time::ticks` is the boot
  CPU's), sends the local APIC an EOI and runs the scheduler's tick (which
  may switch threads); spurious interrupts (the local APIC's, the 8259s' IRQ 7 and 15) are
  counted and get no EOI (a spurious IRQ 15 gets one at the master 8259); a
  local APIC error is counted, logged with its status (the 1st, 2nd, 4th,
  8th... time, and whenever a new status bit appears) and EOI'd; a routed
  I/O APIC vector runs the handler its driver registered and is EOI'd;
  anything else is traced, counted, warned about once per vector, and EOI'd (another
  8259 line also masks the 8259s again; an 8259 vector the local APIC has
  in service is EOI'd there). Of device interrupts, only that last kind is
  traced: at 1 kHz the timer would flood the ring. The shell's `ticks` shows the
  counts (the boot CPU's timer), and boot warns if no tick arrives within 100 ms (it waits 10 ms,
  then up to 90 ms more while none has come, and logs a late one at info).
- **Interrupt state:** interrupts are off through boot and turned on once
  the controllers and the timer are set up. Every gate is an interrupt gate
  (handlers run with interrupts off). `SpinLock` keeps them off while held
  (`arch::irq::push_off`, whose `IrqOff` guard nests by depth per CPU, so guards
  dropped in any order never turn them on early, and can't be sent to
  another thread), so a maskable interrupt's handler never spins on a lock
  the code it interrupted holds: it may log, and is allowed to allocate.
  The crash path turns them off for good before anything else; no guard
  turns them back on, on any CPU. The lock word is the holder (free, or
  the holding CPU's index + 1), so a CPU that takes a lock it already holds
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
  was read too late to place, is set aside and another taken, up to 20 for
  five; boot logs how many if any, and the last. A PIT read-back that
  can't fit how it was programmed is an error. The local APIC timer (divide
  by 16) is calibrated against the TSC as the median of five 10 ms windows
  too (no retakes: each end reads the timer's count next to the TSC, so a
  late look doesn't stretch it, but a pause between those two reads isn't
  detected), and set periodic at 1 kHz. `time::now_ns` (TSC-based, lock-free) and
  `time::sleep` (blocks the thread once the scheduler runs on its CPU,
  section "Kernel threads"; busy-waits before that, on an idle thread, with
  interrupts off or a lock held), `time::spin` (always busy). The boot CPU
  publishes the timer's count; every other CPU starts its own timer with
  it, and the first tick starts that CPU's scheduler, and ticks are counted per CPU (`time::ticks`
  is the boot CPU's). The shell has `ticks`,
  `sleep <ms>`, `uptime`, and `clocks` (the TSC measured again against the
  PIT and the HPET, which the tests compare with the boot calibration: a
  wrong calibration would cancel out in tick counts, since the timer is
  calibrated against the TSC). A second line lists the windows behind each:
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
  implausibly or has no five steady windows in 20 is warned about (with why)
  and the PIT used; a PIT whose
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
  without a crash of their own. `!!!` lines name the rest: a CPU that didn't
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
  a ktest hands a turn between two CPUs 6000 times). Dropping a queue with
  threads waiting panics, as does a thread joining itself (not tested).
  Where a thread can't block, `wait_until` panics (not tested) and
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
- **Not yet:** priorities, migration or load balancing, a per-thread CPU
  time count (`top`, #63), and timeouts on wait queues.

### Keyboard (built in M2)

`kernel/src/keyboard.rs`, the 8042's ports in `arch/x86_64/i8042.rs`.

- **Routing:** `interrupts::route_isa_irq(irq, handler)` (boot CPU only,
  asserted) takes the MADT's interrupt override for the ISA IRQ, if any
  (the GSI, polarity and trigger; ISA's edge, active high without one),
  finds the I/O APIC with an input for that GSI, registers the handler for
  vector `0x50 + gsi` (`idt::set_irq_handler`, while the input is still
  masked) and programs the input masked, for the boot CPU. It returns a
  `Route`, which only it can make (routes are never undone); the driver
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
  another key pressed (arrows, PageUp, modifiers) with whether shift is
  held; releases are skipped, and bytes the decoder refuses are counted
  (the keyboard's overrun code among them). `keyboard::wait_key` blocks
  the calling thread until a key comes: the producer wakes the keyboard's
  wait queue whenever it queued a byte, and the waiter checks the queue
  under the wait queue's lock, so a byte can't slip between its check and
  its sleep (the screen's shell waits there: every keyboard scenario goes
  through it). The shell's
  `kbd` shows the layout, the route, the
  interrupts taken, the bytes queued, dropped, undecoded, flagged, and
  skipped during setup, or why there is no keyboard (and whether IRQ 1 was
  routed); a non-zero skip count is also logged after init.
- **Tests:** QMP `send-key` drives the keyboard (`Machine.type_text` and
  `press`; QEMU queues each key behind the last one, so they arrive in
  order; its input queue holds 1024 entries and drops the rest without a
  word, so `press` paces itself on the time QEMU takes to send each key,
  and `test_a_long_line_of_shifted_keys` checks 150 shifted keys arrive),
  and the scenarios read what the screen's shell did
  back from the screen (`lumen_harness.screen`, through the serial shell's
  `term text`); a scenario boots with
  `lumen.keymap=de` in an ISO built for it, and one presses a key over and
  over from power-on until the keyboard is set up (some of those bytes are
  skipped during setup).
- **Not yet:** key releases and Alt for programs, LEDs, the second port
  (a mouse), and hot-plug.

### Terminal (built in M2)

`crates/lumen-term` (the emulator, host-tested) and `kernel/src/term.rs`
(drawing it); `fb.rs` keeps the pixels, the header and the crash screen.

- **The emulator:** `Terminal::write` runs bytes through `vte`'s parser
  into a grid of cells (a character, foreground and background, each the
  default or one of 256 colours, and flags: bold, inverse, the two halves
  of a wide character). It handles UTF-8, C0 controls (a line feed
  without a carriage return: `term::write_str`, `term::write_fmt` and log
  lines turn `\n` into CR LF), cursor
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
  errors' and warnings' messages coloured too (a ktest reads the cells).
  The screen's shell writes there (section "Shell"), nothing of it on
  serial (except what a `crash` says on both: a fault that didn't happen,
  the heap's line before a heap crash; the screen half not tested). Each of its `write!`s and each log line is formatted and written under one lock, so
  another CPU's log line can't split an escape sequence written in one
  `write!` (ktests check the formatting happens under the lock); a log line
  starts from the default colours (a ktest leaves bold, inverse and a
  background set first). The arguments must not allocate (the heap asserts
  it isn't called under the screen's lock). A CPU that is already drawing (an
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
  and top row, cells drawn, and whether a command has the canvas: that
  part not tested) and `term text` (the view's rows, and the cursor counted in the view, or
  `cursor below the view` when scrolled back past it; tested); without a
  terminal, `term` says so (not tested). In the screen's shell
  Shift+PageUp/PageDown scroll the view half a screen (logged at debug
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
  a command on the other console holds (`reclaim_canvas`, with a warning;
  a ktest runs commands that leak a canvas, with and without another
  owner's held). A canvas taken back, or any after a crash report starts,
  draws nothing more (`Canvas::is_live`; a draw under way stops at its
  next row or glyph) and its drop leaves the terminal alone (ktests check
  the taking back; the crash case is not tested).
- **Not yet:** virtual terminals (Alt+F1 to F4, ROADMAP M2), a font with
  more than Basic Latin, underline and other SGR attributes, the
  alternate screen, origin mode, left and right margins, and resizing.

### Shell (built in M2)

`crates/lumen-shell` (host-tested) and `kernel/src/shell/` (the consoles,
`mod.rs`; the commands, `commands.rs`; the deliberate faults, `crash.rs`).

- **Two shells, one per console,** each with its own line and history:
  the serial shell on the boot CPU's `main` thread (where kmon ran), and
  the screen's in a thread of its own (`shell-screen`, on the boot CPU,
  started only with a terminal and a keyboard: without a keyboard boot
  says `shell on serial (none on the screen: no keyboard)`, tested with
  `i8042=off`; without a terminal, or no thread for it, not tested). The
  serial shell polls serial (no serial interrupt yet), yielding to
  the CPU's other threads between polls (sleeping instead would switch
  to the idle thread and trace twice a millisecond); the screen's blocks
  in `keyboard::wait_key`. Log lines go to both consoles.
- **Words** (`lumen_shell::words`): blanks separate words; `'...'` keeps
  everything; `"..."` too, except that a backslash escapes `$`, `` ` ``,
  `"`, `\` and a newline; outside quotes a backslash escapes anything;
  parts next to each other make one word, `''` an empty one; `#` at the
  start of a word starts a comment. A randomized host test checks it
  against `shlex` on 200,000 lines. An unclosed quote or a trailing
  backslash is one line of error (`syntax error: ...`), and so is an
  unknown command (`unknown command: x (try 'help')`).
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
  `term` `text`; options that aren't printable are skipped): one that
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
- **Keys:** the screen's from `keyboard::wait_key` (Enter `\n`, Backspace
  U+0008, Delete U+007F, Tab, Ctrl with a letter U+0001 to U+001A, the
  arrows, Home and End; a ktest maps each); serial's from bytes
  (`lumen_shell::input::Decoder`: CR, LF or CR LF as one Enter, DEL and BS
  as Backspace, other control characters as Ctrl and a letter, `ESC [`
  and `ESC O` sequences for the arrows, Home, End and Delete, a C0
  control (below 0x20) ending a sequence it interrupts, UTF-8 without
  overlong forms; host tested). The serial shell's writes are made holding
  the log's line lock (`klog::LockedSerial`, see step 4 of the boot
  sequence), so a log line can't land inside one (not tested on its own).
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
  `clear`, `colors`, `seq`, `sleep`, `uptime`, `clocks`, `exit` (kmon's;
  besides what this list says, the deliberate panic now says `from the
  shell`, and `crash <kind> <cpu>` reports a fault that returned) and
  `history` (the console's lines, numbered). Each writes to the console that ran it. On plain serial,
  `colors` and `clear` say the console is plain (tested), and with
  `lumen.ansi` they work there (tested). A `crash` whose fault doesn't
  happen says so on serial and the screen (the screen not tested). `crash <kind> <cpu>` waits, asleep and allocating nothing,
  until that CPU's report stops this one or its fault returns (so the
  report doesn't find the heap's lock held by the shell freeing its line;
  the screen's shell could still allocate if someone types there
  meanwhile, not tested). After 10 s (not tested) the job is taken back
  if the CPU hasn't taken it; while one is out, another is refused.
  `crash dfree` and `crash heapcorrupt` print the heap's line before the
  damage, which the tests compare with the report's.
- **Tests:** `cargo:lumen-shell`; `test_shell` (echo with quotes on both
  consoles, history and line editing on the screen and history on serial,
  completion on both, one line of error, `lumen.ansi`); the keyboard and
  terminal scenarios type on the screen's shell and read the screen back
  with `term text`; every serial scenario that drove kmon runs against the
  serial shell with the new prompt.
- **Not yet:** Ctrl-R search, Alt-B/F word moves, wrapping lines longer
  than the console, pipes, variables, and a shell in user mode (M9).
  Scripts: see below.

### Scripting (built in M2)

`kernel/src/script.rs`, the shell's `rhai` command, with
[rhai](https://rhai.rs) 1.27 (integers only, no modules, no time, no
closures; see the crates row in section 2).

- **Forms:** `rhai` alone is a REPL (the shell's line editor with its own
  history, prompt `rhai> `, Tab completing Lumen's functions; variables
  kept from line to line; `exit`, `quit` or Ctrl-D on an empty line
  leaves); `rhai -e <expr>` evaluates its words joined by spaces; `rhai
  demo` lists the built-in scripts (`kernel/src/script/*.rhai`: `rainbow`,
  `fib`, `count`; make rebuilds the kernel when one changes) and `rhai demo
  <name>` runs one, until M6 brings files. A value other than `()` is
  printed; an error is one line, `rhai: ...` (the innermost error, a limit
  in its own words, and `in function 'f'` when it happened in one).
- **Lumen's functions:** `cpus()` (each online CPU as `#{index, apic}`),
  `mem()` (`heap_size`, `heap_used`, `heap_free`, `heap_live`),
  `uptime()` (ms since the boot timeline's origin), `trace(n)` (the last n
  events of this CPU's ring, the boot CPU's, where both shells run; gap
  lines included), `sleep(ms)` (clamped to 0 to 60 s), `print` and `debug`
  (to the console), and drawing on the terminal's canvas:
  `draw_pixel(x, y, rgb)`, `draw_rect(x, y, w, h, rgb)`, `draw_text(x, y,
  text, rgb[, bg])`, `clear([rgb])` (negative coordinates are clipped; text
  past the canvas's right edge is cut). `screen_width()` and
  `screen_height()` give the canvas's size without taking it. The canvas is
  taken on the first draw (an error naming why if it can't be). A script
  that drew and succeeded shows `rhai: press a key` on a strip at the
  canvas's bottom and on its console, and keeps the canvas until a key is
  pressed on its console; one that failed (Ctrl-C included) gives it back
  first, so its error shows. Meanwhile, what it prints reaches the
  screen's terminal but is drawn only when the canvas is given back.
- **Its own thread:** each run (or REPL session) runs in a thread with a
  1 MiB stack (`sched::run_sized`, which waits for it, so the closure
  borrows the console; `Console` is `Send`). rhai recurses for calls,
  nested expressions and every walk over a nested value (clone, drop,
  print, size checks), so this bounds how deep those go (below) and the
  stack is twice what the deepest cases need: the ktests
  `deep_recursion_is_an_error_not_a_crash` and
  `nested_values_are_an_error_not_a_crash` run them on 512 KiB.
- **Limits** (`script::LIMITS`), each an error rather than a guard-page
  crash or an out-of-memory panic: 200 million operations (10 to 30 s at
  the 7 to 18 million a second measured locally; Ctrl-C is quicker), 16
  call levels, expression depth 24 (16 in functions), 256 variables in a
  scope, and per value (counted by rhai over the whole value, nested values
  included, whenever it checks: building, storing an item, passing,
  returning) 64 KiB of string bytes, 1024 array items and 256 map items.
  Each nesting level is an item, so values nest at most 1280 deep. rhai's
  `==`, `!=`, `to_string`, `to_debug`, `print` and `debug` on arrays and maps
  call back into the engine for every item (about 1.7 KiB of stack a level);
  Lumen replaces them with Rust walks (`script::equal`, `format_array`,
  `format_map`). `curry` is disabled and closures are compiled out: curried
  arguments nest without being counted. A key added to a map by assigning
  to it isn't counted until the map is checked again; the heap budget
  covers that: a run that grows the heap by more than 32 MiB (checked every
  64 operations, so it can overshoot by what those allocate; other threads'
  allocations count too) is stopped.
- **Ctrl-C** stops a script: the console is asked for a key at most every
  10 ms (the first 10 ms after the start, so keys typed ahead of a short
  script stay), from rhai's progress hook (every 64 operations), from every
  drawing call, `print` and `debug`, and while `sleep` waits; it is found
  inside closures and native callbacks too. Asking takes the keys waiting
  on the console up to a Ctrl-C; the others are dropped.
- **The console:** rhai's callbacks must be `'static`, so they reach the
  console through a slot (`ConsoleSlot`) shared with the command, which
  holds it only while an evaluation runs. Output is formatted before it is
  written: formatting a value allocates, which the heap refuses under the
  screen's lock (the screen console now formats every `write!` first).
- **Hashing:** rhai seeds its function hashes from a value `const-random`
  makes when rhai is compiled; the kernel sets a fixed seed before the
  first engine, so runs don't depend on it (the compiled-in value is still
  linked, so images differ by it between builds).
- **Tests:** `test_script` (`rhai -e` with `-smp 4`, the REPL, the demos,
  the `rainbow` golden and the terminal back after a key, Ctrl-C on serial
  and on the keyboard, the screen shell printing values whose formatting
  allocates, `screen_width()` without taking the screen, the strip and
  the terminal back); ktests for the functions, one-line errors and the
  limits' messages, the engine's limits, Ctrl-C (closures, functions,
  `sleep`, drawing, the `count` demo), the heap budget, recursion and
  nesting on half the script stack, drawing and waiting for a key, and the
  REPL going on after Ctrl-C and errors.
- **Not yet:** scripts from files (M6), floats, closures, scripts in user
  mode (M9), a script running in the background.

## 7. Driver interface (planned, M3+)

Drivers are kernel modules behind traits whose methods are **messages**: an
owned request goes in and a completion comes back, with no shared mutable
state across the boundary. That keeps drivers testable with fakes and lets a
driver move to a user-space server later by serializing the same messages over
a channel.

```rust
pub trait BlockDevice: Send + Sync {
    fn info(&self) -> BlockInfo;                       // sector size, count, read-only?
    fn submit(&self, req: BlockRequest) -> Ticket;     // non-blocking
    fn poll(&self, t: Ticket) -> Poll<BlockResult>;    // or a completion queue
}
pub struct BlockRequest { pub op: BlockOp, pub lba: u64, pub buf: DmaBuffer }
```

- **Discovery:** PCI enumeration (and later ACPI) produces `DeviceInfo`; drivers
  register `probe(&DeviceInfo) -> Option<Box<dyn Driver>>`.
- **Resources as capabilities:** a driver receives a `DeviceResources` value
  holding exactly its BARs (MMIO), its IRQ line or MSI vectors, and a DMA
  allocator. It cannot touch other devices, even in the kernel. These map
  one-to-one onto capabilities if the driver moves out.
- Device classes: `BlockDevice`, `NetDevice` (send/receive frames), `InputDevice`
  (event stream), `Display` (framebuffer + flush rectangles), `Console`.
- **virtio first** (blk, net, input, gpu, console) under QEMU; real-hardware
  drivers (NVMe, AHCI, xHCI HID, a common NIC) in M12 behind the same traits.

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
| `Device` | device resources (for drivers in user space) | mmio, irq, dma |

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

## 11. GUI and the automation API (planned, M4)

- **Compositor service** owns the display (`Display` driver). Clients draw into
  shared-memory surfaces and submit damage rectangles. It handles window
  stacking, focus and input routing.
- **Toolkit** (`lumen-ui`): a retained widget tree. **Every widget node is also
  an automation node**:

```rust
pub struct AutomationNode {
    pub id: NodeId,              // stable for the node's lifetime
    pub role: Role,              // Button, TextField, List, ListItem, Menu, Window...
    pub name: String,            // accessible label: "Save", "File name"
    pub value: Option<Value>,    // text, number, checked, selection
    pub state: StateFlags,       // focused, enabled, selected, expanded
    pub actions: Vec<ActionId>,  // "press", "set_value", "select", "scroll_to"
    pub children: Vec<NodeId>,
}
```

- **Automation protocol** (a channel per app): `get_tree(window)`,
  `invoke(node, action, args) -> Result`, `subscribe(changes)`, and app-level
  semantic actions (`files.copy(src, dst)`, `editor.replace(range, text)`) that
  an app may declare in addition to widget actions. Agents act through this
  protocol and hold an `AppActions` capability naming the app and the allowed
  actions. **Agents never get synthetic mouse or keyboard input.**
- The same tree powers accessibility (screen reader) and GUI tests. Golden
  screenshots plus tree assertions together make UI tests robust.
- **X-ray mode** is an app (and an overlay) that reads the `Trace` capability.

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
  previous thread's state). Later: syscalls, page faults, allocator
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
| Whole system | pytest scenarios: boot QEMU headless, `expect` on serial with timeouts, `send_line`, keys over QMP (`press`, `type_text`), exit codes; run in parallel (pytest-xdist, one worker per CPU) | `tests/scenarios` |
| Screens | QMP `screendump` compared to golden PNG regions | `tests/golden` |
| Codegen | `make audit`: disassembly and LLVM IR checks | `tools/harness/lumen_harness/audit.py` |
| Harness itself | self-tests that the TAP parser, audit and image diff catch failures, and that the QEMU command line, artifact layout, CI wiring and status generation behave | `tests/harness` |
| Feature gate | every feature in `tests/features/*.toml` must pass (CI); `docs/STATUS.md` generated from the same data | `make status-check`, `make status` |

QEMU exit codes: 33 = tests passed, 35 = a test failed, 37 = crash report.
A triple fault exits with 0 (`-no-reboot`) and therefore fails. Each scenario
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
  src/arch/        HAL; x86_64/ has gdt, idt, irq, context (thread switch), apic, pic, pit, i8042 (ports), serial, stacks (registry), cpu, qemu, paging, percpu (GS base)
  src/acpi.rs      ACPI discovery (MADT, HPET) through lumen-acpi
  src/interrupts.rs  interrupt controllers: 8259s masked, local APIC, I/O APICs, routing ISA IRQs
  src/keyboard.rs  the PS/2 keyboard: 8042 init, IRQ 1, decoding (pc-keyboard), lumen.keymap
  src/time.rs      TSC and local APIC timer calibration, the boot origin, now_ns, sleep, ticks
  src/smp.rs       starting the other CPUs, run_on
  src/sched.rs     kernel threads: per-CPU run queues, preemption, sleep, wait queues
  src/boot.rs      Limine requests
  src/cmdline.rs   kernel command line (flags, key=value)
  src/sync.rs      SpinLock (interrupts off while held)
  src/klog.rs      logger (levels, subsystem tags, serial timestamps)
  src/fb.rs        framebuffer: pixels, glyphs, the hello screen, the crash screen
  src/term.rs      the terminal on the framebuffer (lumen-term): drawing changed cells, log lines, the cursor
  src/symbols.rs   kernel symbol table (symbolized crash reports)
  src/crash.rs     crash reporter
  src/ktest.rs     in-kernel test registry and runner
  src/shell/       the shells (lumen-shell): serial and the screen's consoles, the commands, deliberate faults (crash.rs)
  src/mm/          memory management: frame allocator (frame.rs), page tables (paging.rs), heap (heap.rs), kernel stacks (stack.rs), window mapping (window.rs)
  linker.ld        layout at -2 GiB, .ktests section
crates/            no_std, host-testable libraries (lumen-elf, lumen-mm, lumen-heap, lumen-acpi, lumen-time, lumen-queue, lumen-term, lumen-shell, lumen-uart, ...)
boot/limine.conf   bootloader config template
tools/             fetch-deps.sh, mkiso.sh, harness/ (Python package)
tests/             scenarios/, harness/, golden/, features/*.toml
docs/              ARCHITECTURE.md, ROADMAP.md, STATUS.md (generated)
```
