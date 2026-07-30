//! Pusher cache that reorders out-of-order chunks into sequential order.

use crate::{ProgressEntry, ProgressListener, Pusher};
use bytes::Bytes;
use std::collections::BTreeMap;

/// Pusher wrapper that reorders out-of-order chunks into sequential order.
///
/// Buffers chunks in a `BTreeMap` and flushes them in sequential order once gaps
/// are filled. When the cache exceeds `high_watermark`, a flush is triggered
/// to bring it down to `low_watermark`.
#[derive(Debug)]
pub struct CacheSeqPusher<P> {
    inner: P,
    cache: BTreeMap<u64, Bytes>,
    cache_size: usize,
    high_watermark: usize,
    low_watermark: usize,
}

impl<P: Pusher> CacheSeqPusher<P> {
    /// Wrap `inner` with the given `high_watermark` / `low_watermark` (in bytes).
    ///
    /// Eviction to the inner pusher triggers once the buffered size reaches
    /// `high_watermark`, and stops once it falls back to `low_watermark`.
    pub const fn new(inner: P, high_watermark: usize, low_watermark: usize) -> Self {
        Self {
            inner,
            cache: BTreeMap::new(),
            cache_size: 0,
            high_watermark,
            low_watermark,
        }
    }

    fn evict_until(&mut self, target_size: usize) -> Result<(), P::Error> {
        let mut expected = None;
        while let Some(entry) = self.cache.first_entry() {
            let start = *entry.key();
            if self.cache_size <= target_size && Some(start) != expected {
                break;
            }
            let chunk = entry.remove();
            let chunk_len = chunk.len();
            let next_pos = start + chunk_len as u64;
            self.cache_size -= chunk_len;
            if let Err((e, ret)) = self.inner.push(&(start..next_pos), chunk) {
                if !ret.is_empty() {
                    self.cache_size += ret.len();
                    self.cache.insert(next_pos - ret.len() as u64, ret);
                }
                return Err(e);
            }
            expected = Some(next_pos);
        }
        Ok(())
    }
}

impl<P: Pusher> Pusher for CacheSeqPusher<P> {
    type Error = P::Error;

    fn set_listener(&mut self, cb: ProgressListener) {
        self.inner.set_listener(cb);
    }

    fn push(&mut self, range: &ProgressEntry, bytes: Bytes) -> Result<(), (Self::Error, Bytes)> {
        if bytes.is_empty() {
            return Ok(());
        }

        self.cache_size += bytes.len();
        if let Some(old_bytes) = self.cache.insert(range.start, bytes) {
            self.cache_size -= old_bytes.len();
        }

        if self.cache_size >= self.high_watermark
            && let Err(e) = self.evict_until(self.low_watermark)
        {
            return Err((e, Bytes::new()));
        }

        Ok(())
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.evict_until(0)?;
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// Records every push and can be told to fail the next one (with the bytes
    /// returned so the cache can re-buffer them).
    #[derive(Clone)]
    struct RecordingSink {
        pushes: Arc<Mutex<Vec<(ProgressEntry, Bytes)>>>,
        fail_next: Arc<AtomicBool>,
        listener_set: Arc<AtomicBool>,
    }
    impl RecordingSink {
        fn new() -> Self {
            Self {
                pushes: Arc::new(Mutex::new(Vec::new())),
                fail_next: Arc::new(AtomicBool::new(false)),
                listener_set: Arc::new(AtomicBool::new(false)),
            }
        }
    }
    impl Pusher for RecordingSink {
        type Error = std::io::Error;
        fn set_listener(&mut self, _: ProgressListener) {
            self.listener_set.store(true, Ordering::SeqCst);
        }
        fn push(
            &mut self,
            range: &ProgressEntry,
            bytes: Bytes,
        ) -> Result<(), (Self::Error, Bytes)> {
            if self.fail_next.fetch_and(false, Ordering::SeqCst) {
                return Err((std::io::Error::other("boom"), bytes));
            }
            self.pushes.lock().unwrap().push((range.clone(), bytes));
            Ok(())
        }
        fn flush(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    fn bb(s: &str) -> Bytes {
        Bytes::copy_from_slice(s.as_bytes())
    }

    #[test]
    fn empty_push_is_noop() {
        // Line 69: a zero-length chunk returns `Ok(())` immediately.
        let sink = RecordingSink::new();
        let mut p = CacheSeqPusher::new(sink.clone(), 100, 0);
        p.push(&(0..0), Bytes::new()).unwrap();
        assert!(sink.pushes.lock().unwrap().is_empty());
    }

    #[test]
    fn duplicate_start_overwrites_old_bytes() {
        // Line 74: re-inserting at an already-cached start replaces the old entry
        // and adjusts `cache_size` so the total does not grow.
        let sink = RecordingSink::new();
        let mut p = CacheSeqPusher::new(sink.clone(), 100, 0);
        p.push(&(0..10), bb(&"A".repeat(10))).unwrap();
        p.push(&(0..10), bb(&"B".repeat(10))).unwrap();
        p.flush().unwrap();
        let pushes = sink.pushes.lock().unwrap();
        assert_eq!(pushes.len(), 1);
        assert_eq!(&pushes[0].1[..], b"BBBBBBBBBB");
        drop(pushes);
    }

    #[test]
    fn evict_stops_at_gap_once_below_target() {
        // Line 41: when the cache has dropped to <= low_watermark and the next
        // cached chunk is not contiguous with what was just flushed, eviction stops.
        let sink = RecordingSink::new();
        let mut p = CacheSeqPusher::new(sink.clone(), 100, 30);
        p.push(&(0..40), bb(&"A".repeat(40))).unwrap();
        p.push(&(40..80), bb(&"B".repeat(40))).unwrap();
        // This insertion reaches the high watermark (100) and triggers eviction to 30.
        // During eviction A (40) then B (40) are flushed; cache_size drops to 20 (<= 30)
        // but the next buffered chunk starts at 200 (a gap, not contiguous with the
        // previous flush end of 80), so the loop breaks at line 41 and C stays buffered.
        p.push(&(200..220), bb(&"C".repeat(20))).unwrap();

        let pushes = sink.pushes.lock().unwrap();
        assert_eq!(pushes.len(), 2);
        assert_eq!(pushes[0].0, 0..40);
        assert_eq!(pushes[1].0, 40..80);
        drop(pushes);
    }

    #[test]
    fn inner_push_failure_during_push_eviction_propagates() {
        // Lines 47-52 (re-buffer remainder) and 78-80 (propagate the error from `push`).
        let sink = RecordingSink::new();
        sink.fail_next.store(true, Ordering::SeqCst);
        let mut p = CacheSeqPusher::new(sink, 10, 0);
        let res = p.push(&(0..10), bb(&"A".repeat(10)));
        assert!(res.is_err());
    }

    #[test]
    fn inner_push_failure_during_flush_propagates() {
        // Lines 47-52 via `flush`: the failing chunk is re-buffered and the error surfaces.
        let sink = RecordingSink::new();
        sink.fail_next.store(true, Ordering::SeqCst);
        let mut p = CacheSeqPusher::new(sink, 100, 0);
        p.push(&(0..10), bb(&"A".repeat(10))).unwrap();
        assert!(p.flush().is_err());
    }

    #[test]
    fn set_listener_forwards_to_inner() {
        // Lines 63-65: the listener is forwarded to the inner pusher.
        let sink = RecordingSink::new();
        let mut p = CacheSeqPusher::new(sink.clone(), 100, 0);
        p.set_listener(Box::new(|_| {}));
        assert!(sink.listener_set.load(Ordering::SeqCst));
    }

    #[test]
    fn evict_breaks_on_gap_below_target() {
        // Line 41: once `cache_size` has dropped to <= target but the next
        // buffered chunk is not the expected sequential position, eviction
        // stops early and the gapped chunk remains buffered.
        let sink = RecordingSink::new();
        let mut p = CacheSeqPusher::new(sink.clone(), 20, 10);
        p.push(&(0..10), bb(&"A".repeat(10))).unwrap();
        p.push(&(20..30), bb(&"B".repeat(10))).unwrap();
        // [0..10] is flushed; [20..30] stays buffered because of the gap at 10.
        let pushes = sink.pushes.lock().unwrap();
        assert_eq!(pushes.len(), 1);
        assert_eq!(pushes[0].0, 0..10);
        drop(pushes);
    }
}
