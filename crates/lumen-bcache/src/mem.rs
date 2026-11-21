use alloc::vec;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::{BlockDevice, BlockError, BlockInfo};

struct SpinMutex<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: Access to `value` is serialized by `locked`.
unsafe impl<T: Send> Sync for SpinMutex<T> {}
// SAFETY: Moving the mutex moves `value`, which is `Send`.
unsafe impl<T: Send> Send for SpinMutex<T> {}

impl<T> SpinMutex<T> {
    const fn new(value: T) -> Self {
        Self { locked: AtomicBool::new(false), value: UnsafeCell::new(value) }
    }

    fn lock(&self) -> SpinMutexGuard<'_, T> {
        while self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        SpinMutexGuard(self)
    }
}

struct SpinMutexGuard<'a, T>(&'a SpinMutex<T>);

impl<T> Deref for SpinMutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: We exclusively hold the spin mutex.
        unsafe { &*self.0.value.get() }
    }
}

impl<T> DerefMut for SpinMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: We exclusively hold the spin mutex.
        unsafe { &mut *self.0.value.get() }
    }
}

impl<T> Drop for SpinMutexGuard<'_, T> {
    fn drop(&mut self) {
        self.0.locked.store(false, Ordering::Release);
    }
}

#[derive(Clone, Copy)]
enum WriteFault {
    Immediate(BlockError),
    After { remaining_successes: usize, err: BlockError },
}

struct MemState {
    volatile: Vec<u8>,
    durable: Vec<u8>,
    read_log: Vec<(u64, usize)>,
    write_log: Vec<(u64, usize)>,
    flush_calls: u64,
    next_read_err: Option<BlockError>,
    next_write_err: Option<WriteFault>,
    next_flush_err: Option<BlockError>,
}

/// In-memory [`BlockDevice`] with volatile/durable storage and fault injection.
///
/// `write_sectors` updates only the `volatile` buffer; `flush` commits
/// `volatile` to `durable`; `simulate_power_loss` discards uncommitted
/// `volatile` writes by restoring `volatile` from `durable`.
pub struct MemBlockDevice {
    info: BlockInfo,
    state: SpinMutex<MemState>,
}

impl MemBlockDevice {
    /// Create a read-write, flush-capable in-memory device filled with zeros.
    pub fn new(sector_size: u32, sector_count: u64) -> Self {
        Self::with_options(sector_size, sector_count, false, true)
    }

    /// Create an in-memory device with custom `read_only` and `flush_supported` flags.
    pub fn with_options(sector_size: u32, sector_count: u64, read_only: bool, flush_supported: bool) -> Self {
        let len = usize::try_from(sector_count.saturating_mul(u64::from(sector_size))).unwrap_or(0);
        Self {
            info: BlockInfo { sector_size, sector_count, read_only, flush_supported },
            state: SpinMutex::new(MemState {
                volatile: vec![0u8; len],
                durable: vec![0u8; len],
                read_log: Vec::new(),
                write_log: Vec::new(),
                flush_calls: 0,
                next_read_err: None,
                next_write_err: None,
                next_flush_err: None,
            }),
        }
    }

    /// Initialize both `volatile` and `durable` contents from `data`.
    pub fn seed(&self, offset: usize, data: &[u8]) {
        let mut s = self.state.lock();
        s.volatile[offset..offset + data.len()].copy_from_slice(data);
        s.durable[offset..offset + data.len()].copy_from_slice(data);
    }

    /// Simulate power loss by overwriting `volatile` with `durable`.
    pub fn simulate_power_loss(&self) {
        let mut s = self.state.lock();
        let durable = s.durable.clone();
        s.volatile.copy_from_slice(&durable);
    }

    /// Snapshot of current `volatile` bytes.
    pub fn volatile_snapshot(&self) -> Vec<u8> {
        self.state.lock().volatile.clone()
    }

    /// Snapshot of current `durable` bytes.
    pub fn durable_snapshot(&self) -> Vec<u8> {
        self.state.lock().durable.clone()
    }

    /// Recorded `(lba, len_bytes)` calls to [`BlockDevice::read_sectors`].
    pub fn read_log(&self) -> Vec<(u64, usize)> {
        self.state.lock().read_log.clone()
    }

    /// Recorded `(lba, len_bytes)` calls to [`BlockDevice::write_sectors`].
    pub fn write_log(&self) -> Vec<(u64, usize)> {
        self.state.lock().write_log.clone()
    }

    /// Number of [`BlockDevice::flush`] calls observed.
    pub fn flush_calls(&self) -> u64 {
        self.state.lock().flush_calls
    }

    /// Clear recorded read/write logs and flush counter.
    pub fn clear_logs(&self) {
        let mut s = self.state.lock();
        s.read_log.clear();
        s.write_log.clear();
        s.flush_calls = 0;
    }

    /// Fail the next [`BlockDevice::read_sectors`] call with `err`.
    pub fn inject_read_error(&self, err: BlockError) {
        self.state.lock().next_read_err = Some(err);
    }

    /// Fail the next [`BlockDevice::write_sectors`] call with `err`.
    pub fn inject_write_error(&self, err: BlockError) {
        self.state.lock().next_write_err = Some(WriteFault::Immediate(err));
    }

    /// Allow `successes` write calls to succeed, then fail the next with `err`.
    pub fn inject_write_error_after(&self, successes: usize, err: BlockError) {
        self.state.lock().next_write_err = Some(WriteFault::After { remaining_successes: successes, err });
    }

    /// Fail the next [`BlockDevice::flush`] call with `err`.
    pub fn inject_flush_error(&self, err: BlockError) {
        self.state.lock().next_flush_err = Some(err);
    }

    fn byte_range(&self, lba: u64, len: usize) -> Result<(usize, usize), BlockError> {
        let sector_size = usize::try_from(self.info.sector_size).map_err(|_| BlockError::Unaligned)?;
        if len == 0 || !len.is_multiple_of(sector_size) {
            return Err(BlockError::Unaligned);
        }
        let sectors = (len / sector_size) as u64;
        let end_lba = lba.checked_add(sectors).ok_or(BlockError::OutOfRange)?;
        if end_lba > self.info.sector_count {
            return Err(BlockError::OutOfRange);
        }
        let start = usize::try_from(lba.checked_mul(u64::from(self.info.sector_size)).ok_or(BlockError::OutOfRange)?)
            .map_err(|_| BlockError::OutOfRange)?;
        let end = start.checked_add(len).ok_or(BlockError::OutOfRange)?;
        Ok((start, end))
    }
}

impl BlockDevice for MemBlockDevice {
    fn info(&self) -> BlockInfo {
        self.info
    }

    fn read_sectors(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let (start, end) = self.byte_range(lba, buf.len())?;
        let mut s = self.state.lock();
        s.read_log.push((lba, buf.len()));
        if let Some(err) = s.next_read_err.take() {
            return Err(err);
        }
        buf.copy_from_slice(&s.volatile[start..end]);
        Ok(())
    }

    fn write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
        if self.info.read_only {
            return Err(BlockError::ReadOnly);
        }
        let (start, end) = self.byte_range(lba, buf.len())?;
        let mut s = self.state.lock();
        s.write_log.push((lba, buf.len()));
        if let Some(fault) = s.next_write_err.take() {
            match fault {
                WriteFault::Immediate(err) => return Err(err),
                WriteFault::After { remaining_successes: 0, err } => return Err(err),
                WriteFault::After { remaining_successes, err } => {
                    s.next_write_err = Some(WriteFault::After { remaining_successes: remaining_successes - 1, err });
                }
            }
        }
        s.volatile[start..end].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&self) -> Result<(), BlockError> {
        let mut s = self.state.lock();
        s.flush_calls += 1;
        if let Some(err) = s.next_flush_err.take() {
            return Err(err);
        }
        let volatile = s.volatile.clone();
        s.durable.copy_from_slice(&volatile);
        Ok(())
    }
}
