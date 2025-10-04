extern crate alloc;
use crate::{ProgressEntry, PullResult, PullStream, RandPuller, SeqPuller};
use alloc::{sync::Arc, vec::Vec};
use bytes::Bytes;
use futures::stream;

pub fn build_mock_data(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 256) as u8).collect()
}

#[derive(Clone)]
pub struct MockPuller(pub Arc<[u8]>);
impl MockPuller {
    pub fn new(data: &[u8]) -> Self {
        Self(Arc::from(data))
    }
}
impl RandPuller for MockPuller {
    type Error = ();
    async fn pull(
        &mut self,
        range: &ProgressEntry,
    ) -> PullResult<Self::Error, impl PullStream<Self::Error>> {
        let data = &self.0[range.start as usize..range.end as usize];
        Ok(stream::iter(
            data.iter().map(|e| Ok(Bytes::from_iter([*e]))),
        ))
    }
}
impl SeqPuller for MockPuller {
    type Error = ();
    async fn pull(&mut self) -> PullResult<Self::Error, impl PullStream<Self::Error>> {
        Ok(stream::iter(
            self.0.iter().map(|e| Ok(Bytes::from_iter([*e]))),
        ))
    }
}
