use crate::ProgressEntry;
use bytes::Bytes;
use core::fmt::Debug;

pub trait Pusher: Send + 'static {
    type Error: Send + Unpin + 'static;
    #[allow(clippy::missing_errors_doc)]
    fn push(&mut self, range: &ProgressEntry, content: Bytes) -> Result<(), (Self::Error, Bytes)>;
    #[allow(clippy::missing_errors_doc)]
    fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

pub trait AnyError: Debug + Send + Unpin + 'static {}
impl<T: Debug + Send + Unpin + 'static> AnyError for T {}

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
        self.inner.push(range, content).map_err(|(e, b)| {
            let err: Box<dyn AnyError> = Box::new(e);
            (err, b)
        })
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush().map_err(|e| {
            let err: Box<dyn AnyError> = Box::new(e);
            err
        })
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
}
