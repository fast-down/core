extern crate alloc;
use crate::ProgressEntry;
use alloc::boxed::Box;
use bytes::Bytes;
use core::fmt::Debug;

pub trait Pusher: Send + 'static {
    type Error: Send + Unpin + 'static;
    fn push(&mut self, range: &ProgressEntry, content: Bytes) -> Result<(), (Self::Error, Bytes)>;
    fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

pub trait AnyError: Debug + Send + Unpin + 'static {}
impl<T: Debug + Send + Unpin + 'static> AnyError for T {}

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
            .map_err(|(e, b)| (Box::new(e) as Box<dyn AnyError>, b))
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner
            .flush()
            .map_err(|e| Box::new(e) as Box<dyn AnyError>)
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
