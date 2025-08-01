use crate::WorkerId;
use bytes::Bytes;
use std::ops::Range;

pub trait Puller: Send {
    type Error: Send + 'static;
    fn pull(&mut self) -> impl Future<Output = Result<Option<Bytes>, Self::Error>> + Send;
}

pub trait Fetcher: Send {
    type Error: Send + 'static;
    type Puller: Puller;
    fn fetch(
        &self,
        id: WorkerId,
        range: Option<&Range<u64>>,
    ) -> impl Future<Output = Result<Self::Puller, Self::Error>> + Send;
    fn clone(&self) -> Self;
}
