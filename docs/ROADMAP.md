# Lumen roadmap

Each milestone ends when its **exit criteria pass in CI**. Every criterion
names the automated test that proves it: `ktest:` is an in-kernel test,
`scenario:` a QEMU test in `tests/scenarios/`, and `cargo:` a host unit test.
Test names below are the intended names; the PR that implements a feature adds
the test and an entry in `tests/features/<area>.toml`. Current state:
[STATUS.md](STATUS.md).

Work is split into GitHub issues labelled with the milestone (`m1`, `m2`, ...)
and a component (`area:mm`, `area:arch`, ...). One issue is one branch is one PR.

---

## M0: Decisions, harness, first boot

- Boot via Limine to the logger: `scenario:test_boot::test_boot_reaches_logger`
- Deliberate faults produce symbolized crash reports, not resets:
  `scenario:test_crash::test_fault_produces_symbolized_report[pf,ud,de,gp,df]`,
  `scenario:test_crash::test_panic_produces_report`
- In-kernel TAP runner: `scenario:test_ktest::test_in_kernel_tests_pass`
- Golden screenshots: `scenario:test_screen::test_boot_hello_golden`
- Codegen flags proven: `audit:flags`
- CI on every PR; `docs/STATUS.md` generated from tests

## M1: Foundations

| Feature | Exit criteria (tests) |
|---|---|
| Physical frame allocator built from the Limine map, with explicit reservations (kernel image, boot stack, page tables, framebuffer, ACPI, kernel ELF file) | `ktest:lumen_kernel::mm::frame::never_returns_reserved` (allocate every frame, check none overlaps a reserved range; independently, nothing behind a live mapping is allocatable), `ktest:lumen_kernel::mm::frame::live_memory_oracle_detects_a_leak`, `ktest:lumen_kernel::mm::frame::alloc_free_reuse`, `cargo:lumen-mm` (bitmap logic on the host), `scenario:test_mm::test_frame_allocator_summary_logged` |
| Paging: our own page tables, higher-half kernel with W^X per section, guard pages | `ktest:mm::paging::kernel_text_not_writable` (a write faults with a #PF report), `ktest:mm::paging::map_unmap_roundtrip`, `scenario:test_mm::test_write_to_text_crashes_cleanly` |
| Kernel heap with working `free` (`GlobalAlloc`) | `ktest:mm::heap::reuse_after_free`, `ktest:mm::heap::fragmentation_stress` (random alloc/free, then a large allocation succeeds), `cargo:lumen-heap` |
| Kernel stacks from the allocator with guard pages; replace the static boot and IST stacks | `scenario:test_mm::test_stack_overflow_hits_guard` (recursion produces a crash report naming the guard page, not a silent corruption) |
| Timer: LAPIC timer calibrated against HPET or the PIT; TSC frequency; interrupts on; IRQ-safe `SpinLock` | `ktest:time::tsc_calibrated_within_tolerance`, `ktest:time::sleep_10ms_elapses`, `scenario:test_time::test_timer_interrupts_counted` |
| Kernel trace ring buffer (per-CPU, lock-free) plus log timestamps | `cargo:lumen-trace` (ring wraparound, concurrent writers model), `ktest:trace::events_recorded_in_order` |
| Boot timeline: phases traced and printed | `scenario:test_boot::test_boot_timeline_printed` (every phase present, monotonic timestamps) |
| (Optional) SMP bring-up via Limine MP | `scenario:test_smp::test_all_cpus_online` (with `-smp 4`) |

## M2: Processes

| Feature | Exit criteria |
|---|---|
| Scheduler (preemptive round robin, then priorities) | `ktest:sched::two_threads_interleave`, `ktest:sched::sleeping_thread_wakes` |
| User mode (ring 3), `syscall`/`sysret`, per-process address spaces | `scenario:test_user::test_hello_user` (a user program prints through `debug_log`), `scenario:test_user::test_user_fault_kills_process_not_kernel` |
| Capability tables and handles; syscall ABI from ARCHITECTURE section 8 | `ktest:cap::derive_only_narrows`, `scenario:test_cap::test_open_without_dir_cap_fails`, `cargo:lumen-abi` (ABI structs, error codes) |
| ELF loader for user programs | `cargo:lumen-elf` (program headers), `scenario:test_user::test_bad_elf_rejected` |
| IPC channels with handle passing | `scenario:test_ipc::test_ping_pong`, `scenario:test_ipc::test_handle_transfer_moves` |
| User runtime (`lumen-rt`) | `scenario:test_user::test_rt_alloc_and_print` |
| Shell as a user program (replaces `kmon`); scenario tests move to the shell | `scenario:test_shell::test_echo`, `scenario:test_shell::test_pipe_text` (`echo hi \| cat`), `scenario:test_shell::test_unknown_command_single_error` |

## M3: Storage

| Feature | Exit criteria |
|---|---|
| virtio-blk driver behind `BlockDevice` | `scenario:test_blk::test_read_write_sectors` |
| Block cache | `cargo:lumen-bcache` (eviction, write-back ordering) |
| VFS with `Dir` capabilities and RAMFS | `scenario:test_fs::test_ramfs_ops`, `scenario:test_fs::test_mv_across_directories` (v1 regression) |
| LumenFS: copy-on-write, journaled, snapshots | `cargo:lumen-fs` (crash consistency: inject a crash at every write, then remount and check invariants), `scenario:test_fs::test_snapshot_restore_exact` |
| Persistence across reboot | `scenario:test_fs::test_persists_across_reboot` (write, sync, kill QEMU, boot again on the same image, verify) |

## M4: Input and console

| Feature | Exit criteria |
|---|---|
| virtio-input and PS/2 keyboard, keymaps (US, plus one more) | `scenario:test_input::test_keys_via_qmp_send_key` |
| Structured shell: typed records, `where`, `sort`, `first`; text fallback | `cargo:lumen-shell` (parser and pipeline engine), `scenario:test_shell::test_ls_where_sort_first` |

## M5: Graphics

| Feature | Exit criteria |
|---|---|
| virtio-gpu / GOP display driver, compositor | `scenario:test_gui::test_two_windows_golden` |
| Font rendering and widget toolkit **with the automation tree** | `cargo:lumen-ui` (layout, tree), `scenario:test_gui::test_automation_tree_lists_buttons`, `scenario:test_gui::test_invoke_press_changes_state` |
| Window manager, terminal, file manager (snapshot browser), editor | one golden plus one automation-tree scenario per app |
| X-ray mode | `scenario:test_xray::test_run_queue_matches_trace` |

## M6: Networking

| Feature | Exit criteria |
|---|---|
| virtio-net, Ethernet/ARP/IPv4/UDP/TCP/DHCP/DNS | `cargo:lumen-net` (packet parsing and TCP state machine), `scenario:test_net::test_dhcp_lease` (QEMU user net), `scenario:test_net::test_tcp_echo` (to a host echo server) |
| TLS 1.3 (decision: port rustls or write our own; ADR in this doc before starting) | `scenario:test_net::test_tls_handshake_local` (to a local test server with a test CA) |
| HTTP client | `scenario:test_net::test_http_get_local` |

## M7: Claude

Load the `claude-api` skill before writing the client, and use current model IDs.

| Feature | Exit criteria |
|---|---|
| Keychain service | `scenario:test_keychain::test_key_never_logged` (grep every log for the test key) |
| API client (streaming) | `scenario:test_claude::test_against_mock_server` (local mock of the Messages API) |
| Ask bar, agent panel with lanes, pause, per-step undo | `scenario:test_agent::test_undo_all_restores_snapshot` (mock model) |
| Capability prompts and audit log | `cargo:lumen-audit` (hash chain verification, tamper detection), `scenario:test_agent::test_denied_capability_blocks_action` |

## M8: Product

Login screen and user accounts, installer (writes the system to a disk),
settings, themes and dark mode. Exit: `scenario:test_install::test_install_then_boot_from_disk`,
`scenario:test_login::test_wrong_password_rejected`, golden screens per theme.

## M9: Real hardware

UEFI on a real x86-64 laptop, ACPI, NVMe/AHCI, USB HID (xHCI), a common NIC,
and a USB boot image. Exit: the same scenario suite on QEMU with the
real-hardware drivers (`-device nvme`, `qemu-xhci`, `e1000e`), plus a manual
hardware checklist recorded in the PR (the only manual step allowed).

## M10: ARM64 port

QEMU `virt` (and UTM), built on the HAL. Exit: the entire scenario suite runs
on both architectures in CI.
