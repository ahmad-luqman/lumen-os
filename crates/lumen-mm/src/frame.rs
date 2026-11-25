//! Atomic user-leaf reference counts over the physical allocator's span.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::{FRAME_SIZE, PhysFrame, PhysRange};

/// A refused update never changes the count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefError {
    OutOfRange,
    AlreadyLive,
    NotLive,
    Overflow,
}

/// Whether a decrement released the last reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Remaining {
    Free,
    Live(u32),
}

/// One atomic count per frame, with no allocation or hardware access.
///
/// Zero means uncounted; `set_one` accepts only zero, `inc` accepts only a
/// live count below `u32::MAX`, and `dec` accepts only a live count. Overflow
/// and underflow return errors without wrapping or changing the count.
/// The caller owns allocation and reclamation: it must hold a reference
/// while incrementing and retire every mapping before decrementing.
#[derive(Clone, Copy)]
pub struct RefTable<'a> {
    span: PhysRange,
    counts: &'a [AtomicU32],
}

impl<'a> RefTable<'a> {
    /// Zero the counts covering `span` rounded outward, leaving excess
    /// storage untouched. Returns `None` without changes if storage is short.
    pub fn new(span: PhysRange, storage: &'a mut [AtomicU32]) -> Option<Self> {
        let span = span.grow_to_frames();
        let counts = storage.get_mut(..Self::slots(span))?;
        for count in counts.iter_mut() {
            *count.get_mut() = 0;
        }
        Some(Self { span, counts })
    }

    /// Number of atomic counters needed for the outward-rounded span.
    pub const fn slots(span: PhysRange) -> usize {
        (span.grow_to_frames().len() / FRAME_SIZE) as usize
    }

    fn slot(&self, frame: PhysFrame) -> Result<&AtomicU32, RefError> {
        let addr = frame.as_u64();
        if !self.span.contains(addr) {
            return Err(RefError::OutOfRange);
        }
        Ok(&self.counts[((addr - self.span.start()) / FRAME_SIZE) as usize])
    }

    /// The current count, or `None` outside the managed span.
    pub fn get(&self, frame: PhysFrame) -> Option<u32> {
        self.slot(frame).ok().map(|count| count.load(Ordering::Acquire))
    }

    /// Start the count of a newly allocated frame. Refuses a live frame.
    pub fn set_one(&self, frame: PhysFrame) -> Result<(), RefError> {
        self.slot(frame)?
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| RefError::AlreadyLive)
    }

    /// Add one reference, refusing zero and `u32::MAX`.
    pub fn inc(&self, frame: PhysFrame) -> Result<(), RefError> {
        self.slot(frame)?
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| if n == 0 { None } else { n.checked_add(1) })
            .map(|_| ())
            .map_err(|n| if n == 0 { RefError::NotLive } else { RefError::Overflow })
    }

    /// Release one reference, reporting whether the frame may be freed.
    pub fn dec(&self, frame: PhysFrame) -> Result<Remaining, RefError> {
        self.slot(frame)?
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
            .map(|n| if n == 1 { Remaining::Free } else { Remaining::Live(n - 1) })
            .map_err(|_| RefError::NotLive)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::collections::BTreeMap;

    fn f(n: u64) -> PhysFrame {
        PhysFrame::from_u64(n * FRAME_SIZE).unwrap()
    }

    #[test]
    fn storage_bounds_and_initialization() {
        let span = PhysRange::new(FRAME_SIZE + 1, 4 * FRAME_SIZE - 1);
        assert_eq!(RefTable::slots(span), 3);
        let mut short = [const { AtomicU32::new(7) }; 2];
        assert!(RefTable::new(span, &mut short).is_none());
        assert_eq!(short[0].load(Ordering::Relaxed), 7);
        let mut storage = [const { AtomicU32::new(7) }; 4];
        let table = RefTable::new(span, &mut storage).unwrap();
        for n in 1..4 {
            assert_eq!(table.get(f(n)), Some(0));
        }
        for n in [0, 4, 100] {
            assert_eq!(table.get(f(n)), None);
            assert_eq!(table.set_one(f(n)), Err(RefError::OutOfRange));
            assert_eq!(table.inc(f(n)), Err(RefError::OutOfRange));
            assert_eq!(table.dec(f(n)), Err(RefError::OutOfRange));
        }
        assert_eq!(storage[3].load(Ordering::Relaxed), 7);
        assert_eq!(RefTable::slots(PhysRange::new(7, 7)), 0);
    }

    #[test]
    fn limits_refuse_overflow_underflow_and_reinitialization() {
        let mut storage = [AtomicU32::new(0)];
        let table = RefTable::new(PhysRange::new(FRAME_SIZE, 2 * FRAME_SIZE), &mut storage).unwrap();
        assert_eq!(table.inc(f(1)), Err(RefError::NotLive));
        assert_eq!(table.dec(f(1)), Err(RefError::NotLive));
        assert_eq!(table.get(f(1)), Some(0));
        assert_eq!(table.set_one(f(1)), Ok(()));
        assert_eq!(table.set_one(f(1)), Err(RefError::AlreadyLive));
        assert_eq!(table.get(f(1)), Some(1));
        table.counts[0].store(u32::MAX - 1, Ordering::Relaxed);
        assert_eq!(table.inc(f(1)), Ok(()));
        assert_eq!(table.inc(f(1)), Err(RefError::Overflow));
        assert_eq!(table.get(f(1)), Some(u32::MAX));
        assert_eq!(table.dec(f(1)), Ok(Remaining::Live(u32::MAX - 1)));
        table.counts[0].store(1, Ordering::Relaxed);
        assert_eq!(table.dec(f(1)), Ok(Remaining::Free));
        assert_eq!(table.dec(f(1)), Err(RefError::NotLive));
        assert_eq!(table.get(f(1)), Some(0));
        assert_eq!(table.set_one(f(1)), Ok(()));
    }

    #[test]
    fn random_operations_match_plain_map() {
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        let mut storage = [const { AtomicU32::new(0) }; 32];
        let table = RefTable::new(PhysRange::new(3 * FRAME_SIZE, 35 * FRAME_SIZE), &mut storage).unwrap();
        let mut model = BTreeMap::<u64, u32>::new();
        let mut outcomes = [0; 6];
        for _ in 0..50_000 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let n = 3 + rng % 32;
            let count = model.entry(n).or_default();
            match (rng >> 16) % 3 {
                0 => {
                    let expected = if *count == 0 { Ok(()) } else { Err(RefError::AlreadyLive) };
                    assert_eq!(table.set_one(f(n)), expected);
                    outcomes[usize::from(expected.is_err())] += 1;
                    if expected.is_ok() {
                        *count = 1;
                    }
                }
                1 => {
                    let expected = if *count == 0 { Err(RefError::NotLive) } else { Ok(()) };
                    assert_eq!(table.inc(f(n)), expected);
                    outcomes[2 + usize::from(expected.is_err())] += 1;
                    if expected.is_ok() {
                        *count += 1;
                    }
                }
                _ => {
                    let expected = match *count {
                        0 => Err(RefError::NotLive),
                        1 => Ok(Remaining::Free),
                        n => Ok(Remaining::Live(n - 1)),
                    };
                    assert_eq!(table.dec(f(n)), expected);
                    outcomes[4 + usize::from(expected.is_err())] += 1;
                    if expected.is_ok() {
                        *count -= 1;
                    }
                }
            }
            for n in 3..35 {
                assert_eq!(table.get(f(n)), Some(*model.get(&n).unwrap_or(&0)));
            }
        }
        assert!(outcomes.iter().all(|&n| n > 0), "{outcomes:?}");
    }

    #[test]
    fn concurrent_references_balance() {
        let mut storage = [AtomicU32::new(0)];
        let table = RefTable::new(PhysRange::new(FRAME_SIZE, 2 * FRAME_SIZE), &mut storage).unwrap();
        table.set_one(f(1)).unwrap();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..10_000 {
                        table.inc(f(1)).unwrap();
                        assert!(matches!(table.dec(f(1)), Ok(Remaining::Live(_))));
                    }
                });
            }
        });
        assert_eq!(table.get(f(1)), Some(1));
        assert_eq!(table.dec(f(1)), Ok(Remaining::Free));
    }
}
