//! Pusher cache that flushes contiguous runs without byte merging.

use crate::{ProgressEntry, ProgressListener, Pusher};
use bytes::Bytes;
use std::collections::BTreeMap;

use super::util::{rebuffer_tail, runs_longest_first};

/// Pusher wrapper that buffers chunks and flushes large contiguous runs without merging.
///
/// Not used by the built-in file pipeline, which uses [`crate::CacheSeqPusher`].
///
/// Out-of-order chunks are stored in a `BTreeMap`. When a contiguous run reaches
/// the high watermark, it is flushed to the inner pusher as-is (no byte merging).
/// This reduces CPU overhead compared to [`super::CacheMergePusher`] at the cost of
/// more individual write calls.
#[derive(Debug)]
pub struct CacheDirectPusher<P> {
    inner: P,
    cache: BTreeMap<u64, Bytes>,
    cache_size: usize,
    high_watermark: usize,
    low_watermark: usize,
}

impl<P: Pusher> CacheDirectPusher<P> {
    /// Wrap `inner` with the given `high_watermark` / `low_watermark` (in bytes).
    ///
    /// Eviction to the inner pusher triggers once the buffered size reaches
    /// `high_watermark`, and stops once it falls back to `low_watermark`.
    ///
    /// `low_watermark` must not exceed `high_watermark`. A larger `low_watermark`
    /// makes `high_watermark` irrelevant, because eviction does nothing until the
    /// buffered size passes `low_watermark`, which then acts as the sole watermark.
    ///
    /// With `high_watermark == low_watermark`, a push that lands the buffered size
    /// exactly on the watermark evicts nothing; the next push takes it above and
    /// eviction proceeds as usual.
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

        let runs = runs_longest_first(&self.cache);

        for (mut start, mut total_len) in runs {
            while total_len > 0 {
                let chunk = self.cache.remove(&start).unwrap();
                let len = chunk.len();
                self.cache_size -= len;
                total_len -= len;
                let range = start..start + len as u64;
                if let Err((e, ret_bytes)) = self.inner.push(&range, chunk) {
                    rebuffer_tail(&mut self.cache, &mut self.cache_size, start, len, ret_bytes);
                    return Err(e);
                }
                start += len as u64;
            }
            if self.cache_size <= target_size {
                break;
            }
        }
        Ok(())
    }
}

impl<P: Pusher> Pusher for CacheDirectPusher<P> {
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
    use crate::testkit::{PartialSink, RecordingSink, bb};
    use std::sync::atomic::Ordering;

    #[test]
    fn empty_push_is_noop() {
        // a zero-length chunk returns `Ok(())` without buffering.
        let sink = RecordingSink::new();
        let mut p = CacheDirectPusher::new(sink.clone(), 100, 0);
        p.push(&(0..0), Bytes::new()).unwrap();
        assert!(sink.pushes.lock().unwrap().is_empty());
    }

    #[test]
    fn equal_watermarks_skip_only_the_exact_hit() {
        // With high == low, `evict_until` returns early only while `cache_size` is
        // exactly on the watermark. It is not a permanent no-op: the next push takes
        // the buffer above the watermark and eviction runs normally.
        let sink = RecordingSink::new();
        let mut p = CacheDirectPusher::new(sink.clone(), 10, 10);
        p.push(&(0..10), bb(&"A".repeat(10))).unwrap();
        assert!(sink.pushes.lock().unwrap().is_empty());

        p.push(&(10..11), bb("B")).unwrap();
        let ranges: Vec<_> = sink
            .pushes
            .lock()
            .unwrap()
            .iter()
            .map(|(r, _)| r.clone())
            .collect();
        assert_eq!(ranges, vec![0..10, 10..11]);
    }

    #[test]
    fn below_watermark_buffers_until_flush() {
        // No eviction happens while below the high watermark; flush pushes each chunk.
        let sink = RecordingSink::new();
        let mut p = CacheDirectPusher::new(sink.clone(), 100, 10);
        p.push(&(0..10), bb(&"A".repeat(10))).unwrap();
        p.push(&(10..20), bb(&"B".repeat(10))).unwrap();
        assert!(sink.pushes.lock().unwrap().is_empty());
        p.flush().unwrap();
        let pushes = sink.pushes.lock().unwrap();
        assert_eq!(pushes.len(), 2);
        assert_eq!(pushes[0].0, 0..10);
        assert_eq!(pushes[1].0, 10..20);
        drop(pushes);
    }

    #[test]
    fn evicts_longest_run_first() {
        // runs are segmented, sorted longest-first, and each chunk is
        // flushed as-is in ascending offset order within a run.
        let sink = RecordingSink::new();
        let mut p = CacheDirectPusher::new(sink.clone(), 70, 0);
        // runB (length 40)
        p.push(&(100..120), bb(&"B".repeat(20))).unwrap();
        p.push(&(120..140), bb(&"B".repeat(20))).unwrap();
        // runC (length 10)
        p.push(&(200..210), bb(&"C".repeat(10))).unwrap();
        // runA (length 20, split so the trigger crosses the watermark mid-run)
        p.push(&(0..10), bb(&"A".repeat(10))).unwrap();
        p.push(&(10..20), bb(&"A".repeat(10))).unwrap();
        // The final 10 bytes of runA arrive after the eviction.
        p.push(&(20..30), bb(&"A".repeat(10))).unwrap();
        p.flush().unwrap();

        let pushes = sink.pushes.lock().unwrap();
        // Longest run (runB, length 40) must be flushed first.
        assert_eq!(pushes[0].0, 100..120);
        assert_eq!(pushes.len(), 6);
        drop(pushes);
    }

    #[test]
    fn duplicate_start_overwrites_old_bytes() {
        // re-inserting at an already-cached start replaces the old
        // bytes and keeps `cache_size` correct.
        let sink = RecordingSink::new();
        let mut p = CacheDirectPusher::new(sink.clone(), 100, 0);
        p.push(&(0..10), bb(&"A".repeat(10))).unwrap();
        p.push(&(0..10), bb(&"B".repeat(10))).unwrap();
        p.flush().unwrap();
        let pushes = sink.pushes.lock().unwrap();
        assert_eq!(pushes.len(), 1);
        assert_eq!(&pushes[0].1[..], b"BBBBBBBBBB");
        drop(pushes);
    }

    #[test]
    fn inner_push_failure_rebuffers_partial() {
        // when an inner push fails, the un-written tail is re-inserted
        // into the cache and the error is returned to the caller.
        let sink = RecordingSink::new();
        sink.fail_next.store(true, Ordering::SeqCst);
        let mut p = CacheDirectPusher::new(sink, 10, 0);
        let res = p.push(&(0..10), bb(&"A".repeat(10)));
        assert!(res.is_err());
    }

    #[test]
    fn evict_stops_early_at_low_watermark() {
        // once the buffered size drops to <= low_watermark, the
        // eviction loop breaks early and the remaining (separate) run stays buffered.
        // The three chunks are kept as separate runs via gaps.
        let sink = RecordingSink::new();
        let mut p = CacheDirectPusher::new(sink.clone(), 100, 30);
        p.push(&(0..60), bb(&"A".repeat(60))).unwrap();
        p.push(&(100..120), bb(&"B".repeat(20))).unwrap();
        p.push(&(200..220), bb(&"C".repeat(20))).unwrap();

        let pushes = sink.pushes.lock().unwrap();
        // runA is flushed; runB and runC are retained because the cache is already
        // at/below the low watermark after runA.
        assert_eq!(pushes.len(), 2);
        assert_eq!(pushes[0].0, 0..60);
        assert_eq!(pushes[1].0, 100..120);
        drop(pushes);
    }

    #[test]
    fn set_listener_forwards_to_inner() {
        // the listener is forwarded to the inner pusher.
        let sink = RecordingSink::new();
        let mut p = CacheDirectPusher::new(sink.clone(), 100, 0);
        p.set_listener(Box::new(|_| {}));
        assert!(sink.listener_set.load(Ordering::SeqCst));
    }

    #[test]
    fn direct_evict_partial_write_rebuffers_chunk_tail_at_correct_offset() {
        // A run of three 10-byte chunks [0..30) is evicted chunk-by-chunk; a partial
        // inner write on the first chunk (first 2 bytes persisted, tail returned) must
        // re-buffer that chunk's 8-byte tail at offset 2 and retry it on flush.
        let sink = PartialSink::new();
        let mut p = CacheDirectPusher::new(sink.clone(), 30, 0);
        p.push(&(0..10), bb(&"A".repeat(10))).unwrap();
        p.push(&(10..20), bb(&"B".repeat(10))).unwrap();
        let res = p.push(&(20..30), bb(&"C".repeat(10)));
        assert!(res.is_err());
        p.flush().unwrap();
        let pushes = sink.pushes.lock().unwrap();
        // First chunk handed to inner, then its tail retried at [2..10), then the rest.
        assert_eq!(pushes.len(), 4);
        assert_eq!(pushes[0].0, 0..10);
        assert_eq!(pushes[1].0, 2..10);
        assert_eq!(&pushes[1].1[..], b"AAAAAAAA");
        assert_eq!(pushes[2].0, 10..20);
        assert_eq!(pushes[3].0, 20..30);
    }
}
