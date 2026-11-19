//! A guest (pointer, length) range, checked against the app's memory: both
//! taken as unsigned 32-bit numbers (a wasm `i32` is what the app passes),
//! added without overflow, every byte inside the memory's current size, or
//! [`Error::BadPointer`]. Every host call checks its ranges this way before
//! it reads or writes a byte (ARCHITECTURE section 8, "Every call, in this
//! order").

use crate::Error;
use core::ops::Range;

/// The bytes `[ptr, ptr + len)` of a memory `memory` bytes long.
pub fn range(ptr: i32, len: i32, memory: usize) -> Result<Range<usize>, Error> {
    let start = u64::from(ptr as u32);
    let end = start.checked_add(u64::from(len as u32)).ok_or(Error::BadPointer)?;
    if end > memory as u64 {
        return Err(Error::BadPointer);
    }
    Ok(start as usize..end as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_inside_the_memory_are_taken() {
        assert_eq!(range(0, 0, 0), Ok(0..0));
        assert_eq!(range(0, 16, 16), Ok(0..16));
        assert_eq!(range(10, 6, 16), Ok(10..16));
        // Empty at the very end.
        assert_eq!(range(16, 0, 16), Ok(16..16));
    }

    #[test]
    fn past_the_end_wrapping_or_negative_is_a_bad_pointer() {
        assert_eq!(range(10, 7, 16), Err(Error::BadPointer));
        assert_eq!(range(17, 0, 16), Err(Error::BadPointer));
        // Negative numbers are huge unsigned ones.
        assert_eq!(range(-1, 1, 1 << 20), Err(Error::BadPointer));
        assert_eq!(range(0, -1, 1 << 20), Err(Error::BadPointer));
        // u32::MAX + u32::MAX doesn't wrap: it is past any wasm32 memory
        // (4 GiB at most).
        assert_eq!(range(-1, -1, 1 << 32), Err(Error::BadPointer));
        assert_eq!(range(-1, 1, 1 << 32), Ok(u32::MAX as usize..1 << 32));
        // 0xffff_fff0 + 0x20 would wrap to 0x10 in 32 bits.
        assert_eq!(range(0xffff_fff0_u32 as i32, 0x20, 1 << 20), Err(Error::BadPointer));
    }
}
