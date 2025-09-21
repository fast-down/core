extern crate alloc;
use core::time::Duration;

use crate::{ProgressEntry, RandPuller, SeqPuller};
use alloc::{sync::Arc, vec::Vec};
use bytes::Bytes;
use futures::{TryStream, stream};

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
    fn pull(
        &mut self,
        range: &ProgressEntry,
    ) -> impl TryStream<Ok = Bytes, Error = (Self::Error, Option<Duration>)> + Send + Unpin {
        let data = &self.0[range.start as usize..range.end as usize];
        stream::iter(data.iter().map(|e| Ok(Bytes::from_iter([*e]))))
    }
}
impl SeqPuller for MockPuller {
    type Error = ();
    fn pull(
        &mut self,
    ) -> impl TryStream<Ok = Bytes, Error = (Self::Error, Option<Duration>)> + Send + Unpin {
        stream::iter(self.0.iter().map(|e| Ok(Bytes::from_iter([*e]))))
    }
}
