use crate::ProgressEntry;
use bytes::Bytes;
use core::future;

pub trait RandWriter: Send {
    type Error: Send;
    fn write(
        &mut self,
        range: ProgressEntry,
        content: Bytes,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
    fn flush(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        future::ready(Ok(()))
    }
}

pub trait SeqWriter: Send {
    type Error: Send;
    fn write(&mut self, content: Bytes) -> impl Future<Output = Result<(), Self::Error>> + Send;
    fn flush(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        future::ready(Ok(()))
    }
}
