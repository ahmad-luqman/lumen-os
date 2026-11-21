//! `/proc/<pid>/maps` lines for an address space's VMAs.
//!
//! The kernel's `AddressSpace::format_maps` takes its lock, reads the `brk`
//! cursor and hands the VMAs here. Each line has Linux's fields:
//! `start-end perms offset 00:00 0` with `start`, `end` and `offset` at least
//! 8 hex digits, and `perms` `rwx` then `p` or `s`. A file-backed mapping is
//! `s` when shared and `p` when private; an anonymous one is always `p`, even
//! `MAP_SHARED | MAP_ANONYMOUS` (Linux prints `s`). A label (`[heap]` or
//! `[stack]`) follows two spaces, where Linux pads the name to a fixed
//! column. No device, inode or path is reported for a file-backed mapping
//! (`00:00 0`, where Linux prints the file's device, inode and path).
//!
//! An anonymous VMA overlapping the heap (`[brk initial, brk mapped end)`) is
//! printed split at the heap's edges, so a `.bss` coalesced with the first
//! `brk` pages still shows `[heap]` from the initial break. An anonymous VMA
//! inside the stack range is `[stack]`, and never split.

use core::fmt;

use crate::{Backing, Prot, Vma};

/// What a maps listing needs besides the VMAs: the heap and the stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MapsLayout {
    /// The initial program break (`Brk::initial`): where `[heap]` starts.
    pub brk_initial: u64,
    /// The end of the pages `brk` mapped (`Brk::mapped_end`): where `[heap]`
    /// ends. No heap is shown unless this is above `brk_initial`.
    pub brk_mapped_end: u64,
    /// Inclusive bottom of the initial user stack VMA.
    pub stack_bottom: u64,
    /// Exclusive top of the initial user stack VMA.
    pub stack_top: u64,
}

const HEAP: &str = "[heap]";
const STACK: &str = "[stack]";

/// Write one maps line (with its `\n`).
fn line<W: fmt::Write>(
    out: &mut W,
    start: u64,
    end: u64,
    prot: Prot,
    shared: bool,
    offset: u64,
    label: Option<&str>,
) -> fmt::Result {
    let r = if prot.read() { 'r' } else { '-' };
    let w = if prot.write() { 'w' } else { '-' };
    let x = if prot.exec() { 'x' } else { '-' };
    let sp = if shared { 's' } else { 'p' };
    write!(out, "{start:08x}-{end:08x} {r}{w}{x}{sp} {offset:08x} {:02x}:{:02x} {}", 0u32, 0u32, 0u64)?;
    if let Some(label) = label {
        write!(out, "  {label}")?;
    }
    out.write_char('\n')
}

/// Write the maps lines of `vmas` (in address order) to `out`.
pub fn write_maps<'a, W: fmt::Write>(
    out: &mut W,
    vmas: impl IntoIterator<Item = &'a Vma>,
    layout: &MapsLayout,
) -> fmt::Result {
    debug_assert!(layout.stack_bottom <= layout.stack_top, "stack range {layout:x?}");
    let brk_init = layout.brk_initial;
    let brk_mapped = layout.brk_mapped_end;
    let has_heap = brk_mapped > brk_init;
    for vma in vmas {
        if let Backing::File(fm) = vma.backing {
            line(out, vma.start, vma.end, vma.prot, fm.shared, fm.offset, None)?;
            continue;
        }
        let is_stack = vma.start >= layout.stack_bottom && vma.end <= layout.stack_top;
        if !is_stack && has_heap && vma.start < brk_init && vma.end > brk_init {
            // Straddles the initial break: below it, the heap, and any rest.
            line(out, vma.start, brk_init, vma.prot, false, 0, None)?;
            let heap_end = vma.end.min(brk_mapped);
            line(out, brk_init, heap_end, vma.prot, false, 0, Some(HEAP))?;
            if vma.end > heap_end {
                line(out, heap_end, vma.end, vma.prot, false, 0, None)?;
            }
        } else if !is_stack && has_heap && vma.start >= brk_init && vma.start < brk_mapped && vma.end > brk_mapped {
            // Starts in the heap and runs past its end.
            line(out, vma.start, brk_mapped, vma.prot, false, 0, Some(HEAP))?;
            line(out, brk_mapped, vma.end, vma.prot, false, 0, None)?;
        } else {
            let label = if is_stack {
                Some(STACK)
            } else if has_heap && vma.start >= brk_init && vma.end <= brk_mapped {
                Some(HEAP)
            } else {
                None
            };
            line(out, vma.start, vma.end, vma.prot, false, 0, label)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::string::String;
    use std::vec;
    use std::vec::Vec;

    use super::*;
    use crate::{Brk, FileMapping, MapPlace, PAGE_SIZE, USER_MAX, USER_MIN, USER_STACK_BOTTOM, USER_STACK_TOP, VmaMap};

    const P: u64 = PAGE_SIZE;
    const RW: Prot = Prot::READ.union(Prot::WRITE);
    const RX: Prot = Prot::READ.union(Prot::EXEC);

    /// The kernel ktest's break: the initial break at 0x40_4000, one page mapped.
    const LAYOUT: MapsLayout = layout(0x40_4000, 0x40_5000);

    const fn layout(brk_initial: u64, brk_mapped_end: u64) -> MapsLayout {
        MapsLayout { brk_initial, brk_mapped_end, stack_bottom: USER_STACK_BOTTOM, stack_top: USER_STACK_TOP }
    }

    fn anon(start: u64, end: u64, prot: Prot) -> Vma {
        Vma { start, end, prot, backing: Backing::Anonymous }
    }

    fn file(start: u64, end: u64, prot: Prot, offset: u64, shared: bool) -> Vma {
        let fm = FileMapping { file_id: 7, offset, shared, may_write: !shared };
        Vma { start, end, prot, backing: Backing::File(fm) }
    }

    fn maps<'a>(vmas: impl IntoIterator<Item = &'a Vma>, layout: &MapsLayout) -> String {
        let mut out = String::new();
        write_maps(&mut out, vmas, layout).unwrap();
        out
    }

    #[test]
    fn no_vmas_no_lines() {
        assert_eq!(maps(&[], &LAYOUT), "");
    }

    #[test]
    fn bss_coalesced_with_brk_splits_at_the_initial_break() {
        // The kernel ktest's case: .bss 0x40_2000..0x40_4000 merged with one brk page.
        assert_eq!(
            maps(&[anon(0x40_2000, 0x40_5000, RW)], &LAYOUT),
            "00402000-00404000 rw-p 00000000 00:00 0\n\
             00404000-00405000 rw-p 00000000 00:00 0  [heap]\n"
        );
    }

    #[test]
    fn a_vma_straddling_the_whole_heap_splits_in_three() {
        assert_eq!(
            maps(&[anon(0x40_2000, 0x40_7000, RW)], &LAYOUT),
            "00402000-00404000 rw-p 00000000 00:00 0\n\
             00404000-00405000 rw-p 00000000 00:00 0  [heap]\n\
             00405000-00407000 rw-p 00000000 00:00 0\n"
        );
    }

    #[test]
    fn a_vma_crossing_the_initial_break_and_ending_inside_the_heap_splits_in_two() {
        // What a partial `mprotect` or `munmap` of the heap leaves: the
        // `[heap]` part ends at the VMA's end, not at the heap's.
        assert_eq!(
            maps(&[anon(0x40_2000, 0x40_5000, RW)], &layout(0x40_4000, 0x40_6000)),
            "00402000-00404000 rw-p 00000000 00:00 0\n\
             00404000-00405000 rw-p 00000000 00:00 0  [heap]\n"
        );
    }

    #[test]
    fn a_vma_starting_in_the_heap_and_running_past_it_splits_in_two() {
        // Starting at the initial break.
        assert_eq!(
            maps(&[anon(0x40_4000, 0x40_6000, RW)], &LAYOUT),
            "00404000-00405000 rw-p 00000000 00:00 0  [heap]\n\
             00405000-00406000 rw-p 00000000 00:00 0\n"
        );
        // Starting above it, one page before the heap's end.
        assert_eq!(
            maps(&[anon(0x40_5000, 0x40_7000, RW)], &layout(0x40_4000, 0x40_6000)),
            "00405000-00406000 rw-p 00000000 00:00 0  [heap]\n\
             00406000-00407000 rw-p 00000000 00:00 0\n"
        );
    }

    #[test]
    fn a_vma_inside_the_heap_is_labelled_whole() {
        // Exactly the heap.
        assert_eq!(
            maps(&[anon(0x40_4000, 0x40_5000, RW)], &LAYOUT),
            "00404000-00405000 rw-p 00000000 00:00 0  [heap]\n"
        );
        // Ending at the heap's end, starting above the initial break.
        assert_eq!(
            maps(&[anon(0x40_5000, 0x40_6000, RW)], &layout(0x40_4000, 0x40_6000)),
            "00405000-00406000 rw-p 00000000 00:00 0  [heap]\n"
        );
    }

    #[test]
    fn vmas_beside_the_heap_are_not_labelled() {
        // Ending at the initial break, and starting at the heap's end.
        assert_eq!(
            maps(&[anon(0x40_2000, 0x40_4000, RW), anon(0x40_5000, 0x40_6000, RW)], &LAYOUT),
            "00402000-00404000 rw-p 00000000 00:00 0\n\
             00405000-00406000 rw-p 00000000 00:00 0\n"
        );
    }

    #[test]
    fn an_unmoved_break_shows_no_heap() {
        let none = layout(0x40_4000, 0x40_4000);
        assert_eq!(maps(&[anon(0x40_2000, 0x40_5000, RW)], &none), "00402000-00405000 rw-p 00000000 00:00 0\n");
        assert_eq!(maps(&[anon(0x40_4000, 0x40_5000, RW)], &none), "00404000-00405000 rw-p 00000000 00:00 0\n");
    }

    #[test]
    fn the_stack_is_labelled_only_inside_its_range() {
        assert_eq!(
            maps(&[anon(USER_STACK_BOTTOM, USER_STACK_TOP, RW)], &LAYOUT),
            "7fffef800000-7ffff0000000 rw-p 00000000 00:00 0  [stack]\n"
        );
        // A page in from either end is still the stack.
        assert_eq!(
            maps(&[anon(USER_STACK_BOTTOM + P, USER_STACK_TOP - P, RW)], &LAYOUT),
            "7fffef801000-7fffeffff000 rw-p 00000000 00:00 0  [stack]\n"
        );
        // A VMA reaching a page past either end is not.
        assert_eq!(
            maps(&[anon(USER_STACK_BOTTOM - P, USER_STACK_TOP, RW)], &LAYOUT),
            "7fffef7ff000-7ffff0000000 rw-p 00000000 00:00 0\n"
        );
        assert_eq!(
            maps(&[anon(USER_STACK_BOTTOM, USER_STACK_TOP + P, RW)], &LAYOUT),
            "7fffef800000-7ffff0001000 rw-p 00000000 00:00 0\n"
        );
    }

    #[test]
    fn the_stack_is_never_split_by_the_heap() {
        // A (made-up) break inside the stack range: the stack VMA keeps its label, whole.
        let in_stack = layout(USER_STACK_BOTTOM + P, USER_STACK_BOTTOM + 2 * P);
        // Straddling the initial break.
        assert_eq!(
            maps(&[anon(USER_STACK_BOTTOM, USER_STACK_TOP, RW)], &in_stack),
            "7fffef800000-7ffff0000000 rw-p 00000000 00:00 0  [stack]\n"
        );
        // Starting at it and running past the heap's end.
        assert_eq!(
            maps(&[anon(USER_STACK_BOTTOM + P, USER_STACK_TOP, RW)], &in_stack),
            "7fffef801000-7ffff0000000 rw-p 00000000 00:00 0  [stack]\n"
        );
        // Exactly the heap.
        assert_eq!(
            maps(&[anon(USER_STACK_BOTTOM + P, USER_STACK_BOTTOM + 2 * P, RW)], &in_stack),
            "7fffef801000-7fffef802000 rw-p 00000000 00:00 0  [stack]\n"
        );
    }

    #[test]
    fn file_backed_lines_show_their_offset_sharing_and_no_device() {
        assert_eq!(
            maps(
                &[
                    file(0x7f00_0000_0000, 0x7f00_0000_2000, RX, 0x3000, false),
                    file(0x7f00_0000_2000, 0x7f00_0000_3000, RW, 0, true),
                ],
                &LAYOUT
            ),
            "7f0000000000-7f0000002000 r-xp 00003000 00:00 0\n\
             7f0000002000-7f0000003000 rw-s 00000000 00:00 0\n"
        );
    }

    #[test]
    fn file_backed_lines_are_never_labelled_or_split() {
        // Straddling the heap, inside it, and the whole stack range.
        assert_eq!(
            maps(
                &[
                    file(0x40_2000, 0x40_7000, RW, 0x1000, false),
                    file(0x40_4000, 0x40_5000, RW, 0, true),
                    file(USER_STACK_BOTTOM, USER_STACK_TOP, RW, 0, false),
                ],
                &LAYOUT
            ),
            "00402000-00407000 rw-p 00001000 00:00 0\n\
             00404000-00405000 rw-s 00000000 00:00 0\n\
             7fffef800000-7ffff0000000 rw-p 00000000 00:00 0\n"
        );
    }

    #[test]
    fn each_permission_letter_comes_from_its_own_bit() {
        let at = 0x1000_0000;
        let lines: Vec<String> = [Prot::NONE, Prot::READ, Prot::WRITE, Prot::EXEC, Prot::ALL]
            .into_iter()
            .map(|prot| maps(&[anon(at, at + P, prot)], &LAYOUT))
            .collect();
        assert_eq!(
            lines,
            vec![
                "10000000-10001000 ---p 00000000 00:00 0\n",
                "10000000-10001000 r--p 00000000 00:00 0\n",
                "10000000-10001000 -w-p 00000000 00:00 0\n",
                "10000000-10001000 --xp 00000000 00:00 0\n",
                "10000000-10001000 rwxp 00000000 00:00 0\n",
            ]
        );
        assert_eq!(
            maps(&[file(at, at + P, Prot::NONE, 0, true)], &LAYOUT),
            "10000000-10001000 ---s 00000000 00:00 0\n"
        );
    }

    #[test]
    fn addresses_and_offsets_are_padded_to_eight_digits_and_grow_past_them() {
        assert_eq!(
            maps(
                &[
                    anon(USER_MIN, USER_MIN + P, RW),
                    file(0x1_0000_0000, 0x1_0000_1000, Prot::READ, 0x1_2345_6000, false),
                    anon(USER_MAX - P, USER_MAX, RW),
                ],
                &LAYOUT
            ),
            "00010000-00011000 rw-p 00000000 00:00 0\n\
             100000000-100001000 r--p 123456000 00:00 0\n\
             7fffffffe000-7ffffffff000 rw-p 00000000 00:00 0\n"
        );
    }

    #[test]
    fn a_built_address_space_lists_text_data_heap_mmap_and_stack() {
        // What the loader and `brk` leave: text, .data + .bss, a break grown
        // by a page and a half, a file mapping and the stack.
        let mut vmas = VmaMap::new();
        vmas.map(MapPlace::Fixed(0x40_0000), 2 * P, RX, Backing::Anonymous).unwrap();
        vmas.map(MapPlace::Fixed(0x40_2000), 2 * P, RW, Backing::Anonymous).unwrap();
        let mut brk = Brk::new(0x40_4000).unwrap();
        assert!(brk.set(&mut vmas, 0x40_5800).succeeded);
        let fm = FileMapping { file_id: 1, offset: 0x2000, shared: false, may_write: true };
        vmas.map_file(MapPlace::Fixed(0x7f00_0000_0000), P, Prot::READ, fm).unwrap();
        vmas.map(MapPlace::Fixed(USER_STACK_BOTTOM), USER_STACK_TOP - USER_STACK_BOTTOM, RW, Backing::Anonymous)
            .unwrap();
        assert_eq!(
            maps(vmas.iter(), &layout(brk.initial(), brk.mapped_end())),
            "00400000-00402000 r-xp 00000000 00:00 0\n\
             00402000-00404000 rw-p 00000000 00:00 0\n\
             00404000-00406000 rw-p 00000000 00:00 0  [heap]\n\
             7f0000000000-7f0000001000 r--p 00002000 00:00 0\n\
             7fffef800000-7ffff0000000 rw-p 00000000 00:00 0  [stack]\n"
        );
    }
}
