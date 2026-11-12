//! Typed addresses: [`PhysAddr`], [`PhysFrame`] and [`VirtAddr`].
//!
//! Physical and virtual addresses are both `u64`s, and mixing them up is a
//! classic kernel bug (passing a physical address where the HHDM pointer was
//! meant reads someone else's memory). These types make the compiler catch
//! it, and each one checks its invariant when it is built:
//!
//! - [`PhysAddr`]: below 2^52, the architectural limit on x86-64.
//! - [`PhysFrame`]: a [`PhysAddr`] that is 4 KiB aligned (the start of a frame).
//! - [`VirtAddr`]: canonical for 4-level paging (bits 63..47 all equal), the
//!   only addresses the CPU accepts.

use core::fmt;

use crate::FRAME_SIZE;

/// Physical addresses are at most 52 bits wide on x86-64.
pub const PHYS_ADDR_BITS: u32 = 52;
const PHYS_LIMIT: u64 = 1 << PHYS_ADDR_BITS;
/// Virtual addresses are 48 bits wide (sign-extended) with 4-level paging.
pub const VIRT_ADDR_BITS: u32 = 48;

/// A physical address (below 2^52).
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct PhysAddr(u64);

impl PhysAddr {
    /// `addr` as a physical address, or `None` if it is 2^52 or above.
    pub const fn new(addr: u64) -> Option<Self> {
        if addr < PHYS_LIMIT { Some(Self(addr)) } else { None }
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// `self + bytes`, or `None` if that leaves the physical address space.
    pub const fn checked_add(self, bytes: u64) -> Option<Self> {
        match self.0.checked_add(bytes) {
            Some(a) => Self::new(a),
            None => None,
        }
    }

    /// Is the address a multiple of `align`? Panics unless `align` is a
    /// power of two.
    pub const fn is_aligned(self, align: u64) -> bool {
        assert!(align.is_power_of_two(), "alignment must be a power of two");
        self.0 & (align - 1) == 0
    }
}

/// The start of a 4 KiB physical frame.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct PhysFrame(PhysAddr);

impl PhysFrame {
    /// The frame starting at `addr`, or `None` if `addr` is not 4 KiB aligned.
    pub const fn from_start(addr: PhysAddr) -> Option<Self> {
        if addr.is_aligned(FRAME_SIZE) { Some(Self(addr)) } else { None }
    }

    /// The frame starting at the raw address `addr`, or `None` if it is not a
    /// 4 KiB aligned physical address.
    pub const fn from_u64(addr: u64) -> Option<Self> {
        match PhysAddr::new(addr) {
            Some(a) => Self::from_start(a),
            None => None,
        }
    }

    /// The frame that contains `addr`.
    pub const fn containing(addr: PhysAddr) -> Self {
        Self(PhysAddr(addr.0 & !(FRAME_SIZE - 1)))
    }

    pub const fn start(self) -> PhysAddr {
        self.0
    }

    /// The start address as a raw `u64`.
    pub const fn as_u64(self) -> u64 {
        self.0.0
    }

    /// The frame `frames` frames after this one, or `None` past the end of
    /// the physical address space.
    pub const fn checked_add(self, frames: u64) -> Option<Self> {
        match frames.checked_mul(FRAME_SIZE) {
            Some(bytes) => match self.0.checked_add(bytes) {
                Some(a) => Some(Self(a)),
                None => None,
            },
            None => None,
        }
    }
}

/// A canonical virtual address (4-level paging).
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct VirtAddr(u64);

impl VirtAddr {
    /// `addr` as a virtual address, or `None` if it is not canonical.
    pub const fn new(addr: u64) -> Option<Self> {
        // Canonical: bits 63..47 are all copies of bit 47.
        let top = addr >> (VIRT_ADDR_BITS - 1);
        if top == 0 || top == (1 << (64 - VIRT_ADDR_BITS + 1)) - 1 { Some(Self(addr)) } else { None }
    }

    /// The address of `ptr`. Every pointer the CPU accepted is canonical.
    pub fn from_ptr<T: ?Sized>(ptr: *const T) -> Self {
        Self::new(ptr.cast::<u8>() as u64).expect("a pointer the CPU used is canonical")
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    pub const fn as_ptr<T>(self) -> *const T {
        self.0 as *const T
    }

    pub const fn as_mut_ptr<T>(self) -> *mut T {
        self.0 as *mut T
    }

    /// `self + bytes`, or `None` if that overflows or is not canonical.
    pub const fn checked_add(self, bytes: u64) -> Option<Self> {
        match self.0.checked_add(bytes) {
            Some(a) => Self::new(a),
            None => None,
        }
    }

    /// Round down to a multiple of `align`. Panics unless `align` is a power
    /// of two no larger than 2^47: clearing only bits below 47 keeps a
    /// canonical address canonical (clearing bit 47 of an upper-half address
    /// would not).
    pub const fn align_down(self, align: u64) -> Self {
        assert!(align.is_power_of_two() && align <= 1 << (VIRT_ADDR_BITS - 1), "bad alignment");
        Self(self.0 & !(align - 1))
    }

    /// Is the address a multiple of `align`? Panics unless `align` is a
    /// power of two.
    pub const fn is_aligned(self, align: u64) -> bool {
        assert!(align.is_power_of_two(), "alignment must be a power of two");
        self.0 & (align - 1) == 0
    }
}

macro_rules! hex_formatting {
    ($($t:ty),*) => {$(
        impl fmt::Debug for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($t), "({:#x})"), self.as_u64())
            }
        }
        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{:#x}", self.as_u64())
            }
        }
        impl fmt::LowerHex for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::LowerHex::fmt(&self.as_u64(), f)
            }
        }
    )*};
}
hex_formatting!(PhysAddr, PhysFrame, VirtAddr);

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::format;

    #[test]
    fn phys_addr_stops_at_52_bits() {
        assert_eq!(PhysAddr::new(PHYS_LIMIT - 1).map(PhysAddr::as_u64), Some(PHYS_LIMIT - 1));
        assert_eq!(PhysAddr::new(PHYS_LIMIT), None);
        assert_eq!(PhysAddr::new(u64::MAX), None);
        let top = PhysAddr::new(PHYS_LIMIT - 8).unwrap();
        assert_eq!(top.checked_add(7).map(PhysAddr::as_u64), Some(PHYS_LIMIT - 1));
        assert_eq!(top.checked_add(8), None);
        assert_eq!(top.checked_add(u64::MAX), None);
    }

    #[test]
    fn frames_are_aligned() {
        assert_eq!(PhysFrame::from_u64(0x5000).map(PhysFrame::as_u64), Some(0x5000));
        assert_eq!(PhysFrame::from_u64(0x5008), None);
        assert_eq!(PhysFrame::from_u64(PHYS_LIMIT), None);
        let a = PhysAddr::new(0x5fff).unwrap();
        assert_eq!(PhysFrame::containing(a).as_u64(), 0x5000);
        assert_eq!(PhysFrame::from_start(a), None);
        assert_eq!(PhysFrame::from_start(PhysAddr::new(0).unwrap()).map(PhysFrame::as_u64), Some(0));
    }

    #[test]
    fn virt_addr_must_be_canonical() {
        for ok in [0, 0x0000_7fff_ffff_ffff, 0xffff_8000_0000_0000, 0xffff_ffff_8000_0000, u64::MAX] {
            assert_eq!(VirtAddr::new(ok).map(VirtAddr::as_u64), Some(ok), "{ok:#x}");
        }
        for bad in [0x0000_8000_0000_0000, 0x8000_0000_0000_0000, 0xfff7_ffff_ffff_ffff, 0x7fff_8000_0000_0000] {
            assert_eq!(VirtAddr::new(bad), None, "{bad:#x}");
        }
        let low_top = VirtAddr::new(0x0000_7fff_ffff_f000).unwrap();
        assert_eq!(low_top.checked_add(0xfff).map(VirtAddr::as_u64), Some(0x0000_7fff_ffff_ffff));
        assert_eq!(low_top.checked_add(0x1000), None, "crossing into the non-canonical hole");
        assert_eq!(VirtAddr::new(u64::MAX).unwrap().checked_add(1), None);
    }

    #[test]
    fn virt_alignment() {
        let v = VirtAddr::new(0xffff_8000_0012_3456).unwrap();
        assert_eq!(v.align_down(FRAME_SIZE).as_u64(), 0xffff_8000_0012_3000);
        assert_eq!(v.align_down(1 << 21).as_u64(), 0xffff_8000_0000_0000);
        assert!(v.is_aligned(2) && !v.is_aligned(4));
        assert!(v.align_down(FRAME_SIZE).is_aligned(FRAME_SIZE));
    }

    #[test]
    fn frames_step_by_whole_frames() {
        let f = PhysFrame::from_u64(0x5000).unwrap();
        assert_eq!(f.checked_add(0), Some(f));
        assert_eq!(f.checked_add(3).map(PhysFrame::as_u64), Some(0x8000));
        let last = PhysFrame::from_u64(PHYS_LIMIT - FRAME_SIZE).unwrap();
        assert_eq!(last.checked_add(1), None);
        assert_eq!(f.checked_add(u64::MAX), None);
    }

    #[test]
    #[should_panic(expected = "bad alignment")]
    fn align_down_refuses_to_break_canonical_form() {
        // Clearing bit 47 of an upper-half address would make it non-canonical.
        let _ = VirtAddr::new(0xffff_8000_0000_0000).unwrap().align_down(1 << 48);
    }

    #[test]
    #[should_panic(expected = "power of two")]
    fn zero_alignment_is_a_bug() {
        let _ = PhysAddr::new(0).unwrap().is_aligned(0);
    }

    #[test]
    fn formatting_is_hex() {
        let v = VirtAddr::new(0xffff_8000_0000_1000).unwrap();
        assert_eq!(format!("{v}"), "0xffff800000001000");
        assert_eq!(format!("{v:?}"), "VirtAddr(0xffff800000001000)");
        assert_eq!(format!("{:?}", PhysFrame::from_u64(0x2000).unwrap()), "PhysFrame(0x2000)");
        assert_eq!(format!("{:#018x}", PhysAddr::new(0x10).unwrap()), "0x0000000000000010");
    }
}
