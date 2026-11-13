//! A lock-free single-producer, single-consumer byte queue.
//!
//! An interrupt handler pushes bytes (a keyboard's scancodes) and a thread
//! pops them, without a lock: the handler must never wait for the code it
//! interrupted. [`ByteQueue`] is a ring of `N` atomic bytes (`N` a power of
//! two) with two counters that only grow: `tail` (bytes pushed) and `head`
//! (bytes popped), so `tail - head` is how many are waiting.
//!
//! - **Push** (the producer): if the ring is full the byte is dropped and
//!   counted ([`ByteQueue::dropped`]), never waited for; otherwise it is
//!   stored in its slot, then `tail` is published (Release), so a consumer
//!   that sees the new `tail` (Acquire) sees the byte.
//! - **Pop** (the consumer): reads `tail` (Acquire), takes the byte at
//!   `head`, then publishes `head + 1` (Release), so the producer reuses the
//!   slot only after the byte was read.
//!
//! Only one producer and one consumer may run at a time: callers serialize
//! each side (the kernel's producer is one CPU's interrupt handler, its
//! consumers take a lock). The code is safe Rust: misuse (two producers at
//! once) can lose or repeat bytes, but never causes undefined behaviour.

#![cfg_attr(not(test), no_std)]

use core::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};

/// A lock-free SPSC queue of up to `N` bytes (`N` a power of two).
pub struct ByteQueue<const N: usize> {
    slots: [AtomicU8; N],
    /// Bytes pushed so far (the producer's).
    tail: AtomicUsize,
    /// Bytes popped so far (the consumer's).
    head: AtomicUsize,
    dropped: AtomicU64,
}

impl<const N: usize> ByteQueue<N> {
    const POWER_OF_TWO: () = assert!(N.is_power_of_two(), "a ByteQueue's size is a power of two");

    pub const fn new() -> Self {
        #[allow(clippy::let_unit_value)]
        let () = Self::POWER_OF_TWO;
        ByteQueue {
            slots: [const { AtomicU8::new(0) }; N],
            tail: AtomicUsize::new(0),
            head: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    /// Add `byte` at the back (producer only); false if the queue was full
    /// (the byte is dropped and counted).
    pub fn push(&self, byte: u8) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        if tail.wrapping_sub(self.head.load(Ordering::Acquire)) >= N {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.slots[tail % N].store(byte, Ordering::Relaxed);
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        true
    }

    /// Take the byte at the front (consumer only), if any.
    pub fn pop(&self) -> Option<u8> {
        let head = self.head.load(Ordering::Relaxed);
        if head == self.tail.load(Ordering::Acquire) {
            return None;
        }
        let byte = self.slots[head % N].load(Ordering::Relaxed);
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Some(byte)
    }

    /// How many bytes are waiting (a snapshot).
    pub fn len(&self) -> usize {
        self.tail.load(Ordering::Acquire).wrapping_sub(self.head.load(Ordering::Acquire))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl<const N: usize> Default for ByteQueue<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn bytes_come_out_in_order() {
        let q = ByteQueue::<8>::new();
        assert_eq!(q.pop(), None);
        for b in 1..=5 {
            assert!(q.push(b));
        }
        assert_eq!(q.len(), 5);
        assert_eq!((1..=5).map(|_| q.pop().unwrap()).collect::<Vec<_>>(), [1, 2, 3, 4, 5]);
        assert_eq!(q.pop(), None);
        assert!(q.is_empty());
    }

    #[test]
    fn a_full_queue_drops_and_counts() {
        let q = ByteQueue::<4>::new();
        for b in 0..4 {
            assert!(q.push(b));
        }
        assert!(!q.push(99));
        assert!(!q.push(98));
        assert_eq!(q.dropped(), 2);
        assert_eq!(q.pop(), Some(0));
        assert!(q.push(4)); // room again
        assert_eq!((0..4).map(|_| q.pop().unwrap()).collect::<Vec<_>>(), [1, 2, 3, 4]);
    }

    #[test]
    fn it_wraps_around_many_times() {
        let q = ByteQueue::<4>::new();
        for i in 0..1000u32 {
            assert!(q.push(i as u8));
            assert!(q.push((i + 1) as u8));
            assert_eq!(q.pop(), Some(i as u8));
            assert_eq!(q.pop(), Some((i + 1) as u8));
        }
        assert_eq!(q.dropped(), 0);
    }

    #[test]
    fn counters_wrap_past_usize_max() {
        let q = ByteQueue::<4>::new();
        q.head.store(usize::MAX - 1, Ordering::Relaxed);
        q.tail.store(usize::MAX - 1, Ordering::Relaxed);
        for b in 0..4 {
            assert!(q.push(b));
        }
        assert!(!q.push(4));
        assert_eq!(q.len(), 4);
        assert_eq!((0..4).map(|_| q.pop().unwrap()).collect::<Vec<_>>(), [0, 1, 2, 3]);
    }

    #[test]
    fn a_producer_and_a_consumer_thread_agree() {
        // 200k bytes through a 16-byte queue: the consumer sees exactly the
        // pushed sequence (bytes that didn't fit are retried, not lost).
        const COUNT: u32 = 200_000;
        let q = Arc::new(ByteQueue::<16>::new());
        let producer = {
            let q = q.clone();
            std::thread::spawn(move || {
                for i in 0..COUNT {
                    while !q.push(i as u8) {
                        std::hint::spin_loop();
                    }
                }
            })
        };
        let mut next = 0u32;
        while next < COUNT {
            if let Some(b) = q.pop() {
                assert_eq!(b, next as u8, "byte {next}");
                next += 1;
            }
        }
        producer.join().unwrap();
        assert_eq!(q.pop(), None);
    }
}
