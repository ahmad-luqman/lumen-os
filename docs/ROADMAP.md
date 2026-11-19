# Lumen roadmap

Each milestone ends when its **exit criteria pass in CI**. Every criterion
names the automated test that proves it: `ktest:` is an in-kernel test,
`scenario:` a QEMU test in `tests/scenarios/`, and `cargo:` a host unit test.
Test names below are the intended names; the PR that implements a feature adds
the test and an entry in `tests/features/<area>.toml`. Current state:
[STATUS.md](STATUS.md).

Work is split into GitHub issues labelled with the milestone (`m1`, `m2`, ...)
and a component (`area:mm`, `area:arch`, ...). One issue is one branch is one PR.

## How this roadmap is ordered

From M2 on, the goal is a system you can **play with** as early as
possible, built quickly. That changes three things compared with the
original plan:

- **Crates before hand-written code.** If a maintained `no_std` crate does
  the job, use it: drivers (`virtio-drivers`), networking (`smoltcp`), TLS
  (`rustls`), WASM (`wasmi`), fonts, images, terminal parsing. We still
  write what is Lumen's own (the kernel core, capabilities, snapshots, the
  automation tree) and the glue. The M1 code stays; nothing is rewritten
  just to swap in a crate.
- **Playable features run in the kernel first.** The shell, the desktop,
  WASM apps and networking run in ring 0 on kernel threads. WASM gives apps
  their sandbox (an app can only call what the kernel hands it), so ring 3,
  syscalls and ELF programs move to M9. LumenFS moves to M10, after a
  RAM-backed filesystem with snapshots and persistence (M6). Principles 2
  and 3 in [ARCHITECTURE.md](ARCHITECTURE.md) still hold. Capabilities are
  enforced at the WASM host interface until M9. Undo comes from RAMFS
  snapshots until M10.
- **The rules don't change.** Stable Rust only. Every dependency is pinned
  with `=`, and each new one goes in its own PR. `make audit` must still
  pass with the crate linked in. A feature counts as done only when its
  test passes in CI.

### Candidate crates

Each of these compiles as a dependency for `x86_64-unknown-none` on the
pinned toolchain (checked 2026-11-13). None is linked into the kernel or
tested in Lumen yet. The PR that adopts one proves it.

| Area | Crates | Notes |
|---|---|---|
| Input, terminal, shell | `pc-keyboard`, `vte`, `noline`, `shlex`, `unicode-width`, `rhai` | `vt100` needs `std` |
| Devices | `virtio-drivers` (blk, net, gpu, input, console, rng, sound, 9p transport over PCI) | `pci_types` is only needed for non-virtio devices (M12) |
| Graphics | `tiny-skia` (`no-std-float`), `embedded-graphics`, `fontdue`, `zune-png` | Floating-point math runs as soft-float in the kernel: cache glyphs |
| Apps | `wasmi` | `wasmtime` is too big |
| Data | `serde`, `serde_json`, `postcard`, `hashbrown` | |
| Files | `ext4-view` (read-only) | `fatfs` 0.3 needs nightly; no writable FS crate builds on stable |
| Network | `smoltcp`, `httparse`, `rustls` + `rustls-rustcrypto` (pre-release) or `embedded-tls` | Crypto needs software backends (below) |
| Crypto | `sha2`, `chacha20poly1305`, `argon2` | |

The kernel has no SSE, and the RustCrypto SIMD backends make LLVM crash
on this target. They build with software backends:
`--cfg poly1305_backend="soft" --cfg chacha20_backend="soft"
--cfg sha2_backend="soft"`, plus `--cfg curve25519_dalek_backend="serial"`
and `--cfg getrandom_backend="custom"` for `rustls-rustcrypto`. Older
crates such as `aes` and `polyval` use `--cfg aes_force_soft` /
`polyval_force_soft` instead. Unusable on stable: `acpi` 5/6 and `aml`
(no AML interpreter, see M12), `ps2-mouse`, `fatfs` 0.3.

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
| Physical frame allocator built from the Limine map, with explicit reservations (kernel image, boot stack, page tables, framebuffer, ACPI, kernel ELF file) | `ktest:lumen_kernel::mm::frame::never_returns_reserved` (allocate every frame, check none overlaps a reserved range; independently, no frame behind the kernel image, boot stack, kernel file or framebuffer and no page table is allocatable), the other `ktest:lumen_kernel::mm::frame::*` tests (negative controls, reservation list, page-walk cross-check, reuse; see `tests/features/mm.toml`), `cargo:lumen-mm` (bitmap logic on the host), `scenario:test_mm::test_frame_allocator_summary_logged` |
| Paging: our own page tables, higher-half kernel with W^X per section, HHDM, map/unmap, uncached MMIO window; #PF reports show the page walk (guard pages come with the stacks, below) | `ktest:lumen_kernel::mm::paging::kernel_text_not_writable` (PTE flags of each segment), `ktest:lumen_kernel::mm::paging::map_unmap_roundtrip` (incl. TLB invalidation), the other `ktest:lumen_kernel::mm::paging::*` tests (W^X per page and per frame, the HHDM maps exactly its entry types, the active tables are ours, map/unmap refuse what they don't own, framebuffer WC, MMIO reads the RSDP, the pre-switch check can fail, the crash walk stops at unreadable tables; see `tests/features/mm.toml`), `cargo:lumen-mm` (mapper vs a model on the host), `scenario:test_mm::test_write_to_text_crashes_cleanly`, `scenario:test_mm::test_nx_data_crashes_cleanly` |
| Kernel heap with working `free` (`GlobalAlloc`) | `ktest:lumen_kernel::mm::heap::reuse_after_free`, `ktest:lumen_kernel::mm::heap::fragmentation_stress` (random alloc/free, then a large allocation succeeds without the heap growing), the other `ktest:lumen_kernel::mm::heap::*` tests (`Vec`/`Box`/`BTreeMap`, growth failure rolls back; see `tests/features/mm.toml`), `cargo:lumen-heap` (reuse, coalescing, alignment, refused frees, randomized stress against a shadow map), `scenario:test_mm::test_out_of_memory_report_shows_the_heap`, `scenario:test_mm::test_double_free_is_refused` |
| Kernel stacks from the allocator with guard pages; move off the boot stack after `mm::init`, replace the static IST stacks | `scenario:test_mm::test_stack_overflow_hits_guard` (recursion produces a crash report naming the guard page, not a silent corruption), `ktest:lumen_kernel::mm::stack::guard_page_unmapped`, `ktest:lumen_kernel::mm::stack::ist_stacks_are_guarded`, `ktest:lumen_kernel::mm::stack::kernel_runs_on_a_guarded_main_stack`, `cargo:lumen-mm` (window layout, registry incl. racing readers) |
| Timer: LAPIC timer calibrated against HPET or the PIT; TSC frequency; interrupts on; IRQ-safe `SpinLock` | `ktest:lumen_kernel::time::tsc_calibrated_within_tolerance`, `ktest:lumen_kernel::time::sleep_10ms_elapses`, `ktest:lumen_kernel::time::the_timer_is_programmed_for_1_khz`, `ktest:lumen_kernel::sync::*` (interrupts off while a lock is held, nested), `scenario:test_time::test_timer_interrupts_counted`, `scenario:test_time::test_boot_calibration_agrees_with_both_clocks`; see `tests/features/time.toml` |
| Kernel trace ring buffer (lock-free; one ring per CPU) | `cargo:lumen-trace` (wraparound, ordering, readers never see torn events even with concurrent writers), `ktest:lumen_kernel::trace::*` (in order, boot phases, exceptions and logs, overhead), `scenario:test_trace::*`, `scenario:test_crash::test_report_shows_the_trace_events_leading_up_to_it`, `scenario:test_crash::test_panic_report_trace_ends_with_the_panic` |
| Boot timeline (phases traced and printed) and log timestamps | `scenario:test_boot::test_boot_timeline_printed` (every phase present, in order, non-decreasing; the `before kmain` line at INFO, not `unknown`, Limine started before its handoff, the origin after the handoff and at least 5 ms after it (a floor, not a time bound); the time phase at least 60 ms (the timer's TSC-timed calibration and the 10 ms first-tick sleep), an incomplete timeline failing at once, the total between the `boot complete` and timeline lines' stamps (one origin and unit; order, not a time bound)), `ktest:lumen_kernel::trace::boot_phases_recorded_in_order` (every phase once, in order; the origin is `boot_tsc()`; Limine's frequency is its raw response; `BeforeKmain::check` passes on the real boot: the origin at least Limine's 10 ms post-handoff wait after its handoff tick, compared in TSC ticks, the handoff's units consistent, Limine's frequency at least half of ours; none a time bound), `ktest:lumen_kernel::trace::log_stamps_count_from_the_timelines_origin` (`time::since_boot_ns` between two TSC reads converted as the timeline converts them: one origin and unit, order only; that klog's printed stamps agree with the timeline only the scenario checks), `ktest:lumen_kernel::trace::the_timeline_says_what_it_lacks`, `scenario:test_boot::test_log_lines_are_structured` (timestamps unknown until calibration, then non-decreasing), `cargo:lumen-time` (every `check` branch and the line's text); see `tests/features/boot.toml` |
| (Optional) SMP bring-up via Limine MP | `scenario:test_smp::test_all_cpus_online` (with `-smp 4`), `scenario:test_smp::test_crash_on_ap_reports_cpu` |

## M2: Terminal and shell

You type into a real terminal, on screen and over serial: a shell with
history, tab completion, pipes, live system views and a scripting language.

| Feature | Exit criteria (tests) |
|---|---|
| First crate linked into the kernel (`pc-keyboard`), dependency policy written into ARCHITECTURE section 2 | `audit:flags` passes with the crate linked; `make test` green |
| Kernel threads: preemptive round robin on the LAPIC timer, sleep and wake, on every CPU (the shell, terminals and `top` run as threads) | `ktest:lumen_kernel::sched::two_threads_interleave`, `ktest:lumen_kernel::sched::sleeping_thread_wakes`, `ktest:lumen_kernel::sched::threads_run_on_every_cpu` |
| Keyboard: PS/2 through the I/O APIC (typed redirection entries with read-modify-write), `pc-keyboard` decoding, US layout plus one more | `scenario:test_input::test_keys_via_qmp_send_key`, `scenario:test_input::test_second_layout`, `ktest:lumen_kernel::interrupts::redirection_entry_roundtrip` |
| Terminal emulator on the framebuffer: ANSI/VT100 escapes (`vte`), 16 and 256 colours, cursor, scrollback, clear | `cargo:lumen-term` (escape sequences → cell grid, scrolling, wide characters), `scenario:test_term::test_ansi_colours_golden`, `scenario:test_term::test_scrollback` |
| Virtual terminals (Alt+F1 to Alt+F4), each with its own shell | `scenario:test_term::test_virtual_terminals_are_independent` |
| The shell replaces `kmon`: line editing, history, tab completion of commands and arguments, quoting (checked against `shlex`). One runs on the screen and one on serial. Serial output stays plain lines unless the boot command line asks for ANSI, so the harness can keep matching lines | `cargo:lumen-shell` (parser, quoting, completion), `scenario:test_shell::test_echo`, `scenario:test_shell::test_history_and_line_editing`, `scenario:test_shell::test_tab_completion`, `scenario:test_shell::test_unknown_command_single_error`; every scenario that drove `kmon` passes against the shell |
| System commands: `help`, `cpus`, `mem`, `irqs` (decoded I/O APIC entries and counts), `dmesg` (log ring), `trace`, `uptime`, `date` (CMOS RTC), `clear`, `history` | `scenario:test_shell::test_system_commands` (each prints its header and at least one row), `scenario:test_shell::test_date_matches_host` (within 2 s) |
| `top`: a live, refreshing view of CPUs, threads, IRQ rates and the heap; `q` quits | `scenario:test_shell::test_top_refreshes_and_quits` |
| Power: `reboot` (reset through port 0xCF9), `shutdown` (ACPI S5 through the FADT's PM1a control block; `SLP_TYP` comes from byte-searching the DSDT for `\_S5`, with no AML interpreter) | `scenario:test_shell::test_reboot_boots_again`, `scenario:test_shell::test_shutdown_powers_off` (QEMU exits by power-off, not by crash) |
| Pipes: text (`dmesg \| grep time`) and structured records from system tables (`irqs \| where count > 0 \| sort count \| first 3`) | `cargo:lumen-shell` (pipeline engine), `scenario:test_shell::test_pipe_text`, `scenario:test_shell::test_where_sort_first` |
| Scripting: a `rhai` REPL and `.rhai` scripts, with OS functions exposed (`cpus()`, `mem()`, `trace()`, `draw_*`) | `scenario:test_script::test_rhai_eval` (`rhai 'cpus().len()'` prints 4 under `-smp 4`), `scenario:test_script::test_script_draws_golden` |
| Demos: a Mandelbrot set in fixed-point math, rendered by every CPU in parallel; Conway's Game of Life | `scenario:test_demo::test_mandelbrot_golden`, `ktest:lumen_kernel::demo::every_cpu_rendered_rows`, `scenario:test_demo::test_life_glider_golden` |
| Debug aids: a CPU that re-locks a `SpinLock` it already holds gets a crash report instead of hanging; serial sends FIFO bursts (15 bytes, one slot short of the FIFO) and checks that the UART is present | `scenario:test_crash::test_spinlock_relock_is_reported`, `ktest:lumen_kernel::arch::x86_64::serial::fifo_burst_round_trip`, `ktest:lumen_kernel::arch::x86_64::serial::presence_detected` |

## M3: Devices and sound

| Feature | Exit criteria (tests) |
|---|---|
| PCI enumeration, the virtio adapter for `virtio-drivers` (claiming a device, DMA frames from the frame allocator, bounce buffers for shared buffers), `lspci` with a driver column | `scenario:test_pci::test_lspci_lists_virtio_devices`, `scenario:test_pci::test_lspci_driver_column`, `ktest:lumen_kernel::virtio::dma_frames_are_owned_and_freed`, `ktest:lumen_kernel::virtio::share_bounces_a_buffer_across_pages`, `ktest:lumen_kernel::virtio::transport_brings_up_a_virtio_device_through_the_hal`, `ktest:lumen_kernel::virtio::a_claimed_device_has_memory_space_and_bus_master_on_and_io_off`, `ktest:lumen_kernel::drivers::probing_records_each_outcome` |
| Harness: `qemu.py` options for virtio tablet, keyboard, rng, sound (wav capture), blk, net (user net with port forwarding) and 9p (host folder) | `tests/harness` (each option's command line), `scenario:test_pci::test_lspci_lists_virtio_devices` with all of them attached |
| Randomness: RDRAND plus virtio-rng; `random` | `scenario:test_dev::test_random_differs_between_boots`, `ktest:lumen_kernel::rng::rdrand_or_virtio_available` |
| Mouse: virtio-tablet (absolute pointer), drawn cursor | `scenario:test_input::test_mouse_moves_pointer` (QMP `input-send-event`, golden) |
| Sound: virtio-sound, `beep`, `play` (tones and simple melodies from a small synth) | `scenario:test_sound::test_tone_is_captured` (QEMU's wav audiodev; the test checks the frequency) |
| Interrupts for devices: MSI-X for virtio (no ACPI `_PRT` needed) | `ktest:lumen_kernel::interrupts::msix_vector_delivered` |

## M4: Desktop

Opt-in: the text console stays the default boot; `desktop` (or the
`lumen.desktop` boot flag, `make run DESKTOP=1`) starts the desktop, and
Alt+F1 to F4 leave it. The design is ARCHITECTURE section 11 (and section
7's `InputDevice`); test names follow issues #147 to #157 and may still
change in the PR that adds each test.

| Feature | Exit criteria (tests) |
|---|---|
| Harness: pointer and key steps that confirm each effect (`drag`, `pointer_down`/`pointer_up` confirmed through `mouse`'s frame count, keys through `kbd`'s `presses`) | `harness:test_gui::*` (the steps' commands and their failure messages), `cargo:lumen-input` (`Presses`, the scancode count), `scenario:test_input::test_drag_steps_are_confirmed` |
| Input events for the desktop: `InputDevice` rings, every pointer frame with its position (no lost click), key events with releases and modifiers; `mouse events`, `kbd events` | `cargo:lumen-input` (the ring and `Reader`: order, `push` returns the `Seq`, a reader that falls behind counts the gap, `skip_to`, `after_overrun` on the frames of an overrun batch after its first and on the next batch's first, `Mods` from `pc_keyboard::Modifiers`, Pause as one press, `RAlt2` not pushed), `cargo:lumen-shell` (`Key::from_press`), `scenario:test_input::test_click_position_is_kept`, `scenario:test_input::test_key_events_have_releases_and_modifiers` |
| Compositor: double-buffered framebuffer, windows, stacking, focus, drag with the mouse; opt-in desktop mode, left with Alt+F1 to F4 and resumed with `desktop` | `cargo:lumen-gfx` (rectangles, damage that never loses coverage, clipped blits, blend rounding), `cargo:lumen-wm` (hit testing, focus, the drag state machine, press-pause-resume-release and the same with a ring gap and with `after_overrun`, damage and focus against a full repaint, `EventQueue`: rationing, move merging, sticky flags, releases only after a delivered press), `scenario:test_gui::test_two_windows_golden`, `scenario:test_gui::test_drag_moves_window`, `scenario:test_gui::test_click_focuses_and_raises` (both title bars probed), `scenario:test_gui::test_leaving_and_resuming_the_desktop` (two CPUs; keys typed into a window never reach terminal 1, by `term text` and `vt`'s counts; keys typed in a terminal never reach a window; the F1 press and release reach neither, the window gets the Alt press (its `FocusLost`: `cargo:lumen-wm`); the router's side: `ktest:lumen_kernel::vt::keys_while_the_desktop_is_shown_are_its_own_until_alt_f_leaves_it`), `scenario:test_gui::test_keys_just_before_alt_f_reach_the_window`, `scenario:test_gui::test_a_check_queued_as_the_desktop_pauses_is_answered_paused`, `scenario:test_gui::test_a_button_held_at_the_start_is_ignored`, `scenario:test_gui::test_presents_match_a_full_repaint` (`desktop check` reports 0 and 0 after a drag, a raise, a focus change, a close, and a theme switch to dark and back: the back buffer equals a full repaint, and the screen equals the back buffer outside the pointer's box), `scenario:test_gui::test_a_window_behind_drops_and_says_busy` (`desktop hold`: `windows` counts `lost`, one WARN, a request cancelled at its deadline and skipped, `ui invoke` says busy), `scenario:test_gui::test_pointer_moves_over_the_desktop` (drawn and taken off), `scenario:test_gui::test_desktop_and_canvas_exclude_each_other`, `scenario:test_gui::test_boot_flag_starts_the_desktop`, `scenario:test_gui::test_boot_flag_without_a_screen_warns`, `ktest:lumen_kernel::desktop::opening_past_the_window_limit_fails` |
| The crash screen over the desktop: a crash while the desktop runs still shows the whole report | `scenario:test_crash::test_crash_report_over_desktop` (`-smp 2`, `desktop spin` committing: a full symbolized report, exit code 37, and the crash screen covering the desktop, none of the spinner's green left), `ktest:lumen_kernel::desktop::a_present_stops_at_the_next_row_once_the_canvas_dies` (the real span path, the epoch changed after `k` rows by a test hook, later rows unchanged) |
| Fonts and images: TrueType text with anti-aliasing (`fontdue`, glyph cache), PNG (`zune-png`), wallpaper, light and dark themes | `cargo:lumen-font` (metrics, cache hits and byte bound, `measure`, a golden coverage mask), `ktest:lumen_kernel::font::renders_and_caches`, `cargo:lumen-wm` (both themes' palettes complete), `cargo:lumen-png` (every colour type decoded, an Adam7 file too; a bad, truncated (at every length) or corrupted (every byte, a bad Adler-32) PNG is an error; one past the limits refused with its size; only the image's own chunks reaching the decoder: compressed-chunk bombs, hidden or many, never inflated), `ktest:lumen_kernel::desktop::a_bad_wallpaper_falls_back_with_one_warning`, `ktest:lumen_kernel::desktop::a_smaller_wallpaper_is_drawn_centred_over_the_flat_colour`, `ktest:lumen_kernel::desktop::a_title_drawn_again_rasterizes_nothing`, `ktest:lumen_kernel::desktop::a_window_s_first_event_is_the_theme_to_draw_with`, `ktest:lumen_kernel::desktop::a_theme_switch_repaints_everything_and_tells_each_window`, `ktest:lumen_kernel::desktop::switches_before_the_owner_reads_end_with_the_current_theme`, `ktest:lumen_kernel::desktop::settled_waits_for_every_owner_to_be_told_of_a_switch`, `ktest:lumen_kernel::desktop::a_wallpaper_is_decoded_and_warned_about_once`, `ktest:lumen_kernel::desktop::a_wallpaper_bigger_than_the_screen_is_cut_centred`, `ktest:lumen_kernel::desktop::an_owner_blocked_for_an_event_wakes_for_a_switch`, `ktest:lumen_kernel::font::text_ends_where_measure_says`, `audit:flags` with `fontdue` and `zune-png` linked, `scenario:test_gui::test_text_rendering_golden`, `scenario:test_gui::test_png_wallpaper_golden` (the screen equals the embedded PNG), `scenario:test_gui::test_dark_theme_golden`, `scenario:test_gui::test_theme_boot_flag_picks_dark` (`lumen.theme=dark`), `scenario:test_gui::test_a_theme_boot_flag_naming_no_theme_warns` |
| Toolkit (`lumen-ui`) **with the automation tree** (ARCHITECTURE section 11): `windows`, `ui tree`, `ui invoke` | `cargo:lumen-ui` (layout, hit testing, dispatch, tree, stable ids, each role's value, actions and flags, invoke errors, invoke and click giving the same state, `Reply`: a request cancelled at its deadline never runs, a dropped one answers itself), `cargo:lumen-wm` (a refused or drained request handed back), `ktest:lumen_kernel::ui::a_request_answers_for_itself_when_dropped`, `ktest:lumen_kernel::desktop::closing_a_window_answers_its_queued_requests`, `cargo:lumen-shell` (records quote a cell with a space), `scenario:test_gui::test_automation_tree_lists_buttons`, `scenario:test_gui::test_invoke_press_changes_state`, `scenario:test_gui::test_invoke_on_a_closed_window_fails`, `scenario:test_gui::test_invoke_quit_closes_its_window` ("ran; window <id> closed") |
| Terminal windows: the M2 terminal runs in a window, several at once | `scenario:test_gui::test_terminal_window_runs_shell` (`term text <window>`; keys to the focused window only; a canvas command refused; the focused cursor blinks), `scenario:test_gui::test_terminal_windows_settle_only_when_idle` (unfocused and idle: settled, the cursor hollow and steady; focused or running `top`: not), `scenario:test_gui::test_closing_a_terminal_window_frees_it` (threads joined, the heap back over four windows, `desktop check` 0 0), `scenario:test_gui::test_closing_a_terminal_window_ends_what_runs_in_it` (`rhai`'s REPL, `top`, a script loop), `scenario:test_gui::test_a_terminal_window_scrolls`, `cargo:lumen-term` (the shared renderer), the `test_term` goldens unchanged |
| `screenshot` (the screen as PNG: to disk after M6, out over serial now) | `cargo:lumen-png` (the PNG writer and its framing), `audit:flags` with `miniz_oxide` linked, `scenario:test_gui::test_screenshot_matches_screendump`, `scenario:test_gui::test_desktop_screenshot_matches_screendump` (the back buffer: with the pointer over the desktop, equal outside the arrow's box and different inside it) |
| Desktop shell: taskbar with a clock, launcher, minimize, the demos (Mandelbrot, Life) in windows | `scenario:test_gui::test_launcher_opens_terminal`, `scenario:test_gui::test_taskbar_focuses_window`, `scenario:test_gui::test_minimize_and_restore`, `scenario:test_gui::test_mandelbrot_window_golden` (with `ktest:lumen_kernel::demo::every_cpu_rendered_rows_in_a_window`), `scenario:test_gui::test_life_window_runs_unfocused_and_stops_when_closed`, `scenario:test_gui::test_taskbar_clock_matches_rtc` (polled until the clock and `date` agree; a minute change damages only the clock's bounds, by `desktop status`), `scenario:test_gui::test_a_window_opened_from_the_shell_gets_a_taskbar_button`, `scenario:test_gui::test_a_launch_while_paused_is_refused`, `scenario:test_gui::test_the_launcher_answers_with_the_new_window_s_button`; desktop goldens mask the clock's bounds (from `ui tree`) |

## M5: Apps

Apps are WASM modules run by `wasmi` on kernel threads, holding only the
capability handles the kernel gives them. The design is ARCHITECTURE
section 8, "Apps: WASM modules in the kernel" (#195); test names follow
issues #196 to #208 and may still change in the PR that adds each test.
No M5 test asserts a time bound (CLAUDE.md, "Tests on a busy host").

| Feature | Exit criteria (tests) |
|---|---|
| The toolchain builds apps: `wasm32-unknown-unknown` in the pinned toolchain, the `apps/` workspace built by `make build` (its `hello` exports `answer`; #199 renames it `lumen_main` and moves it to `apps/test-hello`) | `harness:test_wasm_module::*` (each module valid with the later proposals off, only the default features, `hello`'s exports) |
| `wasmi` runs in the kernel, in fuel slices, with soft-float | `ktest:lumen_kernel::app::wasmi_runs_a_long_loop_in_fuel_slices` (resumed as many times as the loop's fuel per iteration gives, the right value, wasmi's call depth trapping as `StackOverflow`, also after resumes), `ktest:lumen_kernel::app::wasm_floats_match`, `ktest:lumen_kernel::app::a_slice_too_small_for_one_instruction_is_topped_up`, `ktest:lumen_kernel::app::a_function_bigger_than_a_slice_compiles_and_runs`, `ktest:lumen_kernel::app::malformed_modules_are_refused`, `ktest:lumen_kernel::app::a_module_importing_a_function_fails_to_load`, `scenario:test_wasm::test_wasm_command_runs_a_loop_in_fuel_slices`, `audit:flags` with `wasmi` linked into the booted kernel |
| Apps load and run within fixed limits: the manifest, the fixed memory and value stack, each app's worst case checked at compile time, slots, slices; `apps`, `run` | `cargo:lumen-app-api` (`manifest::find`: each rule, each refusal with its error; the handle table's count used up gives `Limit`, with a small cap; error codes and kind numbers pinned; `manifest::find` and every wire decoder never panicking on any truncation or flipped byte, trailing bytes refused), `ktest:lumen_kernel::app::load_refuses_bad_modules` (hand-assembled: an import from another module, an unknown `lumen` name or a wrong signature, an imported memory, table or global, a second memory or table, a start section, a missing `memory` or `lumen_main` or the wrong signature, a memory without a maximum or with one unlike its initial size, past `MEMORY_PAGES` or `TABLE_ELEMENTS`, no manifest or two, past `MODULE_BYTES`, invalid wasm: each `Err` naming why, the heap's `used` unchanged), `ktest:lumen_kernel::app::compiled_code_fits_its_charge`, `ktest:lumen_kernel::app::a_fixed_memory_is_allocated_once`, `scenario:test_apps::test_hello_wasm` (its log line and exit 0, polled until `apps` no longer lists it), `scenario:test_apps::test_busy_app_is_preempted` (while `busy` runs the shell answers and another app finishes, each confirmed; its `slices` grow; `kill` ends it, either kill line, and its thread id leaves `threads`), `scenario:test_apps::test_run_refuses_past_max_apps` (windowless apps; the refusal's text pinned; kill one, then `run` works again and ids aren't reused; arguments past `ARGS_BYTES` refused) |
| Host calls and handles: one argument-checking order, capabilities as handles, the error codes | `cargo:lumen-app-api` (range and canvas checks: overflow, past the memory's end, stride below width, stride × height overflowing, 0 or too many rects, clipping; every wire variant's encoding pinned; the largest `Event`, with the biggest varints, within `EVENT_BYTES`; a request's `set_value` one byte past `SET_VALUE_BYTES` refused; log text cut on a char boundary with control characters replaced; the tree's rules and a `TREE_NODES`-deep chain decoded with bounded stack), `ktest:lumen_kernel::app::trace_read_edges` (`since` past `next` refused and equal to it copying 0; `BufferTooSmall` below one header and one event, exactly that accepted; an unknown stream or CPU; the per-read cap; the header agreeing with the result; on live data `next − since = copied + missed`), `scenario:test_apps::test_app_without_capability_is_not_granted` (`run --without time`: `NotGranted`, and a made-up handle `BadHandle`; the app logs both and exits 0), `scenario:test_apps::test_handles_refuse_wrong_kind_and_closed` (no reuse after a close: a new window gets a new handle, the old one stays `BadHandle`; the same handle from `handle_find` every time; `handle_find(Window)` and an unknown number; a closed grant `NotGranted`; `run --without log` refused), `scenario:test_apps::test_host_calls_refuse_bad_arguments` (the `abuse` app: the check order, a bad handle with a bad pointer giving `BadHandle`; wrapping, out-of-range and unaligned pointers; sentinel-filled outputs unchanged by a refusal; an overlapping call writing what a separate one does; one byte past each cap refused and exactly at it accepted; bad canvases, stride × height overflowing; bad UTF-8; `px` out of range; a log level of 0 or 6; `window_wait`'s argument errors and its 0 at a deadline; the event kept in hand across `BufferTooSmall`, `Theme` first; a second `window_open` giving `Limit`; the deepest allowed tree published, then `ui tree` answering; each code asserted, exit 0, no WARN), `scenario:test_apps::test_app_log_is_cut_and_sanitised` |
| Apps end cleanly: one end reason, a fault ends only the app, kill, the close grace | `cargo:lumen-app-api` (the end-reason cell's first writer wins; the close grace's slice count; the request state: taking a request (started, skipped, unmapped), a refused answer leaving it pending), `ktest:lumen_kernel::app::an_app_ending_answers_owner_ended` (the ending path on a `ReplyCell` reads `OwnerEnded`), `scenario:test_apps::test_app_trap_kills_app_not_kernel` (unreachable, a panic whose message has a newline and a control character (the WARN shows U+FFFD, no forged line), memory past the limit (`greedy` logs its page count and the -1), call depth: each one pinned end-line WARN and no ERROR, the shell keeps working; the heap as `test_gui.py` checks it: history filled first, one warm-up run, within a few KiB after each confirmed join), `scenario:test_apps::test_kill_wakes_a_waiting_app` (`time_sleep` far past the test's timeout and `window_wait(-1, 0)`: `state` `waiting` confirmed, then only `killed` or `still ending (in time_sleep\|window_wait)`, the end line `killed`; `kill` of an id not listed says `no app`, of an app that already ended `had already ended`) |
| App SDK: the guest crate, a template, `make app NAME=...`, windows for apps, the launcher lists bundled apps | `harness:test_make_app::*` (`make app NAME=` in a temporary copy builds), `scenario:test_apps::test_template_app_runs` (the committed template: `run` opens its window; a confirmed key changes what it draws; closing ends it, its thread joined), `scenario:test_apps::test_app_ignoring_close_is_ended` (a busy app given `Close`: ended after the slice count), `scenario:test_gui::test_launcher_lists_desktop_apps_only` (the menu has the bundled desktop apps and no test app; invoking one makes `apps` list it) |
| `lumen-ui` inside apps: their automation trees with global ids, `ui invoke` reaching the app, one request per window | `scenario:test_apps::test_app_ids_are_global` (two copies of one app: their trees never share an id; a closed app's ids name nothing), `scenario:test_apps::test_closed_its_window_answer_comes_after_the_window_closes`, `scenario:test_apps::test_app_that_traps_closes_its_window_and_answers` (a started request answered `OwnerEnded`, failing loudly on "still running"; no ERROR; its PR notes the 2 s precondition), `scenario:test_apps::test_calculator_tree_and_invoke` |
| Calculator | `cargo:lumen-calc`, `scenario:test_apps::test_calculator_tree_and_invoke` (`2 + 3 =` shows `5` in the display's value; a second `ui invoke` while one runs says busy), `scenario:test_apps::test_calculator_golden` |
| Clock | `scenario:test_apps::test_clock_golden` (frozen with `run clock --at 10:09`), `scenario:test_apps::test_clock_tree_matches_rtc` (polled until it agrees with `date`) |
| An original snake-style game | `cargo:lumen-snake` (growth, wall and self collision, food never on the snake, same seed same game), `scenario:test_apps::test_snake_golden` (a seed, then N presses of its Step button), `scenario:test_apps::test_snake_tree_and_steps` |
| Game of Life as an app (replacing the kernel's Life window; the text-mode `life` stays) | `scenario:test_apps::test_life_golden` (a pattern, then N presses of Step), `scenario:test_apps::test_life_runs_unfocused_and_stops_when_closed` (replaces `scenario:test_gui::test_life_window_runs_unfocused_and_stops_when_closed`) |
| Text editor on an in-memory buffer (files in M6) | `cargo:lumen-edit` (every edit undone and redone restores the text; a randomized model), `scenario:test_apps::test_editor_golden` (the cursor's blink off), `scenario:test_apps::test_editor_typing_and_tree` |
| Image viewer for bundled images, PNG decoded inside the app | `scenario:test_apps::test_viewer_golden` (the wallpaper at 1:1 equals the PNG's pixels), `scenario:test_apps::test_viewer_list_and_select`, `scenario:test_apps::test_viewer_bad_image_shows_an_error` (the app shows the error and keeps running) |
| Trace events for X-ray: run queues, interrupts, lock contention, in per-CPU X-ray rings that never touch the main ones; `read_since` | `cargo:lumen-trace` (`read_since`: every sequence number copied or missed once, an old `since`, the cap, a polling reader under a writer; `lumen_trace::kind`'s numbers pinned), `ktest:lumen_kernel::trace::run_queue_changes_are_traced_in_order`, `ktest:lumen_kernel::trace::a_contended_lock_is_traced`, `ktest:lumen_kernel::trace::xray_events_leave_the_main_ring_alone` (after the X-ray rings wrapped twice, the boot timeline and a crash report's events read as before), `scenario:test_xray::test_run_queue_matches_trace`, `scenario:test_xray::test_device_interrupts_are_traced` |
| X-ray: a live view of the trace (run queues, IRQs, locks, switches) as an app holding `Trace` | `scenario:test_xray::test_xray_matches_the_scheduler` (counts: N parked threads, the label equals the scheduler's count), `scenario:test_xray::test_xray_golden` (`run xray --replay`: a frozen view of a recorded trace bundled in the app), `scenario:test_xray::test_xray_without_trace_is_not_granted` (`run --without trace xray`) |
| Sampling profiler: `perf top` samples RIP on each timer tick and symbolizes it | `cargo:lumen-perf` (ranking and grouping by symbol, made-up samples), `ktest:lumen_kernel::perf::samples_land_while_armed_and_stop_when_disarmed`, `ktest:lumen_kernel::perf::a_spinning_thread_ranks_first_by_function_and_by_thread`, `ktest:lumen_kernel::perf::a_burn_being_stopped_keeps_its_slot_until_its_thread_has_ended`, `ktest:lumen_kernel::perf::a_profile_stopped_at_once_says_so_and_disarms`, `ktest:lumen_kernel::perf::the_function_index_finds_what_a_scan_finds`, `scenario:test_perf::test_hot_function_is_on_top` (with `perf burn` on one CPU, the burn function has the most non-idle samples, and the count asked for is the count reported), `scenario:test_perf::test_perf_usage_and_refusals` |

Not yet tested: `run`'s key-stopped "still loading" wait (a load is too quick to catch).

## M6: Files

| Feature | Exit criteria (tests) |
|---|---|
| virtio-blk behind `BlockDevice`, block cache | `scenario:test_blk::test_read_write_sectors`, `cargo:lumen-bcache` (eviction, write-back ordering) |
| VFS with `Dir` capabilities and RAMFS; `ls`, `cat`, `cp`, `mv`, `rm`, `mkdir`, `edit` | `scenario:test_fs::test_ramfs_ops`, `scenario:test_fs::test_mv_across_directories` (v1 regression) |
| Snapshots and undo in RAMFS: `snap`, `snaps`, `undo` | `scenario:test_fs::test_snapshot_restore_exact` |
| Persistence: the RAMFS image (with its snapshots) is saved to virtio-blk (`postcard`) and loaded at boot | `scenario:test_fs::test_persists_across_reboot` (write, sync, kill QEMU, boot again on the same image, verify) |
| Host folder over virtio-9p: edit on the Mac, run in Lumen. QEMU accepts a macOS host folder (checked 2026-11-13); the 9P2000.L client is ours | `scenario:test_9p::test_reads_host_file`, `scenario:test_9p::test_writes_host_file` |
| Read-only ext4 disks (`ext4-view`) for apps, fonts and pictures | `scenario:test_fs::test_ext4_image_readable` |
| Apps and scripts load from disk | `scenario:test_apps::test_run_wasm_from_disk` |

## M7: Networking

| Feature | Exit criteria (tests) |
|---|---|
| virtio-net with `smoltcp`: Ethernet, ARP, IPv4, ICMP, UDP, TCP, DHCP, DNS; `ifconfig`, `ping`, `host` | `scenario:test_net::test_dhcp_lease` (QEMU user net), `scenario:test_net::test_ping_gateway`, `scenario:test_net::test_dns_resolves`, `scenario:test_net::test_tcp_echo` (to a host echo server) |
| HTTP client (`httparse`): `get <url>` | `scenario:test_net::test_http_get_local` |
| A web page served by Lumen: live CPUs, heap, threads and trace, opened from the host's browser (port forwarding) | `scenario:test_net::test_status_page_from_host` |
| Remote shell: `nc localhost 2323` on the host opens a Lumen shell | `scenario:test_net::test_remote_shell` |
| TLS 1.3 with `rustls` + `rustls-rustcrypto` (or `embedded-tls`); decision recorded in ARCHITECTURE before starting | `scenario:test_net::test_tls_handshake_local` (to a local test server with a test CA), `scenario:test_net::test_bad_certificate_rejected` |

## M8: Claude

Load the `claude-api` skill before writing the client, and use current model IDs.

| Feature | Exit criteria (tests) |
|---|---|
| Keychain service | `scenario:test_keychain::test_key_never_logged` (grep every log for the test key) |
| API client (streaming, `serde_json`) | `scenario:test_claude::test_against_mock_server` (local mock of the Messages API) |
| `ask` in the shell, and an Ask bar on the desktop | `scenario:test_claude::test_shell_ask_streams_answer` (mock model) |
| Agent: runs shell commands and app actions through capabilities, with a snapshot per step, per-step undo and undo-all | `scenario:test_agent::test_undo_all_restores_snapshot` (mock model) |
| Capability prompts and audit log | `cargo:lumen-audit` (hash chain verification, tamper detection), `scenario:test_agent::test_denied_capability_blocks_action` |

## M9: User mode and isolation

Native programs in ring 3, for when WASM isn't enough and drivers or
services move out of the kernel.

| Feature | Exit criteria (tests) |
|---|---|
| User mode (ring 3), `syscall`/`sysret`, per-process address spaces | `scenario:test_user::test_hello_user` (a user program prints through `debug_log`), `scenario:test_user::test_user_fault_kills_process_not_kernel` |
| Syscall ABI and capability tables from ARCHITECTURE section 8 | `ktest:cap::derive_only_narrows`, `scenario:test_cap::test_open_without_dir_cap_fails`, `cargo:lumen-abi` (ABI structs, error codes) |
| ELF loader (`goblin` or `xmas-elf`) | `cargo:lumen-elf` (program headers), `scenario:test_user::test_bad_elf_rejected` |
| IPC channels with handle passing | `scenario:test_ipc::test_ping_pong`, `scenario:test_ipc::test_handle_transfer_moves` |
| User runtime (`lumen-rt`), and the shell as a user process | `scenario:test_user::test_rt_alloc_and_print`, every `test_shell` scenario passes with the shell in ring 3 |

## M10: LumenFS

| Feature | Exit criteria |
|---|---|
| LumenFS on disk: copy-on-write, journaled, snapshots (ARCHITECTURE section 9); replaces the RAMFS image as the main disk format | `cargo:lumen-fs` (crash consistency: inject a crash at every write, then remount and check invariants), `scenario:test_fs::test_snapshot_restore_exact` and `scenario:test_fs::test_persists_across_reboot` on LumenFS |

## M11: Product

Login screen and user accounts (`argon2` password hashes), installer (writes
the system to a disk), settings. Exit:
`scenario:test_install::test_install_then_boot_from_disk`,
`scenario:test_login::test_wrong_password_rejected`, golden screens per
theme.

## M12: Real hardware

UEFI on a real x86-64 laptop, NVMe/AHCI, USB HID (xHCI), a common NIC, and
a USB boot image. No AML interpreter builds on stable Rust, so devices use
MSI/MSI-X (no `_PRT`) and shutdown uses the `\_S5` byte search from M2. The
serial driver skips a missing UART. Exit: the same scenario suite on QEMU
with the real-hardware drivers (`-device nvme`, `qemu-xhci`, `e1000e`),
plus a manual hardware checklist recorded in the PR (the only manual step
allowed).

## M13: ARM64 port

QEMU `virt` (and UTM), built on the HAL. `virtio-drivers` also speaks the
MMIO transport that `virt` uses. Exit: the entire scenario suite runs on
both architectures in CI.
