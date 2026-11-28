//! Linux `eventfd` counter state machine (`fs/eventfd.c`).
//!
//! No operation in this module allocates, and every operation is panic-free.

use core::fmt;

use crate::{IO_SIZE, InvalidFlags, ReadBytesError, WouldBlock};

/// Maximum value an `eventfd` counter can hold (`0xffff_ffff_ffff_fffe`,
/// Linux `fs/eventfd.c` `ULLONG_MAX - 1`).
pub const MAX_COUNTER: u64 = u64::MAX - 1;

/// `EFD_SEMAPHORE` (`include/uapi/linux/eventfd.h`): each read returns 1 and
/// decrements the counter by 1 instead of draining it to zero.
pub const EFD_SEMAPHORE: u32 = 0x1;

/// `EFD_NONBLOCK` (`include/uapi/linux/eventfd.h` -> `O_NONBLOCK`, `0o4000`).
pub const EFD_NONBLOCK: u32 = 0x800;

/// `EFD_CLOEXEC` (`include/uapi/linux/eventfd.h` -> `O_CLOEXEC`, `0o2000000`).
pub const EFD_CLOEXEC: u32 = 0x8_0000;

const EFD_KNOWN_FLAGS: u32 = EFD_SEMAPHORE | EFD_NONBLOCK | EFD_CLOEXEC;

/// Validated `eventfd2(2)` creation flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EventfdFlags {
    /// `EFD_SEMAPHORE`: semaphore read semantics.
    pub semaphore: bool,
    /// `EFD_NONBLOCK`: non-blocking descriptor.
    pub nonblock: bool,
    /// `EFD_CLOEXEC`: close-on-exec descriptor flag.
    pub cloexec: bool,
}

impl EventfdFlags {
    /// Validate and decode a raw Linux `eventfd2(2)` flags bitmask.
    ///
    /// Rejects any bit outside `EFD_SEMAPHORE | EFD_NONBLOCK | EFD_CLOEXEC`
    /// (`fs/eventfd.c` `do_eventfd`: `if (flags & ~EFD_FLAGS_SET) return -EINVAL`).
    pub const fn from_bits(bits: u32) -> Result<Self, InvalidFlags> {
        if bits & !EFD_KNOWN_FLAGS != 0 {
            return Err(InvalidFlags);
        }
        Ok(Self {
            semaphore: bits & EFD_SEMAPHORE != 0,
            nonblock: bits & EFD_NONBLOCK != 0,
            cloexec: bits & EFD_CLOEXEC != 0,
        })
    }

    /// Re-encode these flags as a Linux `eventfd2(2)` bitmask.
    pub const fn bits(self) -> u32 {
        let mut out = 0u32;
        if self.semaphore {
            out |= EFD_SEMAPHORE;
        }
        if self.nonblock {
            out |= EFD_NONBLOCK;
        }
        if self.cloexec {
            out |= EFD_CLOEXEC;
        }
        out
    }
}

/// Error returned by [`EventCounter::write`] and [`EventCounter::write_bytes`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteError {
    /// Adding `add` would exceed [`MAX_COUNTER`] (`-EAGAIN` / block until space
    /// frees up).
    WouldBlock,
    /// Value was `u64::MAX` or the source buffer was shorter than [`IO_SIZE`]
    /// (8 bytes; `-EINVAL`).
    Invalid,
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WouldBlock => f.write_str("eventfd write would overflow counter"),
            Self::Invalid => f.write_str("invalid eventfd write value or buffer length"),
        }
    }
}

impl core::error::Error for WriteError {}

/// Counter state for a Linux `eventfd` object (`fs/eventfd.c`).
///
/// Maintains the invariant `self.count() <= MAX_COUNTER` at all times.
/// Because there is no in-kernel signaller that can drive the counter to
/// `u64::MAX`, Linux `eventfd_poll`'s `EPOLLERR` (`count == ULLONG_MAX`) state
/// is unreachable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventCounter {
    count: u64,
    semaphore: bool,
}

impl EventCounter {
    /// Create an `eventfd` counter with `initval` and mode `semaphore`.
    ///
    /// Matches Linux `eventfd2(unsigned int initval, int flags)`: `initval` is
    /// 32-bit, so it is always strictly below [`MAX_COUNTER`].
    pub const fn new(initval: u32, semaphore: bool) -> Self {
        Self { count: initval as u64, semaphore }
    }

    /// Current counter value (always `<= MAX_COUNTER`).
    pub const fn count(&self) -> u64 {
        self.count
    }

    /// Whether this counter uses `EFD_SEMAPHORE` read semantics.
    pub const fn is_semaphore(&self) -> bool {
        self.semaphore
    }

    /// Whether a `read` would succeed immediately (`POLLIN` in `eventfd_poll`).
    pub const fn readable(&self) -> bool {
        self.count > 0
    }

    /// Whether the counter is strictly below [`MAX_COUNTER`] (`POLLOUT` in
    /// `fs/eventfd.c` `eventfd_poll`: `ULLONG_MAX - 1 > count`).
    ///
    /// When `true`, `write(1)` is guaranteed to succeed without blocking (larger
    /// values may still return `WouldBlock` if they exceed remaining headroom).
    /// When `false` (`count == MAX_COUNTER`), any non-zero write returns
    /// `WouldBlock`, though `write(0)` still succeeds.
    pub const fn writable(&self) -> bool {
        self.count < MAX_COUNTER
    }

    /// Add `add` to the counter (`fs/eventfd.c` `eventfd_write`).
    ///
    /// - `add == u64::MAX` always returns `Err(WriteError::Invalid)` (`-EINVAL`),
    ///   even when the counter is already at [`MAX_COUNTER`].
    /// - If `self.count + add > MAX_COUNTER`, returns `Err(WriteError::WouldBlock)`
    ///   without modifying the counter.
    /// - `add == 0` succeeds even at [`MAX_COUNTER`] and leaves the counter
    ///   unchanged (matching Linux `ULLONG_MAX - ctx->count > 0`).
    pub fn write(&mut self, add: u64) -> Result<(), WriteError> {
        if add == u64::MAX {
            return Err(WriteError::Invalid);
        }
        if add > MAX_COUNTER - self.count {
            return Err(WriteError::WouldBlock);
        }
        self.count += add;
        Ok(())
    }

    /// Consume from the counter (`fs/eventfd.c` `eventfd_ctx_do_read`).
    ///
    /// - When the counter is zero, returns `Err(WouldBlock)`.
    /// - In semaphore mode, decrements the counter by 1 and returns `Ok(1)`.
    /// - Otherwise, zeroes the counter and returns its previous value.
    pub fn read(&mut self) -> Result<u64, WouldBlock> {
        if self.count == 0 {
            return Err(WouldBlock);
        }
        if self.semaphore {
            self.count -= 1;
            Ok(1)
        } else {
            let value = self.count;
            self.count = 0;
            Ok(value)
        }
    }

    /// Decode a native-endian `u64` from the first 8 bytes of `buf` and apply
    /// [`Self::write`].
    ///
    /// Buffers shorter than [`IO_SIZE`] (8 bytes) return `Err(WriteError::Invalid)`
    /// before inspecting the counter; buffers longer than 8 bytes consume the
    /// first 8 bytes and return `Ok(8)`, matching Linux `eventfd_write`.
    pub fn write_bytes(&mut self, buf: &[u8]) -> Result<usize, WriteError> {
        let Some(&bytes) = buf.first_chunk::<IO_SIZE>() else {
            return Err(WriteError::Invalid);
        };
        self.write(u64::from_ne_bytes(bytes))?;
        Ok(IO_SIZE)
    }

    /// Consume from the counter via [`Self::read`] and encode the native-endian
    /// `u64` into the first 8 bytes of `buf`.
    ///
    /// Buffers shorter than [`IO_SIZE`] (8 bytes) return
    /// `Err(ReadBytesError::Invalid)` even when the counter is zero, matching
    /// Linux `eventfd_read`.
    pub fn read_bytes(&mut self, buf: &mut [u8]) -> Result<usize, ReadBytesError> {
        let Some(head) = buf.first_chunk_mut::<IO_SIZE>() else {
            return Err(ReadBytesError::Invalid);
        };
        let value = self.read()?;
        *head = value.to_ne_bytes();
        Ok(IO_SIZE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_eventfd_constants_match_uapi_literals() {
        // include/uapi/linux/eventfd.h & include/uapi/linux/fcntl.h (x86-64)
        assert_eq!(EFD_SEMAPHORE, 0x1);
        assert_eq!(EFD_NONBLOCK, 0x800);
        assert_eq!(EFD_CLOEXEC, 0x8_0000);
        // M9 census observed eventfd2(EFD_CLOEXEC | EFD_NONBLOCK) = 0x80800.
        assert_eq!(EFD_CLOEXEC | EFD_NONBLOCK, 0x80800);
        // fs/eventfd.c: maximum counter is ULLONG_MAX - 1.
        assert_eq!(MAX_COUNTER, 0xffff_ffff_ffff_fffe);
        assert_eq!(IO_SIZE, 8);
    }

    #[test]
    fn flags_validation_accepts_known_and_rejects_unknown_bits() {
        let census = EventfdFlags::from_bits(0x80800).unwrap();
        assert!(!census.semaphore);
        assert!(census.nonblock);
        assert!(census.cloexec);
        assert_eq!(census.bits(), 0x80800);

        let all = EventfdFlags::from_bits(0x80801).unwrap();
        assert!(all.semaphore && all.nonblock && all.cloexec);
        assert_eq!(all.bits(), 0x80801);

        let empty = EventfdFlags::from_bits(0).unwrap();
        assert!(!empty.semaphore && !empty.nonblock && !empty.cloexec);
        assert_eq!(empty.bits(), 0);

        for bit in 0..32u32 {
            let mask = 1u32 << bit;
            if matches!(mask, 0x1 | 0x800 | 0x8_0000) {
                assert!(EventfdFlags::from_bits(mask).is_ok());
            } else {
                assert_eq!(EventfdFlags::from_bits(mask), Err(InvalidFlags));
                assert_eq!(EventfdFlags::from_bits(0x80801 | mask), Err(InvalidFlags));
            }
        }
    }

    #[test]
    fn counter_maximum_and_overflow_refusal() {
        // fs/eventfd.c eventfd_write: ULLONG_MAX - ctx->count > ucnt.
        let mut c = EventCounter::new(0, false);
        assert!(!c.readable());
        assert!(c.writable());

        // Writing MAX_COUNTER from 0 fills the counter to the exact maximum.
        assert_eq!(c.write(0xffff_ffff_ffff_fffe), Ok(()));
        assert_eq!(c.count(), 0xffff_ffff_ffff_fffe);
        assert!(c.readable());
        assert!(!c.writable());

        // Any non-zero write at maximum is refused with WouldBlock, leaving count intact.
        assert_eq!(c.write(1), Err(WriteError::WouldBlock));
        assert_eq!(c.write(0xffff_ffff_ffff_fffe), Err(WriteError::WouldBlock));
        assert_eq!(c.count(), 0xffff_ffff_ffff_fffe);

        // Zero write at maximum still succeeds in Linux (ULLONG_MAX - (ULLONG_MAX - 1) == 1 > 0).
        assert_eq!(c.write(0), Ok(()));
        assert_eq!(c.count(), 0xffff_ffff_ffff_fffe);

        // Partial fill + exact fit vs off-by-one overflow.
        let mut c2 = EventCounter::new(10, false);
        assert_eq!(c2.write(0xffff_ffff_ffff_fffe - 9), Err(WriteError::WouldBlock));
        assert_eq!(c2.count(), 10);
        assert!(c2.writable());
        assert_eq!(c2.write(0xffff_ffff_ffff_fffe - 10), Ok(()));
        assert_eq!(c2.count(), 0xffff_ffff_ffff_fffe);
        assert!(!c2.writable());

        // One more than remaining capacity fails, even when add is large.
        let mut c3 = EventCounter::new(1, false);
        assert_eq!(c3.write(0xffff_ffff_ffff_fffe), Err(WriteError::WouldBlock));
        assert_eq!(c3.count(), 1);
    }

    #[test]
    fn u64_max_is_always_invalid() {
        // fs/eventfd.c eventfd_write: `if (ucnt == ULLONG_MAX) return -EINVAL;`
        // checked before capacity/blocking.
        let mut empty = EventCounter::new(0, false);
        assert_eq!(empty.write(u64::MAX), Err(WriteError::Invalid));
        assert_eq!(empty.count(), 0);
        assert!(!empty.readable());

        let mut full = EventCounter::new(0, false);
        assert_eq!(full.write(0xffff_ffff_ffff_fffe), Ok(()));
        assert_eq!(full.write(u64::MAX), Err(WriteError::Invalid));
        assert_eq!(full.count(), 0xffff_ffff_ffff_fffe);
    }

    #[test]
    fn read_zeroes_counter_in_normal_mode() {
        // fs/eventfd.c eventfd_ctx_do_read: non-semaphore drains entire count to 0.
        let mut c = EventCounter::new(5, false);
        assert!(c.readable());
        assert!(c.writable());
        assert_eq!(c.write(7), Ok(()));
        assert_eq!(c.read(), Ok(12));
        assert_eq!(c.count(), 0);
        assert!(!c.readable());
        assert!(c.writable());
        assert_eq!(c.read(), Err(WouldBlock));

        // Draining from MAX_COUNTER restores writability immediately.
        assert_eq!(c.write(0xffff_ffff_ffff_fffe), Ok(()));
        assert!(!c.writable());
        assert_eq!(c.read(), Ok(0xffff_ffff_ffff_fffe));
        assert!(!c.readable());
        assert!(c.writable());
    }

    #[test]
    fn semaphore_mode_reads_one_and_decrements() {
        // fs/eventfd.c eventfd_ctx_do_read: EFD_SEMAPHORE reads 1 and decrements by 1.
        let mut c = EventCounter::new(2, true);
        assert!(c.is_semaphore());
        assert!(c.readable());
        assert!(c.writable());

        assert_eq!(c.read(), Ok(1));
        assert_eq!(c.count(), 1);
        assert!(c.readable());

        assert_eq!(c.read(), Ok(1));
        assert_eq!(c.count(), 0);
        assert!(!c.readable());
        assert_eq!(c.read(), Err(WouldBlock));

        // Reading 1 from MAX_COUNTER makes the counter writable again.
        assert_eq!(c.write(0xffff_ffff_ffff_fffe), Ok(()));
        assert!(!c.writable());
        assert_eq!(c.read(), Ok(1));
        assert_eq!(c.count(), 0xffff_ffff_ffff_fffd);
        assert!(c.readable());
        assert!(c.writable());
        assert_eq!(c.write(1), Ok(()));
        assert!(!c.writable());
    }

    #[test]
    fn byte_buffers_enforce_8_byte_minimum_before_state_checks() {
        // fs/eventfd.c eventfd_read / eventfd_write: count < sizeof(ucnt) -> -EINVAL.
        let mut c = EventCounter::new(0, false);
        for len in 0..8usize {
            let mut rbuf = [0xAAu8; 8];
            assert_eq!(c.read_bytes(&mut rbuf[..len]), Err(ReadBytesError::Invalid));
            let wbuf = [0xFFu8; 8];
            assert_eq!(c.write_bytes(&wbuf[..len]), Err(WriteError::Invalid));
        }

        // Exact 8-byte and oversized (> 8 bytes) transfers both transfer 8 bytes.
        let mut over = [0xCCu8; 16];
        over[..8].copy_from_slice(&42u64.to_ne_bytes());
        assert_eq!(c.write_bytes(&over), Ok(8));
        assert_eq!(c.count(), 42);

        let mut out = [0x55u8; 12];
        assert_eq!(c.read_bytes(&mut out), Ok(8));
        assert_eq!(u64::from_ne_bytes(out[..8].try_into().unwrap()), 42);
        // Trailing bytes beyond 8 are untouched.
        assert_eq!(&out[8..], &[0x55; 4]);

        // Empty counter with >= 8 byte buffer returns WouldBlock.
        assert_eq!(c.read_bytes(&mut out), Err(ReadBytesError::WouldBlock));

        // u64::MAX encoded in bytes returns Invalid.
        assert_eq!(c.write_bytes(&u64::MAX.to_ne_bytes()), Err(WriteError::Invalid));
    }

    #[test]
    fn zero_write_on_empty_counter_keeps_it_unreadable() {
        let mut c = EventCounter::new(0, false);
        assert_eq!(c.write(0), Ok(()));
        assert_eq!(c.count(), 0);
        assert!(!c.readable());
        assert!(c.writable());
        assert_eq!(c.read(), Err(WouldBlock));
    }
}
