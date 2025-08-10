use crate::ProgressEntry;
use bytes::Bytes;
use futures::TryStream;

pub trait RandPuller: Send + Clone {
    type Error: Send;
    fn pull(
        &mut self,
        range: &ProgressEntry,
    ) -> impl TryStream<Ok = Bytes, Error = Self::Error> + Send + Unpin;
}

pub trait SeqPuller: Send {
    type Error: Send;
    fn pull(&mut self) -> impl TryStream<Ok = Bytes, Error = Self::Error> + Send + Unpin;
}
