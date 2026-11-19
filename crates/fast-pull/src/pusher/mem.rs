//! In-memory pusher backed by a shared `Vec<u8>`.

use crate::{ProgressEntry, ProgressListener, Pusher};
use bytes::Bytes;
use parking_lot::Mutex;
use std::{sync::Arc, vec::Vec};

/// In-memory pusher for testing or buffer-based workflows.
///
/// All pushed data is stored in a shared `Vec<u8>` protected by a mutex.
/// Supports random-access writes via `copy_from_slice` for non-sequential ranges.
/// A chunk whose length does not match its range is rejected with
/// [`std::io::ErrorKind::InvalidInput`] instead of being written truncated or
/// panicking.
#[derive(Default)]
pub struct MemPusher {
    /// The shared buffer the pusher writes into.
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
    /// Create an empty in-memory pusher.
    #[must_use]
    pub fn new() -> Self {
        Self {
            receive: Arc::new(Mutex::new(Vec::new())),
            listener: None,
        }
    }
    /// Create an in-memory pusher with a pre-allocated buffer of `capacity` bytes.
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
    type Error = std::io::Error;

    fn set_listener(&mut self, cb: ProgressListener) {
        self.listener = Some(cb);
    }

    fn push(&mut self, range: &ProgressEntry, content: Bytes) -> Result<(), (Self::Error, Bytes)> {
        #![allow(clippy::significant_drop_tightening)]
        // The engine derives written bytes from `content.len()` and the range,
        // so a mismatch would slice the buffer at the wrong offsets (or panic in
        // `copy_from_slice`). Reject it before touching the destination.
        let (Ok(start), Ok(end)) = (usize::try_from(range.start), usize::try_from(range.end))
        else {
            return Err((
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("push range {range:?} does not fit in usize"),
                ),
                content,
            ));
        };
        let Some(len) = end.checked_sub(start) else {
            return Err((
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("push range {range:?} is reversed"),
                ),
                content,
            ));
        };
        if content.len() != len {
            return Err((
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "push range {range:?} covers {len} bytes but content has {}",
                        content.len()
                    ),
                ),
                content,
            ));
        }
        let mut guard = self.receive.lock();
        if start == guard.len() {
            guard.extend_from_slice(&content);
        } else {
            if guard.len() < end {
                guard.resize(end, 0);
            }
            guard[start..end].copy_from_slice(&content);
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
        // the `Debug` impl for `MemPusher`.
        let p = MemPusher::new();
        let _ = format!("{p:?}");
    }

    #[test]
    fn length_mismatch_returns_error_and_writes_nothing() {
        // The engine indexes `content` by `range`, so a mismatched chunk must be
        // rejected instead of panicking in `copy_from_slice` or writing a
        // truncated range.
        let mut p = MemPusher::new();
        let payload = Bytes::copy_from_slice(b"abc");
        let (err, returned) = p.push(&(0..5), payload.clone()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(returned, payload, "the caller keeps the rejected bytes");
        assert!(
            p.receive.lock().is_empty(),
            "a rejected push must not touch the buffer"
        );
    }

    #[test]
    fn reversed_range_returns_error() {
        let mut p = MemPusher::new();
        let range = ProgressEntry { start: 5, end: 3 };
        let (err, _) = p.push(&range, Bytes::new()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(p.receive.lock().is_empty());
    }

    #[cfg(target_pointer_width = "32")]
    #[test]
    fn range_beyond_usize_returns_error() {
        // On a 32-bit target the range end cannot be narrowed to `usize`; the
        // pusher must report that instead of truncating it.
        let mut p = MemPusher::new();
        let end = u64::from(u32::MAX) + 1;
        let (err, _) = p.push(&(0..end), Bytes::new()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(p.receive.lock().is_empty());
    }
}
