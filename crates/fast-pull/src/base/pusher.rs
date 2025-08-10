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
    fn flush(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        future::ready(Ok(()))
    }
}

pub trait SeqPusher: Send {
    type Error: Send;
    fn push(&mut self, content: Bytes) -> impl Future<Output = Result<(), Self::Error>> + Send;
    fn flush(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        future::ready(Ok(()))
    }
}
