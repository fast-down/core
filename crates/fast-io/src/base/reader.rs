use crate::ProgressEntry;
use bytes::Bytes;
use futures::TryStream;

pub trait RandReader: Send + Clone {
    type Error: Send;
    fn read(
        &mut self,
        range: &ProgressEntry,
    ) -> impl TryStream<Ok = Bytes, Error = Self::Error> + Send + Unpin;
}

pub trait SeqReader: Send {
    type Error: Send;
    fn read(&mut self) -> impl TryStream<Ok = Bytes, Error = Self::Error> + Send + Unpin;
}
