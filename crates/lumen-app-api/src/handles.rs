//! An app's handle table: what its handles name. A handle is an opaque id
//! in 1..=`i32::MAX`, never reused within the app's life: once the ids are
//! used up, minting another is [`Error::Limit`] rather than a wrap (tested
//! with a small cap). A missing, closed or made-up handle is
//! [`Error::BadHandle`].

use crate::Error;
use alloc::vec::Vec;

/// Handles to `T`s.
#[derive(Debug)]
pub struct HandleTable<T> {
    /// The next id to hand out.
    next: u32,
    /// The last id that may be handed out.
    last: u32,
    open: Vec<(u32, T)>,
}

impl<T> Default for HandleTable<T> {
    fn default() -> Self {
        HandleTable::new()
    }
}

impl<T> HandleTable<T> {
    /// A table handing out 1..=`i32::MAX`.
    pub const fn new() -> HandleTable<T> {
        HandleTable::with_last(i32::MAX as u32)
    }

    /// A table handing out 1..=`last` (`last` at most `i32::MAX`).
    pub const fn with_last(last: u32) -> HandleTable<T> {
        assert!(last <= i32::MAX as u32);
        HandleTable { next: 1, last, open: Vec::new() }
    }

    /// A new handle to `object`: [`Error::Limit`] once the ids are used up.
    pub fn insert(&mut self, object: T) -> Result<i32, Error> {
        if self.next > self.last {
            return Err(Error::Limit);
        }
        let h = self.next;
        self.next += 1;
        self.open.push((h, object));
        Ok(h as i32)
    }

    /// What handle `h` names.
    pub fn get(&self, h: i32) -> Result<&T, Error> {
        self.open.iter().find(|(id, _)| *id as i32 == h && h > 0).map(|(_, o)| o).ok_or(Error::BadHandle)
    }

    /// Close handle `h`, giving back what it named.
    pub fn remove(&mut self, h: i32) -> Result<T, Error> {
        let at = self.open.iter().position(|(id, _)| *id as i32 == h && h > 0).ok_or(Error::BadHandle)?;
        Ok(self.open.remove(at).1)
    }

    /// The open handle whose object matches, if any.
    pub fn find(&self, mut matches: impl FnMut(&T) -> bool) -> Option<i32> {
        self.open.iter().find(|(_, o)| matches(o)).map(|(id, _)| *id as i32)
    }

    /// Open handles.
    pub fn len(&self) -> usize {
        self.open.len()
    }

    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }

    /// The open handles' objects.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.open.iter().map(|(_, o)| o)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_name_what_they_were_given_and_are_never_reused() {
        let mut t = HandleTable::new();
        let a = t.insert('a').unwrap();
        let b = t.insert('b').unwrap();
        assert_eq!((a, b), (1, 2));
        assert_eq!(t.get(a), Ok(&'a'));
        assert_eq!(t.remove(a), Ok('a'));
        assert_eq!(t.get(a), Err(Error::BadHandle));
        assert_eq!(t.remove(a), Err(Error::BadHandle));
        // A new one never takes a closed one's id.
        assert_eq!(t.insert('c'), Ok(3));
        assert_eq!(t.find(|o| *o == 'c'), Some(3));
        assert_eq!(t.find(|o| *o == 'a'), None);
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn made_up_handles_are_bad() {
        let mut t = HandleTable::new();
        t.insert(()).unwrap();
        for h in [0, -1, i32::MIN, 2, i32::MAX] {
            assert_eq!(t.get(h), Err(Error::BadHandle), "{h}");
        }
    }

    #[test]
    fn used_up_ids_are_a_limit_not_a_wrap() {
        let mut t = HandleTable::with_last(3);
        for want in 1..=3 {
            let h = t.insert(want).unwrap();
            assert_eq!(h, want);
            t.remove(h).unwrap();
        }
        assert_eq!(t.insert(4), Err(Error::Limit));
        assert!(t.is_empty());
        // The default table hands out up to i32::MAX.
        let mut full: HandleTable<()> = HandleTable::with_last(i32::MAX as u32);
        full.next = i32::MAX as u32;
        assert_eq!(full.insert(()), Ok(i32::MAX));
        assert_eq!(full.insert(()), Err(Error::Limit));
    }
}
