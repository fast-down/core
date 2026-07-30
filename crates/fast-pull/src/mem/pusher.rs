//! In-memory pusher backed by a shared `Vec<u8>` (feature `mem`).

use crate::{ProgressEntry, ProgressListener, Pusher};
use bytes::Bytes;
use parking_lot::Mutex;
use std::{sync::Arc, vec::Vec};

/// In-memory pusher for testing or buffer-based workflows.
///
/// All pushed data is stored in a shared `Vec<u8>` protected by a mutex.
/// Supports random-access writes via `copy_from_slice` for non-sequential ranges.
#[derive(Default)]
pub struct MemPusher {
    pub receive: Arc<Mutex<Vec<u8>>>,
    listener: Option<ProgressListener>,
}
impl Clone for MemPusher {
    fn clone(&self) -> Self {
        Self {
            receive: self.receive.clone(),
            listener: None,
        }
    }
}
impl MemPusher {
    #[must_use]
    pub fn new() -> Self {
        Self {
            receive: Arc::new(Mutex::new(Vec::new())),
            listener: None,
        }
    }
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            receive: Arc::new(Mutex::new(Vec::with_capacity(capacity))),
            listener: None,
        }
    }
}
impl std::fmt::Debug for MemPusher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemPusher")
            .field("receive", &self.receive)
            .finish_non_exhaustive()
    }
}
impl Pusher for MemPusher {
    type Error = std::convert::Infallible;

    fn set_listener(&mut self, cb: ProgressListener) {
        self.listener = Some(cb);
    }

    fn push(&mut self, range: &ProgressEntry, content: Bytes) -> Result<(), (Self::Error, Bytes)> {
        #![allow(clippy::significant_drop_tightening, clippy::cast_possible_truncation)]
        let mut guard = self.receive.lock();
        if range.start as usize == guard.len() {
            guard.extend_from_slice(&content);
        } else {
            if guard.len() < range.end as usize {
                guard.resize(range.end as usize, 0);
            }
            guard[range.start as usize..range.end as usize].copy_from_slice(&content);
        }
        drop(guard);
        if let Some(l) = &mut self.listener {
            l(range.clone());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn sequential_append() {
        let mut p = MemPusher::new();
        p.push(&(0..3), Bytes::copy_from_slice(b"abc")).unwrap();
        p.push(&(3..6), Bytes::copy_from_slice(b"def")).unwrap();
        assert_eq!(&p.receive.lock()[..], b"abcdef");
    }

    #[test]
    fn random_access_resizes_and_writes() {
        let mut p = MemPusher::new();
        p.push(&(5..8), Bytes::copy_from_slice(b"xyz")).unwrap();
        // The gap before the write is filled with zeros via resize.
        assert_eq!(p.receive.lock().len(), 8);
        p.push(&(0..5), Bytes::copy_from_slice(b"hello")).unwrap();
        assert_eq!(&p.receive.lock()[..], b"helloxyz");
    }

    #[test]
    fn empty_push_is_noop() {
        let mut p = MemPusher::new();
        p.push(&(0..0), Bytes::new()).unwrap();
        assert!(p.receive.lock().is_empty());
    }

    #[test]
    fn listener_invoked_with_range() {
        let mut p = MemPusher::new();
        let seen = Arc::new(Mutex::new(None::<ProgressEntry>));
        let seen2 = seen.clone();
        p.set_listener(Box::new(move |r| {
            *seen2.lock().unwrap() = Some(r);
        }));
        p.push(&(0..4), Bytes::copy_from_slice(b"data")).unwrap();
        assert_eq!(*seen.lock().unwrap(), Some(0..4));
    }

    #[test]
    fn clone_shares_receive() {
        let mut p = MemPusher::new();
        p.push(&(0..2), Bytes::copy_from_slice(b"hi")).unwrap();
        let mut q = p.clone();
        q.push(&(2..4), Bytes::copy_from_slice(b"ya")).unwrap();
        // Both handles observe the same underlying vec.
        assert_eq!(&p.receive.lock()[..], b"hiya");
    }

    #[test]
    fn debug_impl() {
        // Lines 42-46: the `Debug` impl for `MemPusher`.
        let p = MemPusher::new();
        let _ = format!("{p:?}");
    }
}
