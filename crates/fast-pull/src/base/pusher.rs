use crate::ProgressEntry;
use bytes::Bytes;

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
