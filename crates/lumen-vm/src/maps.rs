//! `/proc/<pid>/maps` lines for an address space's VMAs.
//!
//! The kernel's `AddressSpace::format_maps` takes its lock, reads the `brk`
//! cursor, resolves the files its mappings name and hands the VMAs here.
//! Each line has Linux's fields: `start-end perms offset major:minor inode`,
//! with `start`, `end` and `offset` at least 8 hex digits, the device's
//! major and minor at least 2 hex digits each and the inode in decimal, and
//! `perms` `rwx` then `p` or `s`. A file-backed mapping is `s` when shared
//! and `p` when private; an anonymous one is always `p`, even
//! `MAP_SHARED | MAP_ANONYMOUS` (Linux prints `s`).
//!
//! **The name:** a file's path, or a label (`[heap]` or `[stack]`), comes
//! after 73 characters, where Linux's `seq_pad` puts it: the fields and a
//! space are padded with spaces to 72 characters ([`NAME_PAD`]), then one
//! more space (more fields than fit push the name right, still two spaces
//! after the inode). A path holding a newline shows it as `\012`, and an
//! unlinked file's path ends ` (deleted)`, as Linux's `seq_path` and
//! `d_path` print them. A line without a name ends at its inode (Linux
//! leaves the padding space there: a deliberate difference, which no reader
//! of the fields sees).
//!
//! **Which file:** a file-backed VMA names the file its
//! [`FileMapping::file_id`](crate::FileMapping) resolves to (device, inode
//! and path; `00:00 0` and no name if it resolves to nothing). An ELF image
//! loaded by copying its bytes into anonymous VMAs names its file over the
//! [`ImageRange`]s the kernel recorded for it ([`ImageRange::from_segment`],
//! cut back by [`ImageRange::trim`] where its memory is unmapped), as
//! Linux's file-backed segments do: an anonymous VMA is split at each
//! range's edges, and the part inside a range shows the file and the offset
//! in it.
//!
//! An anonymous VMA overlapping the heap (`[brk initial, brk mapped end)`) is
//! printed split at the heap's edges, so a `.bss` coalesced with the first
//! `brk` pages still shows `[heap]` from the initial break. An anonymous VMA
//! inside the stack range is `[stack]`, and never split. Where names
//! overlap, `[stack]` wins, then `[heap]`, then an image range (one of the
//! overlapping ones: a trim's split reorders them).

use alloc::vec::Vec;
use core::fmt::{self, Write as _};

use crate::{Backing, PAGE_SIZE, Prot, Vma, align_down_page, align_up_page};

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

/// A file as its maps lines name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileInfo<'a> {
    /// The file's device (`st_dev`), major and minor.
    pub major: u32,
    pub minor: u32,
    /// The file's inode number (`st_ino`).
    pub ino: u64,
    /// The path it was opened by, its bytes (#268).
    pub path: &'a [u8],
    /// Unlinked since: ` (deleted)` follows the path.
    pub deleted: bool,
}

/// Part of an ELF image copied into anonymous memory at load, named as
/// the file `file` (whatever the kernel keeps for it) it came from:
/// `[start, end)` (page-aligned) holds the file's bytes from `offset` on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageRange<F> {
    pub start: u64,
    pub end: u64,
    /// The file offset of `start`.
    pub offset: u64,
    pub file: F,
}

impl<F> ImageRange<F> {
    /// The range naming a `PT_LOAD` segment's file bytes: `filesz` bytes
    /// from `file_offset` loaded at `vaddr` (load bias added). From the
    /// segment's first page to the end of the page holding its last file
    /// byte, at the file offset of that first page (`file_offset` less
    /// `vaddr`'s offset in its page); its `.bss` past that is anonymous, as
    /// on Linux. `None` for a segment with no file bytes, one whose
    /// `file_offset` and `vaddr` lie at different offsets in their pages
    /// (Linux couldn't map it; a loader refusing it never asks), or one
    /// whose end overflows.
    pub fn from_segment(vaddr: u64, filesz: u64, file_offset: u64, file: F) -> Option<Self> {
        if filesz == 0 || file_offset % PAGE_SIZE != vaddr % PAGE_SIZE {
            return None;
        }
        let start = align_down_page(vaddr);
        let end = align_up_page(vaddr.checked_add(filesz)?)?;
        let offset = align_down_page(file_offset);
        // The file bytes must end inside the file's offsets too.
        file_offset.checked_add(filesz)?;
        Some(Self { start, end, offset, file })
    }
}

impl<F: Clone> ImageRange<F> {
    /// `ranges` less `[start, end)`, the memory an unmap or `MAP_FIXED`
    /// took: a range overlapping it is cut back (its head or its tail), or
    /// split in two around it, the tail's offset following its new start;
    /// one inside it goes. In place, the split tails appended.
    pub fn trim(ranges: &mut Vec<Self>, start: u64, end: u64) {
        debug_assert!(start < end, "trimming an empty range {start:#x}..{end:#x}");
        let mut tails = Vec::new();
        ranges.retain_mut(|r| {
            if r.end <= start || r.start >= end {
                return true;
            }
            if r.end > end {
                let offset = r.offset + (end - r.start);
                tails.push(Self { start: end, end: r.end, offset, file: r.file.clone() });
            }
            if r.start < start {
                r.end = start;
                true
            } else {
                false
            }
        });
        ranges.extend(tails);
    }
}

const HEAP: &str = "[heap]";
const STACK: &str = "[stack]";

/// The width Linux pads a line's fields (and the space after them) to
/// before the name: `seq_setwidth(m, 25 + sizeof(void *) * 6 - 1)` on a
/// 64-bit kernel. The name follows one more space, after 73 characters.
pub const NAME_PAD: usize = 25 + 8 * 6 - 1;

/// What follows a line's fields.
#[derive(Clone, Copy, Debug)]
enum Name<'a> {
    None,
    Label(&'static str),
    File(FileInfo<'a>),
}

/// One maps line.
struct Line<'a> {
    start: u64,
    end: u64,
    prot: Prot,
    shared: bool,
    offset: u64,
    name: Name<'a>,
}

/// A line's bytes appended to the listing, counted for the padding: text
/// through [`Counted::text`] (or `fmt::Write`, for the fields), a path's raw
/// bytes through [`Counted::bytes`]. Appending to a `Vec` can't fail.
struct Counted<'w> {
    out: &'w mut Vec<u8>,
    written: usize,
}

impl Counted<'_> {
    fn bytes(&mut self, b: &[u8]) {
        self.written += b.len();
        self.out.extend_from_slice(b);
    }

    fn text(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
}

impl fmt::Write for Counted<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.bytes(s.as_bytes());
        Ok(())
    }
}

impl Line<'_> {
    /// Write the line (with its `\n`).
    fn write(&self, out: &mut Vec<u8>) {
        let (start, end, offset) = (self.start, self.end, self.offset);
        let r = if self.prot.read() { 'r' } else { '-' };
        let w = if self.prot.write() { 'w' } else { '-' };
        let x = if self.prot.exec() { 'x' } else { '-' };
        let sp = if self.shared { 's' } else { 'p' };
        let (major, minor, ino) = match self.name {
            Name::File(f) => (f.major, f.minor, f.ino),
            Name::None | Name::Label(_) => (0, 0, 0),
        };
        let mut out = Counted { out, written: 0 };
        // `Counted`'s `write_str` is always `Ok`.
        let _ = write!(out, "{start:08x}-{end:08x} {r}{w}{x}{sp} {offset:08x} {major:02x}:{minor:02x} {ino}");
        match self.name {
            Name::None => {}
            Name::Label(label) => {
                pad(&mut out);
                out.text(label);
            }
            Name::File(f) => {
                pad(&mut out);
                // `seq_path(.., "\n")`: the path's bytes as they are,
                // UTF-8 or not (#268), but a newline as `\012`.
                for (i, part) in f.path.split(|&b| b == b'\n').enumerate() {
                    if i > 0 {
                        out.text("\\012");
                    }
                    out.bytes(part);
                }
                if f.deleted {
                    out.text(" (deleted)");
                }
            }
        }
        out.text("\n");
    }
}

/// The space after the inode, padding to [`NAME_PAD`], and the space
/// before the name.
fn pad(out: &mut Counted<'_>) {
    out.text(" ");
    while out.written < NAME_PAD {
        out.text(" ");
    }
    out.text(" ");
}

/// Write the maps lines of `vmas` (in address order) to `out`: a
/// file-backed VMA named by `files` (its `file_id` to the file, or `None`),
/// an anonymous one by `layout` (`[heap]`, `[stack]`) or `images` (each
/// range's file as `image_file` shows it). Bytes, not text: a path is
/// written as its bytes, UTF-8 or not.
pub fn write_maps<'a, 'n, F, I, R>(
    out: &mut Vec<u8>,
    vmas: impl IntoIterator<Item = &'a Vma>,
    layout: &MapsLayout,
    images: &'n [ImageRange<F>],
    image_file: I,
    files: R,
) where
    I: Fn(&'n F) -> FileInfo<'n>,
    R: Fn(u64) -> Option<FileInfo<'n>>,
{
    debug_assert!(layout.stack_bottom <= layout.stack_top, "stack range {layout:x?}");
    let heap = (layout.brk_mapped_end > layout.brk_initial).then_some((layout.brk_initial, layout.brk_mapped_end));
    for vma in vmas {
        let (start, end, prot) = (vma.start, vma.end, vma.prot);
        if let Backing::File(fm) = vma.backing {
            let name = files(fm.file_id).map_or(Name::None, Name::File);
            Line { start, end, prot, shared: fm.shared, offset: fm.offset, name }.write(out);
            continue;
        }
        if vma.start >= layout.stack_bottom && vma.end <= layout.stack_top {
            Line { start, end, prot, shared: false, offset: 0, name: Name::Label(STACK) }.write(out);
            continue;
        }
        // Split at the heap's edges and the images' ranges' edges: no edge
        // falls inside a piece, so each lies wholly inside or outside each.
        let mut at = vma.start;
        while at < vma.end {
            let edges = heap.into_iter().flat_map(|(a, b)| [a, b]).chain(images.iter().flat_map(|r| [r.start, r.end]));
            let next = edges.filter(|&e| e > at && e < vma.end).min().unwrap_or(vma.end);
            let (name, offset) = if heap.is_some_and(|(a, b)| at >= a && next <= b) {
                (Name::Label(HEAP), 0)
            } else if let Some(r) = images.iter().find(|r| at >= r.start && next <= r.end) {
                (Name::File(image_file(&r.file)), r.offset + (at - r.start))
            } else {
                (Name::None, 0)
            };
            Line { start: at, end: next, prot, shared: false, offset, name }.write(out);
            at = next;
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::format;
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

    /// File 7: a file on `/tmp` (device 0:2).
    const TMP_FILE: FileInfo<'static> =
        FileInfo { major: 0, minor: 2, ino: 12, path: b"/tmp/data.bin", deleted: false };
    /// File 1: a file on a disk (device 0xfd:0x101, as Linux prints dm-257).
    const DISK_FILE: FileInfo<'static> =
        FileInfo { major: 0xfd, minor: 0x101, ino: 1_572_898, path: b"/usr/lib/libc.so", deleted: false };

    /// No image ranges.
    const NO_IMAGES: &[ImageRange<FileInfo<'static>>] = &[];

    const fn layout(brk_initial: u64, brk_mapped_end: u64) -> MapsLayout {
        MapsLayout { brk_initial, brk_mapped_end, stack_bottom: USER_STACK_BOTTOM, stack_top: USER_STACK_TOP }
    }

    fn anon(start: u64, end: u64, prot: Prot) -> Vma {
        Vma { start, end, prot, backing: Backing::Anonymous }
    }

    fn file(start: u64, end: u64, prot: Prot, offset: u64, shared: bool) -> Vma {
        file_id(7, start, end, prot, offset, shared)
    }

    fn file_id(file_id: u64, start: u64, end: u64, prot: Prot, offset: u64, shared: bool) -> Vma {
        let fm = FileMapping { file_id, offset, shared, may_write: !shared };
        Vma { start, end, prot, backing: Backing::File(fm) }
    }

    /// Files 7 and 1 resolve; any other id to nothing.
    fn resolve<'a>(id: u64) -> Option<FileInfo<'a>> {
        match id {
            7 => Some(TMP_FILE),
            1 => Some(DISK_FILE),
            _ => None,
        }
    }

    fn maps<'a>(vmas: impl IntoIterator<Item = &'a Vma>, layout: &MapsLayout) -> String {
        with_images(vmas, layout, &[])
    }

    fn with_images<'a>(
        vmas: impl IntoIterator<Item = &'a Vma>,
        layout: &MapsLayout,
        images: &[ImageRange<FileInfo<'static>>],
    ) -> String {
        let mut out = Vec::new();
        write_maps(&mut out, vmas, layout, images, |f| *f, resolve);
        String::from_utf8(out).unwrap()
    }

    /// `fields` followed by `name` at Linux's column (the rule restated, for
    /// lines other than the ones pinned literally).
    fn named(fields: &str, name: &str) -> String {
        format!("{:<72} {name}\n", format!("{fields} "))
    }

    fn range(start: u64, end: u64, offset: u64) -> ImageRange<u32> {
        ImageRange { start, end, offset, file: 7 }
    }

    #[test]
    fn a_segment_names_its_file_bytes_pages_at_its_first_pages_offset() {
        // Page-aligned: its pages, its offset.
        assert_eq!(ImageRange::from_segment(0x40_1000, 0x20, 0x1000, 7), Some(range(0x40_1000, 0x40_2000, 0x1000)));
        // Starting mid-page (0x100 in), its last byte on the next page:
        // two pages from the offset of its first page.
        assert_eq!(ImageRange::from_segment(0x40_3100, 0x1000, 0x2100, 7), Some(range(0x40_3000, 0x40_5000, 0x2000)));
        // Ending exactly at a page's end: no page past it.
        assert_eq!(ImageRange::from_segment(0x40_3800, 0x800, 0x800, 7), Some(range(0x40_3000, 0x40_4000, 0)));
        // No file bytes (all .bss), offsets at different places in their
        // pages, an end past 2^64: no range.
        assert_eq!(ImageRange::from_segment(0x40_3000, 0, 0x3000, 7), None);
        assert_eq!(ImageRange::from_segment(0x40_3100, 0x10, 0x2200, 7), None);
        assert_eq!(ImageRange::from_segment(u64::MAX - 0xfff, 0x2000, 0, 7), None);
        assert_eq!(ImageRange::from_segment(0x40_3000, 0x2000, u64::MAX - 0xfff, 7), None);
    }

    #[test]
    fn trim_cuts_a_ranges_head_or_tail_splits_its_middle_and_spares_the_rest() {
        let whole = || vec![range(0x10_0000, 0x10_4000, 0x5000)];
        // Its head unmapped: the tail stays, its offset following.
        let mut r = whole();
        ImageRange::trim(&mut r, 0xf_f000, 0x10_1000);
        assert_eq!(r, vec![range(0x10_1000, 0x10_4000, 0x6000)]);
        // Its tail: the head stays, offset unchanged.
        let mut r = whole();
        ImageRange::trim(&mut r, 0x10_3000, 0x10_5000);
        assert_eq!(r, vec![range(0x10_0000, 0x10_3000, 0x5000)]);
        // Its middle: both remainders, the tail's offset past the hole.
        let mut r = whole();
        ImageRange::trim(&mut r, 0x10_1000, 0x10_2000);
        assert_eq!(r, vec![range(0x10_0000, 0x10_1000, 0x5000), range(0x10_2000, 0x10_4000, 0x7000)]);
        // All of it, or more: gone.
        let mut r = whole();
        ImageRange::trim(&mut r, 0x10_0000, 0x10_4000);
        assert_eq!(r, vec![]);
        // Beside it, either side: untouched.
        let mut r = whole();
        ImageRange::trim(&mut r, 0xf_0000, 0x10_0000);
        ImageRange::trim(&mut r, 0x10_4000, 0x10_8000);
        assert_eq!(r, whole());
        // Two ranges, one cut back, one untouched, one dropped.
        let mut r = vec![range(0x1000, 0x3000, 0), range(0x3000, 0x4000, 0x9000), range(0x8000, 0x9000, 0)];
        ImageRange::trim(&mut r, 0x2000, 0x4000);
        assert_eq!(r, vec![range(0x1000, 0x2000, 0), range(0x8000, 0x9000, 0)]);
    }

    #[test]
    fn no_vmas_no_lines() {
        assert_eq!(maps(&[], &LAYOUT), "");
    }

    #[test]
    fn names_start_at_column_73_as_linux_pads_them() {
        // Pinned literally: 39 characters of fields, then 34 spaces.
        let heap = maps(&[anon(0x40_4000, 0x40_5000, RW)], &LAYOUT);
        assert_eq!(heap, "00404000-00405000 rw-p 00000000 00:00 0                                  [heap]\n");
        assert_eq!(heap.find('['), Some(73));
        // A line from a real Linux `cat /proc/self/maps` (64-bit addresses,
        // a 7-digit inode): 53 characters of fields, then 20 spaces.
        let vma = file_id(1, 0x55d0_c5a3_b000, 0x55d0_c5a3_d000, Prot::READ, 0, false);
        let mut out = Vec::new();
        let cat = FileInfo { major: 8, minor: 2, ino: 1_572_898, path: b"/usr/bin/cat", deleted: false };
        write_maps(&mut out, &[vma], &LAYOUT, NO_IMAGES, |f| *f, |_| Some(cat));
        let out = String::from_utf8(out).unwrap();
        assert_eq!(out, "55d0c5a3b000-55d0c5a3d000 r--p 00000000 08:02 1572898                    /usr/bin/cat\n");
        assert_eq!(out.find('/'), Some(73));
        // The rule restated agrees.
        assert_eq!(heap, named("00404000-00405000 rw-p 00000000 00:00 0", "[heap]"));
    }

    #[test]
    fn fields_wider_than_the_pad_keep_two_spaces_before_the_name() {
        let wide = FileInfo { major: 0xfff, minor: 0xf_ffff, ino: u64::MAX, ..TMP_FILE };
        let vma = file(0x7fff_ffff_e000, 0x7fff_ffff_f000, RW, 0xffff_ffff_ffff_f000, true);
        let mut out = Vec::new();
        write_maps(&mut out, &[vma], &LAYOUT, NO_IMAGES, |f| *f, |_| Some(wide));
        let out = String::from_utf8(out).unwrap();
        assert_eq!(
            out,
            "7fffffffe000-7ffffffff000 rw-s fffffffffffff000 fff:fffff 18446744073709551615  /tmp/data.bin\n"
        );
    }

    #[test]
    fn bss_coalesced_with_brk_splits_at_the_initial_break() {
        // The kernel ktest's case: .bss 0x40_2000..0x40_4000 merged with one brk page.
        assert_eq!(
            maps(&[anon(0x40_2000, 0x40_5000, RW)], &LAYOUT),
            String::from("00402000-00404000 rw-p 00000000 00:00 0\n")
                + &named("00404000-00405000 rw-p 00000000 00:00 0", "[heap]")
        );
    }

    #[test]
    fn a_vma_straddling_the_whole_heap_splits_in_three() {
        assert_eq!(
            maps(&[anon(0x40_2000, 0x40_7000, RW)], &LAYOUT),
            String::from("00402000-00404000 rw-p 00000000 00:00 0\n")
                + &named("00404000-00405000 rw-p 00000000 00:00 0", "[heap]")
                + "00405000-00407000 rw-p 00000000 00:00 0\n"
        );
    }

    #[test]
    fn a_vma_crossing_the_initial_break_and_ending_inside_the_heap_splits_in_two() {
        // What a partial `mprotect` or `munmap` of the heap leaves: the
        // `[heap]` part ends at the VMA's end, not at the heap's.
        assert_eq!(
            maps(&[anon(0x40_2000, 0x40_5000, RW)], &layout(0x40_4000, 0x40_6000)),
            String::from("00402000-00404000 rw-p 00000000 00:00 0\n")
                + &named("00404000-00405000 rw-p 00000000 00:00 0", "[heap]")
        );
    }

    #[test]
    fn a_vma_starting_in_the_heap_and_running_past_it_splits_in_two() {
        // Starting at the initial break.
        assert_eq!(
            maps(&[anon(0x40_4000, 0x40_6000, RW)], &LAYOUT),
            named("00404000-00405000 rw-p 00000000 00:00 0", "[heap]") + "00405000-00406000 rw-p 00000000 00:00 0\n"
        );
        // Starting above it, one page before the heap's end.
        assert_eq!(
            maps(&[anon(0x40_5000, 0x40_7000, RW)], &layout(0x40_4000, 0x40_6000)),
            named("00405000-00406000 rw-p 00000000 00:00 0", "[heap]") + "00406000-00407000 rw-p 00000000 00:00 0\n"
        );
    }

    #[test]
    fn a_vma_inside_the_heap_is_labelled_whole() {
        // Exactly the heap.
        assert_eq!(
            maps(&[anon(0x40_4000, 0x40_5000, RW)], &LAYOUT),
            named("00404000-00405000 rw-p 00000000 00:00 0", "[heap]")
        );
        // Ending at the heap's end, starting above the initial break.
        assert_eq!(
            maps(&[anon(0x40_5000, 0x40_6000, RW)], &layout(0x40_4000, 0x40_6000)),
            named("00405000-00406000 rw-p 00000000 00:00 0", "[heap]")
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
            named("7fffef800000-7ffff0000000 rw-p 00000000 00:00 0", "[stack]")
        );
        // A page in from either end is still the stack.
        assert_eq!(
            maps(&[anon(USER_STACK_BOTTOM + P, USER_STACK_TOP - P, RW)], &LAYOUT),
            named("7fffef801000-7fffeffff000 rw-p 00000000 00:00 0", "[stack]")
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
    fn the_stack_is_never_split_by_the_heap_or_an_image() {
        // A (made-up) break inside the stack range: the stack VMA keeps its label, whole.
        let in_stack = layout(USER_STACK_BOTTOM + P, USER_STACK_BOTTOM + 2 * P);
        let stack = named("7fffef800000-7ffff0000000 rw-p 00000000 00:00 0", "[stack]");
        // Straddling the initial break.
        assert_eq!(maps(&[anon(USER_STACK_BOTTOM, USER_STACK_TOP, RW)], &in_stack), stack);
        // Starting at it and running past the heap's end.
        assert_eq!(
            maps(&[anon(USER_STACK_BOTTOM + P, USER_STACK_TOP, RW)], &in_stack),
            named("7fffef801000-7ffff0000000 rw-p 00000000 00:00 0", "[stack]")
        );
        // Exactly the heap.
        assert_eq!(
            maps(&[anon(USER_STACK_BOTTOM + P, USER_STACK_BOTTOM + 2 * P, RW)], &in_stack),
            named("7fffef801000-7fffef802000 rw-p 00000000 00:00 0", "[stack]")
        );
        // An image range (made up) inside the stack.
        let image =
            ImageRange { start: USER_STACK_BOTTOM + P, end: USER_STACK_BOTTOM + 2 * P, offset: 0, file: TMP_FILE };
        assert_eq!(with_images(&[anon(USER_STACK_BOTTOM, USER_STACK_TOP, RW)], &LAYOUT, &[image]), stack);
    }

    #[test]
    fn file_backed_lines_show_their_offset_sharing_device_inode_and_path() {
        assert_eq!(
            maps(
                &[
                    file_id(1, 0x7f00_0000_0000, 0x7f00_0000_2000, RX, 0x3000, false),
                    file(0x7f00_0000_2000, 0x7f00_0000_3000, RW, 0, true),
                ],
                &LAYOUT
            ),
            "7f0000000000-7f0000002000 r-xp 00003000 fd:101 1572898                   /usr/lib/libc.so\n\
             7f0000002000-7f0000003000 rw-s 00000000 00:02 12                         /tmp/data.bin\n"
        );
    }

    #[test]
    fn an_unlinked_file_is_deleted_and_a_newline_in_its_path_escaped() {
        let gone = FileInfo { path: b"/tmp/a\nb", deleted: true, ..TMP_FILE };
        let mut out = Vec::new();
        write_maps(
            &mut out,
            &[file(0x1000_0000, 0x1000_1000, RW, 0, true)],
            &LAYOUT,
            NO_IMAGES,
            |f| *f,
            |_| Some(gone),
        );
        assert_eq!(out, named("10000000-10001000 rw-s 00000000 00:02 12", "/tmp/a\\012b (deleted)").as_bytes());
    }

    #[test]
    fn a_path_that_is_not_utf8_is_written_as_its_bytes() {
        // #268: the path's bytes as they are (Linux's `seq_path` escapes
        // only the newline), never a lossy U+FFFD.
        // Two newlines: each is escaped, so a name can't forge a line.
        let odd = FileInfo { path: b"/tmp/\xff\n\xfe\nx", ..TMP_FILE };
        let mut out = Vec::new();
        write_maps(&mut out, &[file(0x1000_0000, 0x1000_1000, RW, 0, true)], &LAYOUT, NO_IMAGES, |f| *f, |_| Some(odd));
        let mut want = named("10000000-10001000 rw-s 00000000 00:02 12", "").into_bytes();
        want.pop(); // the newline after the name
        want.extend_from_slice(b"/tmp/\xff\\012\xfe\\012x\n");
        assert_eq!(out, want);
    }

    #[test]
    fn a_file_id_that_resolves_to_nothing_shows_no_file() {
        assert_eq!(
            maps(&[file_id(99, 0x1000_0000, 0x1000_1000, RW, 0x2000, false)], &LAYOUT),
            "10000000-10001000 rw-p 00002000 00:00 0\n"
        );
    }

    #[test]
    fn file_backed_lines_are_never_labelled_or_split() {
        // Straddling the heap, inside it, and the whole stack range, and
        // over an image range.
        let image = ImageRange { start: 0x40_3000, end: 0x40_4000, offset: 0, file: DISK_FILE };
        assert_eq!(
            with_images(
                &[
                    file(0x40_2000, 0x40_7000, RW, 0x1000, false),
                    file(0x40_4000, 0x40_5000, RW, 0, true),
                    file(USER_STACK_BOTTOM, USER_STACK_TOP, RW, 0, false),
                ],
                &LAYOUT,
                &[image]
            ),
            named("00402000-00407000 rw-p 00001000 00:02 12", "/tmp/data.bin")
                + &named("00404000-00405000 rw-s 00000000 00:02 12", "/tmp/data.bin")
                + &named("7fffef800000-7ffff0000000 rw-p 00000000 00:02 12", "/tmp/data.bin")
        );
    }

    #[test]
    fn image_ranges_name_anonymous_vmas_split_at_their_edges() {
        // Text wholly in its range; data's file part in its range and its
        // .bss page past it, coalesced with the heap's page.
        let images = [
            ImageRange { start: 0x40_0000, end: 0x40_2000, offset: 0, file: DISK_FILE },
            ImageRange { start: 0x40_2000, end: 0x40_3000, offset: 0x2000, file: DISK_FILE },
        ];
        assert_eq!(
            with_images(&[anon(0x40_0000, 0x40_2000, RX), anon(0x40_2000, 0x40_5000, RW)], &LAYOUT, &images),
            named("00400000-00402000 r-xp 00000000 fd:101 1572898", "/usr/lib/libc.so")
                + &named("00402000-00403000 rw-p 00002000 fd:101 1572898", "/usr/lib/libc.so")
                + "00403000-00404000 rw-p 00000000 00:00 0\n"
                + &named("00404000-00405000 rw-p 00000000 00:00 0", "[heap]")
        );
    }

    #[test]
    fn a_vma_partly_inside_an_image_range_shows_the_offset_of_its_own_start() {
        // A range of three pages from offset 0x5000, and VMAs covering its
        // middle page and running a page past each end (what `munmap` and
        // `mprotect` leave).
        let images = [ImageRange { start: 0x50_0000, end: 0x50_3000, offset: 0x5000, file: TMP_FILE }];
        assert_eq!(
            with_images(&[anon(0x50_1000, 0x50_2000, Prot::READ)], &LAYOUT, &images),
            named("00501000-00502000 r--p 00006000 00:02 12", "/tmp/data.bin")
        );
        assert_eq!(
            with_images(&[anon(0x4f_f000, 0x50_4000, Prot::READ)], &LAYOUT, &images),
            String::from("004ff000-00500000 r--p 00000000 00:00 0\n")
                + &named("00500000-00503000 r--p 00005000 00:02 12", "/tmp/data.bin")
                + "00503000-00504000 r--p 00000000 00:00 0\n"
        );
    }

    #[test]
    fn the_heap_wins_over_an_image_range_it_overlaps() {
        let images = [ImageRange { start: 0x40_4000, end: 0x40_5000, offset: 0, file: TMP_FILE }];
        assert_eq!(
            with_images(&[anon(0x40_4000, 0x40_5000, RW)], &LAYOUT, &images),
            named("00404000-00405000 rw-p 00000000 00:00 0", "[heap]")
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
            maps(&[file_id(99, at, at + P, Prot::NONE, 0, true)], &LAYOUT),
            "10000000-10001000 ---s 00000000 00:00 0\n"
        );
    }

    #[test]
    fn addresses_and_offsets_are_padded_to_eight_digits_and_grow_past_them() {
        assert_eq!(
            maps(
                &[
                    anon(USER_MIN, USER_MIN + P, RW),
                    file_id(99, 0x1_0000_0000, 0x1_0000_1000, Prot::READ, 0x1_2345_6000, false),
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
        let fm = FileMapping { file_id: 7, offset: 0x2000, shared: false, may_write: true };
        vmas.map_file(MapPlace::Fixed(0x7f00_0000_0000), P, Prot::READ, fm).unwrap();
        vmas.map(MapPlace::Fixed(USER_STACK_BOTTOM), USER_STACK_TOP - USER_STACK_BOTTOM, RW, Backing::Anonymous)
            .unwrap();
        let exe = FileInfo { major: 0, minor: 1, ino: 33, path: b"/bin/prog", deleted: false };
        let images = [
            ImageRange { start: 0x40_0000, end: 0x40_2000, offset: 0, file: exe },
            ImageRange { start: 0x40_2000, end: 0x40_3000, offset: 0x1000, file: exe },
        ];
        assert_eq!(
            with_images(vmas.iter(), &layout(brk.initial(), brk.mapped_end()), &images),
            named("00400000-00402000 r-xp 00000000 00:01 33", "/bin/prog")
                + &named("00402000-00403000 rw-p 00001000 00:01 33", "/bin/prog")
                + "00403000-00404000 rw-p 00000000 00:00 0\n"
                + &named("00404000-00406000 rw-p 00000000 00:00 0", "[heap]")
                + &named("7f0000000000-7f0000001000 r--p 00002000 00:02 12", "/tmp/data.bin")
                + &named("7fffef800000-7ffff0000000 rw-p 00000000 00:00 0", "[stack]")
        );
    }
}
