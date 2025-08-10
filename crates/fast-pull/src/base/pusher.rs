use crate::ProgressEntry;
use bytes::Bytes;
use core::future;

pub trait RandPusher: Send {
    type Error: Send;
    fn push(
        &mut self,
        range: ProgressEntry,
        content: Bytes,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
    fn end(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        future::ready(Ok(()))
    }
}

pub trait Pusher: Send {
    type Error: Send;
    fn push(&mut self, content: Bytes) -> impl Future<Output = Result<(), Self::Error>> + Send;
    fn end(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        future::ready(Ok(()))
    }
}
