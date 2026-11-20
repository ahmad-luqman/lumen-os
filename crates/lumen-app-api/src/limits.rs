//! Every limit an app runs under (ARCHITECTURE section 8, "Limits"), and
//! each app's worst case, checked against the budget at compile time.
//!
//! The kernel heap has no `realloc` and wasmi grows its buffers by
//! doubling, so an app's allocations are fixed in size or bounded: its
//! linear memory and value stack are allocated once at their full size, its
//! compiled code is at most [`CODE_FACTOR`] times its module, the rest
//! within [`APP_OVERHEAD`], its window's surfaces within [`WINDOW_PIXELS`].
//! [`APP_WORST`] is their sum, and holding one of [`MAX_APPS`] slots is
//! holding that much: there is no runtime counter.

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;

/// What all apps' worst cases together may come to.
pub const APP_BUDGET: u64 = 128 * MIB;
/// Apps running or loading at once.
pub const MAX_APPS: usize = 4;
/// Memory left to the kernel when a smaller machine gets fewer slots. The
/// guests' 256 MiB leave the frame allocator about 167 MiB (the kernel's
/// ELF, the boot structures and the firmware's reservations aside), which
/// holds the kernel's few MiB and four worst cases with this to spare.
pub const KERNEL_RESERVE: u64 = 32 * MIB;
/// The largest module `run` takes.
pub const MODULE_BYTES: usize = MIB as usize;
/// Compiled code's charge: this many times the module's size.
pub const CODE_FACTOR: u64 = 8;
/// A wasm page.
pub const PAGE_BYTES: u64 = 64 * KIB;
/// The largest fixed linear memory, in pages.
pub const MEMORY_PAGES: u64 = 256;
/// wasmi's value stack, in bytes, allocated once.
pub const VALUE_STACK_BYTES: usize = MIB as usize;
/// wasmi's call depth, in frames.
pub const CALL_DEPTH: usize = 1000;
/// Everything else an app holds, at most: the parts below (their sum
/// checked at compile time).
pub const APP_OVERHEAD: u64 = 2 * MIB;
/// The store, the instance, the call stack (`CALL_DEPTH` frames) and the
/// handle table.
pub const STORE_BYTES: u64 = 512 * KIB;
/// Its table, at `TABLE_ELEMENTS` elements of at most 16 bytes.
pub const TABLE_BYTES: u64 = TABLE_ELEMENTS as u64 * 16;
/// Its published tree (#207): what the kernel holds for it (the desktop's
/// rows, the id map) and the peak while a new one replaces it (the old
/// still held, the new decoded and its rows built). Measured on the worst
/// trees by `tests/publish_peak.rs` (929,280 bytes at the peak, each block
/// counted as the kernel's heap sizes it), with room to spare for the
/// heap's fragmentation.
pub const TREE_COPIES_BYTES: u64 = MIB;
/// Its drawing thread's glyph cache (#206; the kernel's `font::Text`).
pub const GLYPH_CACHE_BYTES: u64 = 128 * KIB;

const _: () = assert!(
    STORE_BYTES + TABLE_BYTES + TREE_COPIES_BYTES + GLYPH_CACHE_BYTES <= APP_OVERHEAD,
    "APP_OVERHEAD's parts exceed it"
);

/// The largest window, in pixels (640 × 480).
pub const WINDOW_PIXELS: u64 = 640 * 480;
/// A window's surfaces: the owner's, the committed one, `text_draw`'s
/// scratch.
pub const SURFACES: u64 = 3;
/// A pixel, xRGB8888.
pub const BYTES_PER_PIXEL: u64 = 4;
/// A table's most elements.
pub const TABLE_ELEMENTS: usize = 10_000;
/// Fuel per slice.
pub const SLICE_FUEL: u64 = 1_000_000;
/// Fuel every host call costs.
pub const CALL_FUEL: u64 = 1_000;
/// Fuel a `log` line costs on top.
pub const LOG_FUEL: u64 = 10_000;
/// Fuel per byte of a `log` line, of a published tree and of an answer
/// copied in (#207).
pub const BYTE_FUEL: u64 = 1;
/// Fuel per pixel of a commit or a `text_draw` clip.
pub const PIXEL_FUEL: u64 = 1;
/// Fuel per trace event copied.
pub const EVENT_FUEL: u64 = 16;
/// Fuel per character of each layout pass over a text (`text_measure`
/// makes one, `text_draw` two): about what laying a character out costs
/// next to a wasm instruction (measured in #218's review on TCG: ~170 a
/// character for a measure, ~430 for a draw's two passes).
pub const TEXT_CHAR_FUEL: u64 = 256;
/// Fuel per node (items counted) of a tree `window_publish` decodes, checks
/// and maps to global ids, on top of `BYTE_FUEL` per byte copied in: about
/// what building a node's automation row costs next to a wasm instruction
/// (its strings copied, its id looked up and mapped, its row made).
pub const NODE_FUEL: u64 = 512;
/// The toolkit's text size, in pixels: the kernel's windows' (`font::TEXT_PX`,
/// a `const` assertion ties them), so an app's toolkit looks like a kernel
/// window's.
pub const UI_TEXT_PX: u32 = 16;
/// Windows per app.
pub const WINDOWS: usize = 1;
/// `Rect`s in one `window_commit`.
pub const COMMIT_RECTS: usize = 16;
/// `run`'s arguments, NUL-terminated each.
pub const ARGS_BYTES: usize = 1024;
/// Text in a host call (`log`, `panic`, `text_*`).
pub const TEXT_BYTES: usize = 1024;
/// A window's title.
pub const TITLE_BYTES: usize = 64;
/// A log line's text, after which it is cut.
pub const LOG_BYTES: usize = 256;
/// Text sizes, in pixels.
pub const TEXT_PX_MIN: u32 = 8;
pub const TEXT_PX_MAX: u32 = 96;
/// A request's `set_value` text.
pub const SET_VALUE_BYTES: usize = 512;
/// The largest encoded event.
pub const EVENT_BYTES: usize = 1024;
/// A published tree's nodes, items counted (the most widgets and list or
/// menu items an app's window can expose), and encoded bytes.
pub const TREE_NODES: usize = 1024;
pub const TREE_BYTES: usize = 64 * KIB as usize;
/// An encoded answer.
pub const ANSWER_BYTES: usize = 4 * KIB as usize;
/// Trace events one `trace_read` copies.
pub const TRACE_EVENTS_PER_READ: usize = 256;
/// Slice ends an app given `Close` has before it is ended.
pub const CLOSE_GRACE_SLICES: u32 = 4;
/// How long `kill` waits before saying "still ending", in nanoseconds.
pub const KILL_WAIT_NS: u64 = 1_000_000_000;

/// One app's worst case, in bytes: what holding a slot holds.
pub const APP_WORST: u64 = MEMORY_PAGES * PAGE_BYTES
    + VALUE_STACK_BYTES as u64
    + CODE_FACTOR * MODULE_BYTES as u64
    + APP_OVERHEAD
    + WINDOWS as u64 * SURFACES * WINDOW_PIXELS * BYTES_PER_PIXEL;

const _: () = assert!(MAX_APPS as u64 * APP_WORST <= APP_BUDGET, "the apps' worst cases exceed the budget");

/// How many apps may run at once on a machine whose frame allocator owns
/// `usable` bytes: [`MAX_APPS`], or fewer when the worst cases wouldn't fit
/// beside [`KERNEL_RESERVE`]. The kernel works it out once, at the first
/// `run`.
pub fn slots(usable: u64) -> usize {
    let fit = usable.saturating_sub(KERNEL_RESERVE) / APP_WORST;
    (fit as usize).min(MAX_APPS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_worst_case_fits_the_budget() {
        // 16 MiB of memory, 1 of value stack, 8 of code, 2 of overhead and
        // three 640 × 480 surfaces: about 30.5 MiB.
        assert_eq!(APP_WORST, 27 * MIB + 3 * 640 * 480 * 4);
        // Beside the kernel's reserve on what the guests' 256 MiB leave
        // the frame allocator (166.7 MiB measured).
        assert_eq!(slots(166 * MIB), MAX_APPS);
    }

    #[test]
    fn a_smaller_machine_gets_fewer_slots() {
        assert_eq!(slots(256 * MIB), MAX_APPS);
        assert_eq!(slots(128 * MIB), 3);
        assert_eq!(slots(64 * MIB), 1);
        assert_eq!(slots(32 * MIB), 0);
        assert_eq!(slots(0), 0);
        assert_eq!(slots(u64::MAX), MAX_APPS);
    }
}
