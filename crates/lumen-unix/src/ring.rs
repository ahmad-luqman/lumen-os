//! Wrap-around byte ring (`ByteRing`, `ByteStorage`) and shared `VecDeque`
//! fallible-reserve / non-allocating swap-growth helpers (`try_deque`,
//! `grow_deque`) used across `stream`, `dgram`, and `listener`.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::fmt;
use core::mem;

use crate::{NoMemory, alloc_zeroed_buf};

/// Fallibly allocate an empty `VecDeque<T>` with at least `cap` usable slots.
pub(crate) fn try_deque<T>(cap: usize) -> Result<VecDeque<T>, NoMemory> {
    let mut deque = VecDeque::new();
    deque.try_reserve_exact(cap).map_err(|_| NoMemory)?;
    Ok(deque)
}

/// Swap `spare` into `dst` without allocating if `spare.capacity() > dst.capacity()`,
/// moving all items in FIFO order and leaving the previous (now empty) backing
/// storage in `spare` for the caller to drop outside any lock.
pub(crate) fn grow_deque<T>(dst: &mut VecDeque<T>, spare: &mut VecDeque<T>) {
    if spare.capacity() <= dst.capacity() {
        return;
    }
    debug_assert!(spare.is_empty());
    while let Some(item) = dst.pop_front() {
        spare.push_back(item);
    }
    mem::swap(dst, spare);
}

/// Pre-allocated ring buffer storage allocated outside any socket lock and
/// swapped in via [`StreamPair::grow_buffer`](crate::StreamPair::grow_buffer).
pub struct ByteStorage {
    pub(crate) buf: Vec<u8>,
}

impl fmt::Debug for ByteStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ByteStorage").field("capacity", &self.buf.len()).finish()
    }
}

impl ByteStorage {
    /// Fallibly allocate a zeroed `cap`-byte ring buffer.
    ///
    /// # Errors
    ///
    /// Returns [`NoMemory`] if allocation fails.
    pub fn try_with_capacity(cap: usize) -> Result<Self, NoMemory> {
        Ok(Self { buf: alloc_zeroed_buf(cap)? })
    }

    /// Capacity of this storage in bytes.
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }
}

/// Fixed-capacity wrap-around byte ring with a centralized zero-length guard
/// and zero-allocation storage swapping.
pub(crate) struct ByteRing {
    buf: Vec<u8>,
    head: usize,
    len: usize,
}

impl ByteRing {
    pub(crate) fn try_new(cap: usize) -> Result<Self, NoMemory> {
        Ok(Self { buf: alloc_zeroed_buf(cap)?, head: 0, len: 0 })
    }

    pub(crate) const fn capacity(&self) -> usize {
        self.buf.len()
    }

    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    pub(crate) const fn free(&self) -> usize {
        self.buf.len() - self.len
    }

    pub(crate) fn push_bytes(&mut self, src: &[u8]) {
        let n = src.len();
        if n == 0 {
            return;
        }
        debug_assert!(n <= self.free());
        let cap = self.buf.len();
        let tail = (self.head + self.len) % cap;
        let first = n.min(cap - tail);
        self.buf[tail..tail + first].copy_from_slice(&src[..first]);
        let second = n - first;
        self.buf[..second].copy_from_slice(&src[first..]);
        self.len += n;
    }

    pub(crate) fn copy_out(&self, dst: &mut [u8], n: usize) {
        debug_assert!(n <= self.len);
        debug_assert!(n <= dst.len());
        if n == 0 {
            return;
        }
        let cap = self.buf.len();
        let first = n.min(cap - self.head);
        dst[..first].copy_from_slice(&self.buf[self.head..self.head + first]);
        let second = n - first;
        dst[first..n].copy_from_slice(&self.buf[..second]);
    }

    pub(crate) fn consume(&mut self, n: usize) {
        debug_assert!(n <= self.len);
        if n == 0 {
            return;
        }
        let cap = self.buf.len();
        self.head = (self.head + n) % cap;
        self.len -= n;
        if self.len == 0 {
            self.head = 0;
        }
    }

    pub(crate) fn clear(&mut self) {
        self.len = 0;
        self.head = 0;
    }

    /// Swap in `spare` if it is strictly larger than the current backing
    /// buffer, linearizing any wrapped bytes into `spare` without allocating
    /// and returning the previous buffer (or `spare` unchanged) for the caller
    /// to drop outside any lock.
    pub(crate) fn grow_storage(&mut self, mut spare: ByteStorage) -> ByteStorage {
        if spare.buf.len() <= self.buf.len() {
            return spare;
        }
        self.copy_out(&mut spare.buf, self.len);
        mem::swap(&mut self.buf, &mut spare.buf);
        self.head = 0;
        spare
    }
}
