use crate::ProgressEntry;
use bytes::Bytes;
use core::future;

pub trait RandPusher: Send + 'static {
    type Error: Send + Unpin + 'static;
    fn push(
        &mut self,
        range: ProgressEntry,
        content: Bytes,
    ) -> impl Future<Output = Result<(), (Self::Error, Bytes)>> + Send;
    fn flush(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        future::ready(Ok(()))
    }
}

pub trait SeqPusher: Send + 'static {
    type Error: Send + Unpin + 'static;
    fn push(
        &mut self,
        content: Bytes,
    ) -> impl Future<Output = Result<(), (Self::Error, Bytes)>> + Send;
    fn flush(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        future::ready(Ok(()))
    }
}
