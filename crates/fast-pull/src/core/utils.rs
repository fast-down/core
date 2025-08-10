extern crate alloc;
use crate::{ProgressEntry, Puller, Pusher, RandPuller, RandPusher};
use alloc::vec;
use alloc::{sync::Arc, vec::Vec};
use bytes::Bytes;
use futures::{TryStream, stream};
use tokio::sync::Mutex;

pub fn build_mock_data(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 256) as u8).collect()
}

#[derive(Clone)]
pub struct StaticPuller(pub Arc<[u8]>);
impl StaticPuller {
    pub fn new(data: &[u8]) -> Self {
        Self(Arc::from(data))
    }
}
impl RandPuller for StaticPuller {
    type Error = core::convert::Infallible;
    fn pull(
        &mut self,
        range: &ProgressEntry,
    ) -> impl TryStream<Ok = Bytes, Error = Self::Error> + Send + Unpin {
        let data = &self.0[range.start as usize..range.end as usize];
        stream::iter(data.iter().map(|e| Ok(Bytes::from_iter([*e]))))
    }
}

impl Puller for StaticPuller {
    type Error = core::convert::Infallible;
    fn pull(&mut self) -> impl TryStream<Ok = Bytes, Error = Self::Error> + Send + Unpin {
        stream::iter(self.0.iter().map(|e| Ok(Bytes::from_iter([*e]))))
    }
}

#[derive(Clone)]
pub struct FixedPusher {
    pub receive: Arc<Mutex<Vec<u8>>>,
}
impl FixedPusher {
    pub fn new(size: usize) -> Self {
        Self {
            receive: Arc::new(Mutex::new(vec![0; size])),
        }
    }
    pub async fn assert_eq(&self, other: &[u8]) {
        let receive = self.receive.lock().await;
        assert_eq!(&receive[..], other);
    }
}
impl RandPusher for FixedPusher {
    type Error = core::convert::Infallible;
    async fn push(&mut self, range: ProgressEntry, content: Bytes) -> Result<(), Self::Error> {
        self.receive.lock().await[range.start as usize..range.end as usize]
            .copy_from_slice(&content);
        Ok(())
    }
}

#[derive(Clone)]
pub struct FixedSeqPusher {
    pub receive: Arc<Mutex<Vec<u8>>>,
}
impl FixedSeqPusher {
    pub fn new() -> Self {
        Self {
            receive: Arc::new(Mutex::new(vec![])),
        }
    }
    pub async fn assert_eq(&self, other: &[u8]) {
        let receive = self.receive.lock().await;
        assert_eq!(&receive[..], other);
    }
}
impl Pusher for FixedSeqPusher {
    type Error = core::convert::Infallible;
    async fn push(&mut self, content: Bytes) -> Result<(), Self::Error> {
        self.receive.lock().await.extend_from_slice(&content);
        Ok(())
    }
}
