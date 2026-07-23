use crate::ProgressEntry;
use bytes::Bytes;

/// A callback invoked whenever a chunk has been successfully written to its
/// destination (disk / memory).
///
/// Leaf sinks (`StdFilePusher`, `MmapFilePusher`, `MemPusher`) store this and
/// call it from their [`Pusher::push`] success path. The alias exists so the
/// `Box<dyn Fn ...>` parameter does not trip `clippy::type_complexity`.
pub type ProgressListener = Box<dyn FnMut(ProgressEntry) + Send + 'static>;

/// Abstraction over a data sink that receives pushed byte chunks.
///
/// The pusher writes data to its destination and can optionally flush.
pub trait Pusher: Send + 'static {
    type Error: std::error::Error + Send + Sync + Unpin + 'static;
    #[allow(clippy::missing_errors_doc)]
    fn push(&mut self, range: &ProgressEntry, content: Bytes) -> Result<(), (Self::Error, Bytes)>;
    #[allow(clippy::missing_errors_doc)]
    fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
    /// Install a callback that fires whenever a chunk has been successfully
    /// pushed to its destination.
    ///
    /// The default implementation is a no-op. Leaf sinks override this to store
    /// the callback; every wrapper overrides this to forward it to its inner
    /// pusher. Omitting the forwarding in any wrapper silently drops progress.
    #[allow(clippy::needless_pass_by_value)]
    fn set_listener(&mut self, _cb: ProgressListener) {}
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
    pub fn new<P: Pusher>(pusher: P) -> Self {
        Self {
            pusher: Box::new(PusherAdapter { inner: pusher }),
        }
    }
    pub fn upcast<E: AnyError>(e: E) -> Box<dyn AnyError> {
        Box::new(e)
    }
}
