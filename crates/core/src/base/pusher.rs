use bytes::Bytes;
use std::future;
use std::ops::Range;
use std::task::Poll;

pub trait RandomPusher: Pusher {
    fn push_range(
        &mut self,
        range: Range<u64>,
        content: Bytes,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

pub trait Pusher {
    type Error: Send + 'static;
    fn push(&mut self, content: Bytes) -> impl Future<Output = Result<(), Self::Error>> + Send;
    fn flush(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        future::ready(Ok(()))
    }
}
