extern crate alloc;
use crate::{ProgressEntry, RandPuller, RandPusher, SeqPuller, SeqPusher};
use alloc::vec;
use alloc::{sync::Arc, vec::Vec};
use bytes::Bytes;
use futures::{TryStream, stream};
use tokio::sync::Mutex;

pub fn build_mock_data(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 256) as u8).collect()
}

#[derive(Clone)]
pub struct MockRandPuller(pub Arc<[u8]>);
impl MockRandPuller {
    pub fn new(data: &[u8]) -> Self {
        Self(Arc::from(data))
    }
}
impl RandPuller for MockRandPuller {
    type Error = ();
    fn pull(
        &mut self,
        range: &ProgressEntry,
    ) -> impl TryStream<Ok = Bytes, Error = Self::Error> + Send + Unpin {
        let data = &self.0[range.start as usize..range.end as usize];
        stream::iter(data.iter().map(|e| Ok(Bytes::from_iter([*e]))))
    }
}

pub struct MockSeqPuller(pub Vec<u8>);
impl MockSeqPuller {
    pub fn new(data: Vec<u8>) -> Self {
        Self(data)
    }
}
impl SeqPuller for MockSeqPuller {
    type Error = ();
    fn pull(&mut self) -> impl TryStream<Ok = Bytes, Error = Self::Error> + Send + Unpin {
        stream::iter(self.0.iter().map(|e| Ok(Bytes::from_iter([*e]))))
    }
}

#[derive(Clone)]
pub struct MockRandPusher {
    pub receive: Arc<Mutex<Vec<u8>>>,
    pub result: Arc<[u8]>,
}
impl MockRandPusher {
    pub fn new(result: &[u8]) -> Self {
        Self {
            receive: Arc::new(Mutex::new(vec![0; result.len()])),
            result: Arc::from(result),
        }
    }
    pub async fn assert(&self) {
        let receive = self.receive.lock().await;
        assert_eq!(&receive[..], &self.result[..]);
    }
}
impl RandPusher for MockRandPusher {
    type Error = ();
    async fn push(&mut self, range: ProgressEntry, content: &[u8]) -> Result<(), Self::Error> {
        self.receive.lock().await[range.start as usize..range.end as usize]
            .copy_from_slice(content);
        Ok(())
    }
}

#[derive(Clone)]
pub struct MockSeqPusher {
    pub receive: Arc<Mutex<Vec<u8>>>,
    pub result: Arc<[u8]>,
}
impl MockSeqPusher {
    pub fn new(result: &[u8]) -> Self {
        Self {
            result: Arc::from(result),
            receive: Arc::new(Mutex::new(vec![])),
        }
    }
    pub async fn assert(&self) {
        let receive = self.receive.lock().await;
        assert_eq!(&receive[..], &self.result[..]);
    }
}
impl SeqPusher for MockSeqPusher {
    type Error = ();
    async fn push(&mut self, content: &[u8]) -> Result<(), Self::Error> {
        self.receive.lock().await.extend_from_slice(content);
        Ok(())
    }
}
