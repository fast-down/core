use crate::{ProgressEntry, Pusher};
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
/// On an inner `push` failure during flush, the unwritten tail is retained in the
/// buffer and `Err((e, Bytes::new()))` is returned — the caller does not need to
/// retry, mirroring the other decorators in this module which keep failed data
/// internally.
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
                // Keep the unwritten tail so it can be retried on the next flush.
                let written = len - rem.len();
                self.buf.extend_from_slice(&rem);
                self.run_start = start + written as u64;
                Err(e)
            }
        }
    }
}

impl<P: Pusher> Pusher for BufWriterPusher<P> {
    type Error = P::Error;

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
}
