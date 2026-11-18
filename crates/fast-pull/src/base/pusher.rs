//! The [`Pusher`](crate::Pusher) trait: an abstraction over a chunked data sink.

use crate::ProgressEntry;
use bytes::Bytes;

/// A callback invoked whenever a chunk has been successfully written to its
/// destination (disk / memory).
///
/// Leaf sinks (`StdFilePusher`, `MmapFilePusher`, `MemPusher`) store this and
/// call it from their [`Pusher::push`] success path. The alias exists so the
/// `Box<dyn Fn...>` parameter does not trip `clippy::type_complexity`.
pub type ProgressListener = Box<dyn FnMut(ProgressEntry) + Send + 'static>;

/// Abstraction over a data sink that receives pushed byte chunks.
///
/// The pusher writes data to its destination and can optionally flush.
pub trait Pusher: Send + 'static {
    /// Error produced while pushing.
    type Error: std::error::Error + Send + Sync + Unpin + 'static;
    /// Write `content` covering the given `range` to the destination.
    ///
    /// Returning `Ok(())` means the sink took responsibility for `content`; a
    /// buffering decorator may still hold it in memory until its next `flush`,
    /// provided it guarantees that flush writes it. On failure returns
    /// `Err((error, bytes))` where `bytes` is the part of `content` the caller
    /// still owns: either the bytes to retry, or empty when the decorator kept
    /// them internally for a later flush. A decorator that returns empty must
    /// therefore never drop them silently.
    ///
    /// # What a partial result promises
    ///
    /// The engine never counts written bytes itself. On failure it derives them
    /// as `content.len() - bytes.len()`, adds that to `range.start`, and pushes
    /// the returned `bytes` again at the shifted offset, so an implementation
    /// must satisfy all of:
    ///
    /// 1. `bytes` is a contiguous **suffix** of `content`; the leading
    ///    `content.len() - bytes.len()` bytes are taken as written.
    /// 2. The destination really holds that prefix at `range.start` before the
    ///    call returns, because the retry resumes at `range.start + written` and
    ///    the engine will never write that prefix again.
    /// 3. An **empty** `bytes` means "the whole chunk was accepted". For a
    ///    decorator that is the promise that its next `flush` writes the data it
    ///    kept; for the engine it is indistinguishable from success, so the
    ///    range is recorded as written and the session moves on.
    ///
    /// Requirement 3 is the sharp edge behind "never drop them silently": a
    /// failure reported with empty `bytes` is counted as success even if the
    /// decorator actually dropped the data. None of the three requirements can
    /// be checked by the engine — breaking one shows up as missing or duplicated
    /// bytes in the destination, not as an error.
    #[allow(clippy::missing_errors_doc)]
    fn push(&mut self, range: &ProgressEntry, content: Bytes) -> Result<(), (Self::Error, Bytes)>;
    /// Flush any buffered data to the destination.
    ///
    /// The default implementation is a no-op. File-backed pushers use this to
    /// issue `fsync` / `flush` on the underlying file.
    #[allow(clippy::missing_errors_doc)]
    fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
    /// Install a callback that fires whenever a chunk has been successfully
    /// pushed to its destination.
    ///
    /// The default implementation is a no-op. Leaf sinks override this to store
    /// the callback; every wrapper **must** override this to forward it to its
    /// inner pusher, otherwise progress events are silently dropped.
    #[allow(clippy::needless_pass_by_value)]
    #[allow(unused_variables)]
    fn set_listener(&mut self, cb: ProgressListener) {}
}

/// Marker trait for type-erased error types.
pub trait AnyError: std::error::Error + Send + Sync + Unpin + 'static {}
impl<T: std::error::Error + Send + Sync + Unpin + 'static> AnyError for T {}

impl std::error::Error for Box<dyn AnyError> {}

/// A type-erased pusher that boxes both the pusher and its error type.
///
/// Useful for FFI boundaries or heterogeneous collections of pushers.
#[allow(missing_debug_implementations)]
pub struct BoxPusher {
    /// The boxed, type-erased inner pusher.
    pub pusher: Box<dyn Pusher<Error = Box<dyn AnyError>>>,
}
impl Pusher for BoxPusher {
    type Error = Box<dyn AnyError>;
    fn set_listener(&mut self, cb: ProgressListener) {
        self.pusher.set_listener(cb);
    }
    fn push(&mut self, range: &ProgressEntry, content: Bytes) -> Result<(), (Self::Error, Bytes)> {
        self.pusher.push(range, content)
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        self.pusher.flush()
    }
}

struct PusherAdapter<P: Pusher> {
    inner: P,
}
impl<P: Pusher> Pusher for PusherAdapter<P> {
    type Error = Box<dyn AnyError>;
    fn set_listener(&mut self, cb: ProgressListener) {
        self.inner.set_listener(cb);
    }
    fn push(&mut self, range: &ProgressEntry, content: Bytes) -> Result<(), (Self::Error, Bytes)> {
        self.inner
            .push(range, content)
            .map_err(|(e, b)| (BoxPusher::upcast(e), b))
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush().map_err(|e| BoxPusher::upcast(e))
    }
}

impl BoxPusher {
    /// Box a concrete [`Pusher`], erasing its error type.
    pub fn new<P: Pusher>(pusher: P) -> Self {
        Self {
            pusher: Box::new(PusherAdapter { inner: pusher }),
        }
    }
    /// Box an error into the type-erased `AnyError` form used by [`BoxPusher`].
    pub fn upcast<E: AnyError>(e: E) -> Box<dyn AnyError> {
        Box::new(e)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// A `Pusher` that does not override the default `set_listener` / `flush`.
    struct DummyPusher;
    impl Pusher for DummyPusher {
        type Error = std::io::Error;
        fn push(
            &mut self,
            _range: &ProgressEntry,
            _content: Bytes,
        ) -> Result<(), (Self::Error, Bytes)> {
            Ok(())
        }
    }

    #[test]
    fn default_set_listener_is_noop() {
        // Exercises the default `Pusher::set_listener` body and the
        // default-`flush` `DummyPusher::push` path.
        let mut p = DummyPusher;
        p.set_listener(Box::new(|_| {}));
        p.push(&(0..0), Bytes::new()).unwrap();
    }

    #[test]
    fn upcast_boxes_any_error() {
        // Exercises `BoxPusher::upcast`.
        let boxed: Box<dyn AnyError> = BoxPusher::upcast(std::io::Error::other("boom"));
        let _ = boxed;
    }

    /// Records every push and listener install; can be told to fail the next
    /// `push`/`flush` so the `BoxPusher` (and its `PusherAdapter`) error paths
    /// are exercised.
    #[derive(Clone)]
    struct RecordingPusher {
        pushes: Arc<Mutex<Vec<(ProgressEntry, Bytes)>>>,
        fail_push: Arc<AtomicBool>,
        fail_flush: Arc<AtomicBool>,
        listener_set: Arc<AtomicBool>,
        listener: Arc<Mutex<Option<ProgressListener>>>,
    }
    impl RecordingPusher {
        fn new() -> Self {
            Self {
                pushes: Arc::new(Mutex::new(Vec::new())),
                fail_push: Arc::new(AtomicBool::new(false)),
                fail_flush: Arc::new(AtomicBool::new(false)),
                listener_set: Arc::new(AtomicBool::new(false)),
                listener: Arc::new(Mutex::new(None)),
            }
        }
    }
    impl Pusher for RecordingPusher {
        type Error = std::io::Error;
        fn set_listener(&mut self, cb: ProgressListener) {
            self.listener_set.store(true, Ordering::SeqCst);
            *self.listener.lock().unwrap() = Some(cb);
        }
        fn push(
            &mut self,
            range: &ProgressEntry,
            bytes: Bytes,
        ) -> Result<(), (Self::Error, Bytes)> {
            if self.fail_push.swap(false, Ordering::SeqCst) {
                return Err((std::io::Error::other("push"), bytes));
            }
            if let Some(cb) = self.listener.lock().unwrap().as_mut() {
                cb(range.clone());
            }
            self.pushes.lock().unwrap().push((range.clone(), bytes));
            Ok(())
        }
        fn flush(&mut self) -> Result<(), Self::Error> {
            if self.fail_flush.swap(false, Ordering::SeqCst) {
                Err(std::io::Error::other("flush"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn box_pusher_forwards_push_flush_and_listener() {
        // Covers `BoxPusher`/`PusherAdapter` success forwarding.
        let inner = RecordingPusher::new();
        let mut bp = BoxPusher::new(inner.clone());
        bp.set_listener(Box::new(|_| {}));
        assert!(inner.listener_set.load(Ordering::SeqCst));
        bp.push(&(0..3), Bytes::copy_from_slice(b"abc")).unwrap();
        bp.flush().unwrap();
        let pushes = inner.pushes.lock().unwrap();
        assert_eq!(pushes.len(), 1);
        assert_eq!(pushes[0].0, 0..3);
        drop(pushes);
    }

    #[test]
    fn box_pusher_upcasts_push_error() {
        // Covers `PusherAdapter::push`'s `map_err`/upcast path.
        let inner = RecordingPusher::new();
        inner.fail_push.store(true, Ordering::SeqCst);
        let mut bp = BoxPusher::new(inner);
        let res = bp.push(&(0..3), Bytes::copy_from_slice(b"abc"));
        assert!(res.is_err());
        let _ = res.unwrap_err();
    }

    #[test]
    fn box_pusher_upcasts_flush_error() {
        // Covers `PusherAdapter::flush`'s `map_err`/upcast path.
        let inner = RecordingPusher::new();
        inner.fail_flush.store(true, Ordering::SeqCst);
        let mut bp = BoxPusher::new(inner);
        assert!(bp.flush().is_err());
    }

    #[test]
    fn box_pusher_listener_fires_on_successful_push() {
        // End-to-end progress path: the listener is forwarded down to the leaf
        // sink, and firing it is tied to a successful write, with the correct
        // range reported.
        let inner = RecordingPusher::new();
        let seen = Arc::new(Mutex::new(Vec::<ProgressEntry>::new()));
        let seen2 = seen.clone();
        let mut bp = BoxPusher::new(inner);
        bp.set_listener(Box::new(move |r| seen2.lock().unwrap().push(r)));
        bp.push(&(0..3), Bytes::copy_from_slice(b"abc")).unwrap();
        let s = seen.lock().unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0], 0..3);
    }

    #[test]
    fn box_pusher_push_error_preserves_unwritten_bytes() {
        // On failure the unwritten bytes must survive `PusherAdapter`'s `map_err`
        // untouched; swallowing them would leave the engine unable to retry that
        // chunk, silently losing data.
        let inner = RecordingPusher::new();
        inner.fail_push.store(true, Ordering::SeqCst);
        let mut bp = BoxPusher::new(inner);
        let payload = Bytes::copy_from_slice(b"hello");
        let res = bp.push(&(0..5), payload.clone());
        let (_, unwritten) = res.unwrap_err();
        assert_eq!(unwritten, payload);
    }

    #[test]
    fn box_pusher_default_flush_is_ok() {
        // `DummyPusher` does not override `flush`, so the default no-op runs and
        // must still succeed after being forwarded through `BoxPusher`.
        let mut bp = BoxPusher::new(DummyPusher);
        assert!(bp.flush().is_ok());
    }
}
