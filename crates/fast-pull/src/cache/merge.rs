//! Pusher cache that merges each flush run into a single contiguous buffer.

use crate::{ProgressEntry, ProgressListener, Pusher};
use bytes::{Bytes, BytesMut};
use std::collections::{BTreeMap, btree_map::Entry};

/// Pusher wrapper that buffers chunks and merges each flush run into a single [`Bytes`].
///
/// Out-of-order chunks are stored in a `BTreeMap`. When a contiguous run reaches
/// the high watermark, all chunks in that run are coalesced into one contiguous
/// [`BytesMut`] before being pushed. This minimizes write calls to the inner pusher
/// at the cost of an extra memory copy.
#[derive(Debug)]
pub struct CacheMergePusher<P> {
    inner: P,
    cache: BTreeMap<u64, Bytes>,
    cache_size: usize,
    high_watermark: usize,
    low_watermark: usize,
}

impl<P: Pusher> CacheMergePusher<P> {
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
        if self.cache_size <= target_size {
            return Ok(());
        }

        let mut runs: Vec<(u64, usize)> = Vec::with_capacity(self.cache.len());
        let mut curr_start = None;
        let mut curr_len = 0;
        let mut expected_next = 0;

        for (&start, bytes) in &self.cache {
            let len = bytes.len();
            if let Some(c_start) = curr_start {
                if start == expected_next {
                    curr_len += len;
                    expected_next += len as u64;
                } else {
                    runs.push((c_start, curr_len));
                    curr_start = Some(start);
                    curr_len = len;
                    expected_next = start + len as u64;
                }
            } else {
                curr_start = Some(start);
                curr_len = len;
                expected_next = start + len as u64;
            }
        }
        if let Some(c_start) = curr_start {
            runs.push((c_start, curr_len));
        }
        runs.sort_unstable_by_key(|&(_, len)| std::cmp::Reverse(len));

        let mut curr_buf = BytesMut::with_capacity(self.cache_size);
        let mut err = None;
        for (start, total_len) in runs {
            let need_push = err.is_none() && self.cache_size > target_size;
            let first_bytes = match self.cache.entry(start) {
                Entry::Occupied(entry) => {
                    let is_merged = entry.get().len() == total_len;
                    if !need_push && is_merged {
                        continue;
                    }
                    entry.remove()
                }
                Entry::Vacant(_) => unreachable!(),
            };
            let chunk = if first_bytes.len() == total_len {
                first_bytes
            } else {
                curr_buf.extend_from_slice(&first_bytes);
                let mut curr_key = start + first_bytes.len() as u64;
                let end = start + total_len as u64;
                while curr_key < end {
                    let bytes = self.cache.remove(&curr_key).unwrap();
                    curr_buf.extend_from_slice(&bytes);
                    curr_key += bytes.len() as u64;
                }
                curr_buf.split().freeze()
            };
            if need_push {
                let end = start + total_len as u64;
                let range = start..end;
                self.cache_size -= total_len;
                if let Err((e, ret_bytes)) = self.inner.push(&range, chunk) {
                    err = Some(e);
                    if !ret_bytes.is_empty() {
                        self.cache_size += ret_bytes.len();
                        let retry_start = start + (total_len - ret_bytes.len()) as u64;
                        self.cache.insert(retry_start, ret_bytes);
                    }
                }
            } else {
                self.cache.insert(start, chunk);
            }
        }
        err.map_or(Ok(()), Err)
    }
}

impl<P: Pusher> Pusher for CacheMergePusher<P> {
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

    /// A `Pusher` that records everything pushed into a shared buffer so tests
    /// can inspect it without reaching into `CacheMergePusher`'s private fields.
    #[derive(Clone)]
    struct SharedSink {
        pushes: Arc<Mutex<Vec<(ProgressEntry, Bytes)>>>,
        fail_next: Arc<AtomicBool>,
    }
    impl SharedSink {
        fn new() -> Self {
            Self {
                pushes: Arc::new(Mutex::new(Vec::new())),
                fail_next: Arc::new(AtomicBool::new(false)),
            }
        }
    }
    impl Pusher for SharedSink {
        type Error = std::io::Error;
        fn set_listener(&mut self, _: ProgressListener) {}
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
    fn test_cache_merge_evicts_contiguous_run() {
        let sink = SharedSink::new();
        let mut p = CacheMergePusher::new(sink.clone(), 30, 0);
        // Out-of-order insertion; not yet at watermark.
        p.push(&(0..10), bb(&"A".repeat(10))).unwrap();
        p.push(&(20..30), bb(&"C".repeat(10))).unwrap();
        assert!(sink.pushes.lock().unwrap().is_empty());
        // This insertion reaches the high watermark and triggers a merge+evict.
        p.push(&(10..20), bb(&"B".repeat(10))).unwrap();
        let pushes = sink.pushes.lock().unwrap();
        assert_eq!(pushes.len(), 1);
        assert_eq!(pushes[0].0, 0..30);
        assert_eq!(pushes[0].1.len(), 30);
        // Merged bytes preserve ascending order: A(0..10) B(10..20) C(20..30).
        assert_eq!(&pushes[0].1[..], b"AAAAAAAAAABBBBBBBBBBCCCCCCCCCC");
        drop(pushes);
    }

    #[test]
    fn test_cache_merge_no_evict_below_watermark() {
        let sink = SharedSink::new();
        let mut p = CacheMergePusher::new(sink.clone(), 100, 0);
        p.push(&(0..10), bb(&"A".repeat(10))).unwrap();
        p.push(&(10..20), bb(&"B".repeat(10))).unwrap();
        assert!(sink.pushes.lock().unwrap().is_empty());
        // Explicit flush must drain the buffered run to the inner pusher.
        p.flush().unwrap();
        let pushes = sink.pushes.lock().unwrap();
        assert_eq!(pushes.len(), 1);
        assert_eq!(pushes[0].0, 0..20);
        drop(pushes);
    }

    #[test]
    fn test_cache_merge_inner_failure_propagates() {
        let sink = SharedSink::new();
        sink.fail_next.store(true, Ordering::SeqCst);
        let mut p = CacheMergePusher::new(sink, 10, 0);
        let res = p.push(&(0..10), bb(&"A".repeat(10)));
        assert!(res.is_err());
    }

    #[test]
    fn test_cache_merge_empty_bytes_is_noop() {
        let sink = SharedSink::new();
        let mut p = CacheMergePusher::new(sink.clone(), 1, 0);
        p.push(&(0..0), Bytes::new()).unwrap();
        assert!(sink.pushes.lock().unwrap().is_empty());
    }

    #[test]
    fn test_cache_merge_partial_overwrite_replaces() {
        // A chunk at an already-cached start position replaces the old bytes.
        let sink = SharedSink::new();
        let mut p = CacheMergePusher::new(sink.clone(), 100, 0);
        p.push(&(0..10), bb(&"A".repeat(10))).unwrap();
        p.push(&(0..10), bb(&"B".repeat(10))).unwrap();
        p.flush().unwrap();
        let pushes = sink.pushes.lock().unwrap();
        assert_eq!(pushes.len(), 1);
        assert_eq!(&pushes[0].1[..], b"BBBBBBBBBB");
        drop(pushes);
    }
}
