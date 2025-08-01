use crate::WorkerId;
use crate::base::pusher::{Pusher, RandomPusher};
use crate::base::source::{Fetcher, Puller};
use bytes::Bytes;
use std::ops::Range;
use std::sync::Arc;

pub(crate) fn build_mock_data(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 256) as u8).collect()
}

pub(crate) struct MockFetcher(pub(crate) Vec<u8>);
pub(crate) struct MockPuller(pub(crate) Option<Bytes>);
pub(crate) struct MockPusher(pub(crate) Arc<std::sync::Mutex<Vec<u8>>>);
impl Puller for MockPuller {
    type Error = ();

    async fn pull(&mut self) -> Result<Option<Bytes>, Self::Error> {
        Ok(self.0.take())
    }
}
impl Fetcher for MockFetcher {
    type Error = ();
    type Puller = MockPuller;

    async fn fetch(
        &self,
        _id: WorkerId,
        range: Option<&Range<u64>>,
    ) -> Result<Self::Puller, Self::Error> {
        Ok(match range {
            Some(range) => match self.0.get((range.start as usize)..(range.end as usize)) {
                None => MockPuller(None),
                Some(bytes) => MockPuller(Some(Bytes::copy_from_slice(bytes))),
            },
            None => MockPuller(Some(Bytes::copy_from_slice(&self.0))),
        })
    }

    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl Pusher for MockPusher {
    type Error = ();

    async fn push(&mut self, content: Bytes) -> Result<(), Self::Error> {
        self.0.lock().unwrap().extend_from_slice(&content);
        Ok(())
    }
}
impl RandomPusher for MockPusher {
    async fn push_range(&mut self, range: Range<u64>, content: Bytes) -> Result<(), Self::Error> {
        self.0.lock().unwrap()[range.start as usize..range.end as usize].copy_from_slice(&content);
        Ok(())
    }
}
