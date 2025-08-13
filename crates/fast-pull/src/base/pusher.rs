use crate::ProgressEntry;
use bytes::Bytes;
use core::future;

/// Random Pusher
///
/// Implementation should only overwrite content in `range`, even if `content.len()` is larger than `range.total()`
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
