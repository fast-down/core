//! A `std::io::BufWriter`-style contiguous write buffer for
//! [`Pusher`](crate::Pusher)s.

use crate::{ProgressEntry, ProgressListener, Pusher};
use bytes::{Bytes, BytesMut};

/// Pusher decorator that provides `std::io::BufWriter`-style linear write buffering.
///
/// Unlike the other buffers in this module (`direct`, `merge`, `seq`), which key
/// chunks by `range.start` in a `BTreeMap` to absorb out-of-order writes, this
/// decorator mimics the fixed-size, sequential buffer (backed by `BytesMut`) of
/// `std::io::BufWriter`: it coalesces *contiguous* writes into a single `push` to
/// the inner pusher, flushing when the buffer is full or when an incoming chunk is
/// not contiguous with the buffered run.
///
/// This keeps the whole write chain in the `Pusher` abstraction: any `Pusher` (for
/// example a raw file sink such as `crate::file::StdFilePusher`) can be
/// wrapped to gain syscall / inner-call batching without depending on
/// `std::io::BufWriter`.
///
/// # Position tracking
/// The decorator derives the logical next-write position as `run_start + buf.len()`
/// to detect contiguous vs. seeked writes, exactly like `StdFilePusher::write_at`. On a
/// non-contiguous write, the buffered run is flushed first and a new run is started
/// at the new offset.
///
/// # Error semantics
/// On an inner `push` failure during flush, the buffered run is retained internally
/// for retry and the incoming `bytes` are handed back as `Err((e, bytes))` so the
/// caller can retry them; this mirrors the other decorators in this module, which
/// keep failed data internally rather than dropping it.
#[derive(Debug)]
pub struct BufWriterPusher<P> {
    inner: P,
    buf: BytesMut,
    capacity: usize,
    /// Start offset of the currently buffered contiguous run. The next-write
    /// position is always `run_start + buf.len()` while a run is buffered.
    run_start: u64,
}

impl<P: Pusher> BufWriterPusher<P> {
    /// Build a buffered pusher with the given inner sink and buffer capacity.
    #[must_use]
    pub fn new(inner: P, capacity: usize) -> Self {
        Self {
            inner,
            buf: BytesMut::with_capacity(capacity),
            capacity,
            run_start: 0,
        }
    }

    /// Flush the currently buffered run to the inner pusher, if any.
    ///
    /// On an inner failure the unwritten tail is kept in `self.buf` and `Err(e)`
    /// is returned (data is still held internally for the next flush).
    fn flush_buf(&mut self) -> Result<(), P::Error> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let start = self.run_start;
        let len = self.buf.len();
        // `BytesMut::split()` yields the filled prefix as a `Bytes` (O(1)) and
        // leaves `self.buf` empty but capacity-retained for the next run.
        let chunk: Bytes = self.buf.split().freeze();
        match self.inner.push(&(start..start + len as u64), chunk) {
            Ok(()) => Ok(()),
            Err((e, rem)) => {
                let written = len.saturating_sub(rem.len());
                self.buf.extend_from_slice(&rem);
                self.run_start = start + written as u64;
                Err(e)
            }
        }
    }
}

impl<P: Pusher> Pusher for BufWriterPusher<P> {
    type Error = P::Error;

    fn set_listener(&mut self, cb: ProgressListener) {
        self.inner.set_listener(cb);
    }

    fn push(&mut self, range: &ProgressEntry, bytes: Bytes) -> Result<(), (Self::Error, Bytes)> {
        if bytes.is_empty() {
            return Ok(());
        }

        // Flush if the buffer is non-empty and the incoming chunk is either
        // non-contiguous or would overflow the fixed capacity.
        if !self.buf.is_empty()
            && (range.start != self.run_start + self.buf.len() as u64
                || self.buf.len() + bytes.len() > self.capacity)
            && let Err(e) = self.flush_buf()
        {
            // We still hold the existing buffered run; the caller's bytes were
            // not accepted, so hand them back for retry.
            return Err((e, bytes));
        }

        if self.buf.is_empty() {
            // Start a fresh contiguous run at this offset.
            self.run_start = range.start;
        }

        // Large writes bypass the buffer, matching `BufWriter`'s behaviour.
        if bytes.len() >= self.capacity {
            return self.inner.push(range, bytes);
        }

        self.buf.extend_from_slice(bytes.as_ref());
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.flush_buf()?;
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Shared log of recorded pushes: offset range + bytes.
    type PushLog = Arc<Mutex<Vec<(u64, u64, Vec<u8>)>>>;

    /// Inner pusher that records every `push` it receives.
    #[derive(Clone, Debug, Default)]
    struct RecordingPusher {
        log: PushLog,
    }
    impl Pusher for RecordingPusher {
        type Error = std::io::Error;
        fn push(
            &mut self,
            range: &ProgressEntry,
            bytes: Bytes,
        ) -> Result<(), (Self::Error, Bytes)> {
            self.log
                .lock()
                .unwrap()
                .push((range.start, range.end, bytes.to_vec()));
            Ok(())
        }
    }

    /// Inner pusher that fails the first `push`, then succeeds on every retry.
    #[derive(Clone, Debug)]
    struct FlakyPusher {
        log: PushLog,
        did_fail: Arc<std::sync::atomic::AtomicBool>,
    }
    impl Pusher for FlakyPusher {
        type Error = std::io::Error;
        fn push(
            &mut self,
            range: &ProgressEntry,
            bytes: Bytes,
        ) -> Result<(), (Self::Error, Bytes)> {
            if self.did_fail.load(std::sync::atomic::Ordering::Relaxed) {
                self.log
                    .lock()
                    .unwrap()
                    .push((range.start, range.end, bytes.to_vec()));
                return Ok(());
            }
            self.did_fail
                .store(true, std::sync::atomic::Ordering::Relaxed);
            Err((std::io::Error::other("boom"), bytes))
        }
    }

    /// Inner pusher that writes only the first 2 bytes of the chunk on its first call,
    /// then fails returning the unwritten tail as `rem`. Exercises partial-write handling.
    #[derive(Clone, Debug)]
    struct PartialPusher {
        log: PushLog,
        wrote_partial: Arc<std::sync::atomic::AtomicBool>,
    }
    impl Pusher for PartialPusher {
        type Error = std::io::Error;
        fn push(
            &mut self,
            range: &ProgressEntry,
            bytes: Bytes,
        ) -> Result<(), (Self::Error, Bytes)> {
            self.log
                .lock()
                .unwrap()
                .push((range.start, range.end, bytes.to_vec()));
            if !self
                .wrote_partial
                .swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                // Pretend we persisted the first 2 bytes, then failed with the tail left over.
                let rem = bytes.slice(2..);
                return Err((std::io::Error::other("partial"), rem));
            }
            Ok(())
        }
    }

    /// Inner pusher that records every `set_listener` call and fires the listener on each
    /// successful `push`, so the test can observe whether `BufWriterPusher` forwards it.
    #[derive(Default)]
    struct ListenerRecordingPusher {
        fired: Arc<Mutex<Vec<(u64, u64)>>>,
        listener: Option<ProgressListener>,
    }
    impl Pusher for ListenerRecordingPusher {
        type Error = std::io::Error;
        fn set_listener(&mut self, cb: ProgressListener) {
            self.listener = Some(cb);
        }
        fn push(
            &mut self,
            range: &ProgressEntry,
            _bytes: Bytes,
        ) -> Result<(), (Self::Error, Bytes)> {
            if let Some(cb) = &mut self.listener {
                cb(range.clone());
            }
            self.fired.lock().unwrap().push((range.start, range.end));
            Ok(())
        }
    }

    #[test]
    fn contiguous_writes_coalesce_into_one_push() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut bp = BufWriterPusher::new(RecordingPusher { log: log.clone() }, 1024);
        bp.push(&(0..4), Bytes::from_static(b"abcd")).unwrap();
        bp.push(&(4..8), Bytes::from_static(b"efgh")).unwrap();
        bp.flush().unwrap();

        let l = log.lock().unwrap();
        assert_eq!(l.len(), 1, "expected a single coalesced push");
        assert_eq!(l[0], (0, 8, b"abcdefgh".to_vec()));
        drop(l);
    }

    #[test]
    fn noncontiguous_write_flushes_existing_run() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut bp = BufWriterPusher::new(RecordingPusher { log: log.clone() }, 1024);
        bp.push(&(0..4), Bytes::from_static(b"abcd")).unwrap();
        // Non-contiguous: forces a flush of [0..4) then starts a new run.
        bp.push(&(10..14), Bytes::from_static(b"efgh")).unwrap();
        bp.flush().unwrap();

        let l = log.lock().unwrap();
        assert_eq!(l.len(), 2);
        assert_eq!(l[0], (0, 4, b"abcd".to_vec()));
        assert_eq!(l[1], (10, 14, b"efgh".to_vec()));
        drop(l);
    }

    #[test]
    fn capacity_overflow_flushes() {
        // Capacity 4: the second (contiguous) write overflows and flushes [0..4).
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut bp = BufWriterPusher::new(RecordingPusher { log: log.clone() }, 4);
        bp.push(&(0..4), Bytes::from_static(b"abcd")).unwrap();
        bp.push(&(4..8), Bytes::from_static(b"efgh")).unwrap();
        bp.flush().unwrap();

        let l = log.lock().unwrap();
        assert_eq!(l.len(), 2);
        assert_eq!(l[0], (0, 4, b"abcd".to_vec()));
        assert_eq!(l[1], (4, 8, b"efgh".to_vec()));
        drop(l);
    }

    #[test]
    fn large_write_bypasses_buffer() {
        let log = Arc::new(Mutex::new(Vec::new()));
        // Capacity 4, but write 8 bytes directly -> should bypass buffering.
        let mut bp = BufWriterPusher::new(RecordingPusher { log: log.clone() }, 4);
        bp.push(&(0..8), Bytes::from_static(b"abcdefgh")).unwrap();
        bp.flush().unwrap();

        let l = log.lock().unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0], (0, 8, b"abcdefgh".to_vec()));
        drop(l);
    }

    #[test]
    #[cfg(feature = "mem")]
    fn random_access_write_is_correct_with_mem_pusher() {
        let mem = crate::mem::MemPusher::with_capacity(16);
        let mut bp = BufWriterPusher::new(mem, 8 * 1024);
        bp.push(&(2..5), Bytes::from_static(b"234")).unwrap();
        bp.flush().unwrap();

        let content = bp.inner.receive.lock().clone();
        // `MemPusher` grows rather than pre-sizing, so a write at [2..5) yields 5 bytes.
        assert_eq!(content, b"\0\x00234");
    }

    #[test]
    fn failed_inner_push_is_retained_and_retried() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut bp = BufWriterPusher::new(
            FlakyPusher {
                log: log.clone(),
                did_fail: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
            1024,
        );

        bp.push(&(0..4), Bytes::from_static(b"abcd")).unwrap();
        // First flush hits the failing inner push; data is retained internally.
        assert!(bp.flush().is_err());
        // Next flush retries and succeeds.
        bp.flush().unwrap();

        let l = log.lock().unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0], (0, 4, b"abcd".to_vec()));
        drop(l);
    }

    #[test]
    fn empty_push_is_a_noop() {
        // Line 89: a zero-length chunk returns `Ok(())` without touching the buffer.
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut bp = BufWriterPusher::new(RecordingPusher { log: log.clone() }, 1024);
        bp.push(&(0..0), Bytes::new()).unwrap();
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn flush_failure_during_push_returns_caller_bytes() {
        // Lines 97-101: a non-contiguous write while the buffer is non-empty triggers
        // an inner flush; when that flush fails, the caller's bytes are handed back.
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut bp = BufWriterPusher::new(
            FlakyPusher {
                log,
                did_fail: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
            1024,
        );
        // Buffered run [0..4); the FlakyPusher fails its first inner push.
        bp.push(&(0..4), Bytes::from_static(b"abcd")).unwrap();
        // Non-contiguous write forces a flush of [0..4), which fails; the incoming
        // bytes [10..14) are returned to the caller.
        let res = bp.push(&(10..14), Bytes::from_static(b"efgh"));
        assert!(res.is_err());
        let (_e, remaining) = res.unwrap_err();
        assert_eq!(&remaining[..], b"efgh");
    }

    #[test]
    fn flush_buf_partial_write_is_retained_and_retried() {
        // A partial inner write (first 2 of 4 bytes persisted, then failure with the
        // 2-byte tail as `rem`) must leave exactly that tail buffered for retry.
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut bp = BufWriterPusher::new(
            PartialPusher {
                log: log.clone(),
                wrote_partial: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
            1024,
        );
        bp.push(&(0..4), Bytes::from_static(b"abcd")).unwrap();
        // First flush hits the partial failure; the 2-byte tail stays in `self.buf`.
        assert!(bp.flush().is_err());
        // Retry flushes only the retained tail.
        bp.flush().unwrap();

        let l = log.lock().unwrap();
        // The inner saw the full 4-byte chunk first, then the 2-byte tail on retry.
        assert_eq!(l.len(), 2);
        assert_eq!(l[0], (0, 4, b"abcd".to_vec()));
        assert_eq!(l[1], (2, 4, b"cd".to_vec()));
    }

    #[test]
    fn buf_writer_forwards_set_listener_and_fires_on_flush() {
        let sink = ListenerRecordingPusher::default();
        let fired = sink.fired.clone();
        let mut bp = BufWriterPusher::new(sink, 1024);
        bp.set_listener(Box::new(|_r: ProgressEntry| {}));
        bp.push(&(0..4), Bytes::from_static(b"abcd")).unwrap();
        // A buffered write has not reached the inner sink yet, so no listener fired.
        assert!(
            fired.lock().unwrap().is_empty(),
            "buffered write must not reach the inner listener before flush"
        );
        bp.flush().unwrap();
        // On flush the coalesced range reaches the inner sink and its listener fires.
        assert_eq!(fired.lock().unwrap().as_slice(), &[(0, 4)]);
    }

    #[test]
    fn capacity_full_does_not_flush_prematurely() {
        // Capacity 4: two contiguous 2-byte writes fill the buffer exactly. Reaching
        // `== capacity` must NOT trigger a flush; only `> capacity` does.
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut bp = BufWriterPusher::new(RecordingPusher { log: log.clone() }, 4);
        bp.push(&(0..2), Bytes::from_static(b"ab")).unwrap();
        bp.push(&(2..4), Bytes::from_static(b"cd")).unwrap();
        assert!(
            log.lock().unwrap().is_empty(),
            "reaching exactly capacity must not flush"
        );
        bp.flush().unwrap();
        let l = log.lock().unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0], (0, 4, b"abcd".to_vec()));
    }

    #[test]
    fn failed_noncontiguous_flush_keeps_old_run_for_retry() {
        // A non-contiguous write forces a flush of the buffered [0..4) run; when that
        // flush fails, the caller's bytes are returned AND the old run is kept so it can
        // be retried on the next flush.
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut bp = BufWriterPusher::new(
            FlakyPusher {
                log: log.clone(),
                did_fail: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
            1024,
        );
        bp.push(&(0..4), Bytes::from_static(b"abcd")).unwrap();
        let res = bp.push(&(10..14), Bytes::from_static(b"efgh"));
        assert!(res.is_err());
        // The old run must still be retryable.
        bp.flush().unwrap();
        let l = log.lock().unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0], (0, 4, b"abcd".to_vec()));
    }
}
