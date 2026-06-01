use crate::ProgressEntry;
use bytes::Bytes;
use core::fmt::Debug;

/// Abstraction over a data sink that receives pushed byte chunks.
///
/// The pusher writes data to its destination and can optionally flush.
pub trait Pusher: Send + 'static {
    type Error: Send + Unpin + 'static;
    #[allow(clippy::missing_errors_doc)]
    fn push(&mut self, range: &ProgressEntry, content: Bytes) -> Result<(), (Self::Error, Bytes)>;
    #[allow(clippy::missing_errors_doc)]
    fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Marker trait for type-erased error types.
pub trait AnyError: Debug + Send + Unpin + 'static {}
impl<T: Debug + Send + Unpin + 'static> AnyError for T {}

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
impl<P: Pusher> Pusher for PusherAdapter<P>
where
    P::Error: Debug,
{
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
    pub fn new<P: Pusher>(pusher: P) -> Self
    where
        P::Error: Debug,
    {
        Self {
            pusher: Box::new(PusherAdapter { inner: pusher }),
        }
    }
    pub fn upcast<E: AnyError>(e: E) -> Box<dyn AnyError> {
        Box::new(e)
    }
}
