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
| Target | x86-64 UEFI, QEMU `q35` + OVMF, virtio devices first | Closest to a modern laptop; virtio is simple and well documented. | BIOS (legacy), ARM64 first (M10 port instead). |
| Bootloader | Limine 12.x (pinned binary release + sha256), `limine` crate **0.5.0** | Memory map, framebuffer, higher-half direct map, and our own ELF file for symbolization, with no loader to write. Crate 0.6.x requires nightly (`#![feature(ptr_metadata)]`), so we use 0.5.0 (base revision 3). | Custom UEFI loader (optional learning milestone), GRUB/multiboot2. |
| Kernel shape | **Modular monolith** with message-shaped internal interfaces; capabilities enforced at the syscall boundary | One address space keeps early milestones fast and debuggable. Agent safety needs capability checks between *processes*, which a monolith enforces just as well. Drivers and services expose request/response interfaces, so any of them can move to user space later. | Microkernel: stronger isolation of drivers, but it puts IPC performance and many extra servers in front of every milestone. |
| Test harness | Python (`uv`, pytest) on the host; TAP from the kernel | Fast to write scenarios, mature tooling, and QMP/PIL are easy to use. | Rust `xtask`: one language, but slower iteration on test scripts. |
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
   top is passed to `kmain`, and the stack is aligned.
4. **`arch::init_early`**: COM1 serial, then the GDT and TSS, then the IDT
   with all 256 vectors. **From this point any CPU exception produces a crash
   report.** (#DF, NMI and #MC get their IST stacks in step 6.)
5. Logger (level from `lumen.log=`), Limine responses checked, the symbol table
   located in our own ELF, a memory-map summary, the frame allocator
   (`INFO  mem: frames: N usable, M reserved`), the kernel's own page tables
   (`INFO  mem: paging: kernel page tables active ...`; Limine's are no longer
   used after this) and the heap.
6. Kernel stacks from the allocator, each with a guard page: the three IST
   stacks are installed, then `kmain` switches from Limine's boot stack to
   the 128 KiB main stack (`INFO  mem: stacks: ...`) and continues in
   `kmain_on_kernel_stack`: the framebuffer hello screen, then `boot complete`.
7. Either the in-kernel test runner (`--features ktest`) or the kernel monitor
   (`kmon`, a serial debug console that the user-mode shell replaces in M2).

Kernel command-line flags: `lumen.qemu_exit` (allow ending QEMU through
`isa-debug-exit`, used by tests) and `lumen.log=<error|warn|info|debug|trace>`.

## 5. Memory layout

### Virtual address space (x86-64, 4-level paging)

| Range | Use | Status |
|---|---|---|
| `0x0000_0000_0000_0000 – 0x0000_7fff_ffff_ffff` | empty in the kernel's tables (null guard); user space, per process, from M2 (except the first 64 KiB) | built (empty) |
| `0xffff_8000_0000_0000 – +phys size` | higher-half direct map (HHDM) of RAM: usable, bootloader-reclaimable, kernel-and-modules and framebuffer entries; NX, RW- except kernel-and-modules (R--), framebuffer write-combining | built |
| `0xffff_c000_0000_0000 – 0xffff_c0ff_ffff_ffff` | kernel heap (grows up by mapping frames, to 1 GiB below the top; the top GiB holds a ktest scratch page) | built |
| `0xffff_d000_0000_0000 – 0xffff_d0ff_ffff_ffff` | kernel stacks: 64 KiB (the main stack 128 KiB), **unmapped guard page below every stack**; the top GiB holds a ktest page | built |
| `0xffff_e000_0000_0000 – 0xffff_e0ff_ffff_ffff` | MMIO mappings (uncached, RW-), `map_mmio` | built |
| `0xffff_ffff_8000_0000 – end of image` | kernel image: `.text` R-X, `.rodata` (and `.got`) R--, `.data/.bss` RW-, page-aligned segments | built |

The kernel is linked at -2 GiB because the **kernel code model** requires it
(all symbols fit in sign-extended 32-bit immediates). `make audit` verifies
this along with the absence of SSE/AVX and red-zone use (hard rule 3).

### Physical memory (built in M1)

The frame allocator (`kernel/src/mm/frame.rs`, logic in the host-tested
`crates/lumen-mm`) is built only from Limine's memory map. Only `usable`
regions are handed out. These are **explicitly reserved by name, even if
Limine labels them usable**: frame 0, the kernel image, the kernel ELF file
(the symbol table lives in it), the boot stack and the framebuffer (those four
found by translating their live virtual addresses through the page tables
rather than trusting the map's labels), the ACPI RSDP and the allocator's own
bitmaps. Every non-usable map entry (including the `acpi-*` entries, where
firmware normally puts the ACPI tables; #2 must reserve any table it finds in
usable memory, before anything allocates, since `reserve` refuses frames
already handed out) and every frame of Limine's page tables (active while
the allocator is built; the kernel's own tables are then allocated from it)
is reserved too. The named list is logged at `debug` level.

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
interface if contiguous allocation becomes hot.

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
memory map; Limine's page tables are unused after the switch).

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
  elsewhere. ACPI memory is not in the HHDM; ACPI parsing (#2) maps what it
  needs (`map_mmio` maps the RSDP already). Entries that share a page map it
  once; boot stops if they'd need different rights. A ktest checks every
  HHDM span is mapped throughout (sampled every 2 MiB) with its own rights
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
uncached, nothing mapped) and checks each is named. (The check skips the
allocated stacks, which a fresh copy doesn't map; at the real switch the
kernel still runs on the boot stack, so nothing is skipped then.) EFER.NXE and CR0.WP are set (so
read-only pages bind in ring 0 too) and the PAT layout the mappings rely on
(slot 0 WB, 3 UC, 5 WC, set up by Limine) is checked rather than rewritten.

The entry format, walks and mapper are pure logic in `lumen_mm::paging`,
tested on the host against fake table memory (including a randomized
comparison with a model). Table entries are permissive; rights are enforced
at the leaves, and walks report the rights combined over every level, which
is what the CPU enforces; `Attrs` can't express a writable and executable
page. The kernel API:

- `map`/`unmap` (`unsafe`): 4 KiB pages for the owners of the heap and stack
  windows (the heap and the kernel stacks). The caller promises it owns the frame and the page and
  that nothing refers through a page it unmaps; the functions refuse what
  they can check (addresses outside those windows, frames nobody allocated,
  non-write-back or executable rights, since allocator frames are writable
  through the HHDM). `unmap` invalidates the TLB entry. Everything else
  (image, HHDM, MMIO) is fixed.
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
(M2) will need the kernel's L4 entries 256-511 copied into every address
space, so new kernel L3 tables must be allocated up front then.

### Kernel stacks (built in M1)

Every stack the kernel runs on after `mm::init` comes from the allocator
(`kernel/src/mm/stack.rs`; layout and registry in the host-tested
`lumen_mm::stacks`). `KernelStack::new` takes addresses from the stack
window, upward, with an unmapped guard page below each stack (so the page
above a stack is the next one's guard), maps fresh frames there (RW, NX,
write-back; all or nothing, shared with the heap in `mm/window.rs`) and
registers the stack. Dropping one retracts it from the registry, then
unmaps it and frees its frames; addresses are never reused. Stacks the CPU
points at are leaked so they can't be dropped.

Boot: the IDT starts with every vector on the current stack. Right after
`mm::init`, `kmain` allocates the 64 KiB #DF, NMI and #MC stacks, puts them in
the TSS's IST and only then makes those three gates use them, allocates the
128 KiB main stack and switches to it (`arch::switch_stack`), which plants a
frame record so backtraces run from the new stack back into `kmain` and
`_start` on the old one. Limine's boot stack (128 KiB requested, 64 KiB
assumed if it doesn't confirm) stays reserved and registered for that walk
but is never run on again. Until the switch it is the one stack without a
guard page, and a #DF with a broken `rsp` before then is a triple fault
instead of a report.

The registry (lock-free, 32 slots) is what the crash reporter trusts: a
frame pointer is followed only into a known stack, frames move toward the top
of their stack except for one link into another known stack, and a #PF or
#DF whose CR2 is in a guard page is reported as `kernel stack overflow (guard
page hit)` with the stack's name and bounds. Every function carries inline
stack probes (`make audit` checks), so a frame bigger than a page can't jump
over a guard page.

### Kernel heap (built in M1)

`kernel/src/mm/heap.rs` is the `#[global_allocator]`; the algorithm is the
host-tested `crates/lumen-heap`. One contiguous span grows up from the
heap window's base: when no free block fits, the heap maps fresh frames at
its end (read-write, no-execute, write-back, through `paging::map`) and
appends them. It grows by its own size, between 64 KiB and 4 MiB, or by
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
state, including the last failed growth (size, alignment, reason); kmon's
`heap` command prints the same line. Lock order: heap, page tables, frame
allocator; nothing holding the last two may allocate. The allocator never
logs (a log argument that allocates would re-enter it under the logger's
lock) and formats its kmon and crash line from a copy taken under the lock. The lock is not IRQ-safe until #7, so interrupt
handlers must not allocate yet.

## 6. Interrupts, exceptions and crash reports (built)

- One assembler `.rept` loop generates 256 entry stubs, each 16 bytes apart.
  Stubs push a dummy error code where the CPU does not, then the vector, then
  jump to `lumen_isr_common`, which saves all 15 GPRs into a `TrapFrame` and
  calls `lumen_exception_dispatch`.
- `#BP` is counted and resumes (a ktest proves registers survive the round
  trip). Every other vector is fatal in M0 and goes to `crash::exception`.
- #DF, NMI and #MC run on their own guarded IST stacks (from step 6 of the
  boot, before that on the boot stack), so a corrupted or overflowed kernel
  stack still yields a report; an overflow is named as one (section 5,
  "Kernel stacks").
- **Crash report:** exception name and vector, decoded #PF error code and CR2,
  rip with symbol, all registers, CR0/3/4, a frame-pointer backtrace, then on
  serial only the last trace events, for a #PF the page-table walk of CR2,
  and last one line of heap state (never blocks: if the heap's lock is held
  it reads the state anyway and says so; the free-list walk is bounded and
  stays inside the heap). It is
  written to serial and to the screen. Frame pointers are forced on
  (`-C force-frame-pointers=yes`); the walker follows `rbp` only inside known
  stacks and only to return addresses inside kernel `.text`, so a corrupted
  chain cannot fault. Symbols come from our own ELF (Limine's executable-file
  response) via `crates/lumen-elf` and are demangled with `rustc-demangle`.
- A fault during the report prints one line and halts, with no recursion. With
  `lumen.qemu_exit` the kernel exits QEMU with status 37; otherwise it halts.
- Planned: "Explain this crash with Claude" in the GUI, which sends the same
  report text with the user's consent (M7).

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
  drivers (NVMe, AHCI, xHCI HID, a common NIC) in M9 behind the same traits.

## 8. Processes, syscalls and capabilities (planned, M2)

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

## 9. Storage: VFS, LumenFS and snapshots (planned, M3)

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
  image, and verify (M3 exit criterion).

## 10. IPC (planned, M2)

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

## 11. GUI and the automation API (planned, M5)

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

## 12. The Claude service (planned, M7)

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
  blocks, even from an exception handler that interrupted another record
  (simulated in host tests; not yet exercised in the kernel); readers skip
  events that change while they copy them, so they never see a torn event.
  Events are in `seq` order; timestamps can be out of order across nested
  writers. One 4096-event ring now (one CPU); one per CPU with SMP (#9).
  Recorded so far: boot phases, exceptions (every vector entering the IDT
  dispatcher), log lines. Later: scheduler switches, syscalls, interrupts,
  page faults, allocator activity. Cost per event is printed by
  `ktest:lumen_kernel::trace::trace_overhead_is_small` (tens of TSC ticks
  under TCG locally; TCG's TSC rate depends on the host, so this is not
  comparable across machines or to cycles). Timestamps are raw TSC ticks until
  #7/#8. `kmon`'s `trace [n]` prints the newest events (a gap line says why
  an event is missing), and the serial crash report includes the last 16
  events (tested for a page fault and a panic).
- **Boot timeline:** boot phases emit trace events. The fast-boot target is
  under 1 s from bootloader handoff to the shell, measured in the guest from TSC
  timestamps (never by host wall-clock time in tests).
- **Logger:** `log` facade with level and subsystem tags (built). Every
  emitted log line also records a `Log` trace event (level, tag).
- **Learning:** `explain <subsystem>` and a guided tour link each subsystem to
  its source and docs inside the OS (M5+).

## 15. Testing architecture (built)

| Layer | Tool | Where |
|---|---|---|
| Pure logic | `cargo test` on `no_std` crates | `crates/*` |
| Kernel internals | `ktest!` in-kernel tests → TAP over serial → `isa-debug-exit` | anywhere in `kernel/src` |
| Whole system | pytest scenarios: boot QEMU headless, `expect` on serial with timeouts, `send_line`, exit codes | `tests/scenarios` |
| Screens | QMP `screendump` compared to golden PNG regions | `tests/golden` |
| Codegen | `make audit`: disassembly and LLVM IR checks | `tools/harness/lumen_harness/audit.py` |
| Harness itself | self-tests that the TAP parser, audit and image diff catch failures | `tests/harness` |
| Feature gate | every feature in `tests/features/*.toml` must pass (CI); `docs/STATUS.md` generated from the same data | `make status-check`, `make status` |

QEMU exit codes: 33 = tests passed, 35 = a test failed, 37 = crash report.
A triple fault exits with 0 (`-no-reboot`) and therefore fails. Each scenario
keeps `serial.log`, `qemu.log` (`-d int,cpu_reset,guest_errors`) and
screenshots when it fails, and CI uploads them. QEMU records the interrupt
trace only under TCG, so CI (which runs KVM) re-runs failed scenarios under
TCG before uploading.

## 16. Repository layout

```
kernel/            the kernel (bin crate, x86_64-unknown-none)
  src/arch/        HAL; x86_64/ has gdt, idt, serial, stacks (registry), cpu, qemu, paging
  src/boot.rs      Limine requests
  src/crash.rs     crash reporter
  src/ktest.rs     in-kernel test registry and runner
  src/kmon.rs      serial debug monitor (until the M2 shell)
  src/mm/          memory management: frame allocator (frame.rs), page tables (paging.rs), heap (heap.rs), kernel stacks (stack.rs)
  linker.ld        layout at -2 GiB, .ktests section
crates/            no_std, host-testable libraries (lumen-elf, lumen-mm, lumen-heap, ...)
boot/limine.conf   bootloader config template
tools/             fetch-deps.sh, mkiso.sh, harness/ (Python package)
tests/             scenarios/, harness/, golden/, features/*.toml
docs/              ARCHITECTURE.md, ROADMAP.md, STATUS.md (generated)
```
