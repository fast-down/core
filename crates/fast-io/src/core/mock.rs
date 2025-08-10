extern crate alloc;
use crate::{ProgressEntry, RandReader, RandWriter, SeqReader, SeqWriter};
use alloc::vec;
use alloc::{sync::Arc, vec::Vec};
use bytes::Bytes;
use futures::{TryStream, stream};
use tokio::sync::Mutex;

pub fn build_mock_data(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 256) as u8).collect()
}

#[derive(Clone)]
pub struct MockRandReader(pub Arc<[u8]>);
impl MockRandReader {
    pub fn new(data: &[u8]) -> Self {
        Self(Arc::from(data))
    }
}
impl RandReader for MockRandReader {
    type Error = ();
    fn read(
        &mut self,
        range: &ProgressEntry,
    ) -> impl TryStream<Ok = Bytes, Error = Self::Error> + Send + Unpin {
        let data = &self.0[range.start as usize..range.end as usize];
        stream::iter(data.iter().map(|e| Ok(Bytes::from_iter([*e]))))
    }
}

pub struct MockSeqReader(pub Vec<u8>);
impl MockSeqReader {
    pub fn new(data: Vec<u8>) -> Self {
        Self(data)
    }
}
impl SeqReader for MockSeqReader {
    type Error = ();
    fn read(&mut self) -> impl TryStream<Ok = Bytes, Error = Self::Error> + Send + Unpin {
        stream::iter(self.0.iter().map(|e| Ok(Bytes::from_iter([*e]))))
    }
}

#[derive(Clone)]
pub struct MockRandWriter {
    pub receive: Arc<Mutex<Vec<u8>>>,
    pub result: Arc<[u8]>,
}
impl MockRandWriter {
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
impl RandWriter for MockRandWriter {
    type Error = ();
    async fn write(&mut self, range: ProgressEntry, content: Bytes) -> Result<(), Self::Error> {
        self.receive.lock().await[range.start as usize..range.end as usize]
            .copy_from_slice(&content);
        Ok(())
    }
}

#[derive(Clone)]
pub struct MockSeqWriter {
    pub receive: Arc<Mutex<Vec<u8>>>,
    pub result: Arc<[u8]>,
}
impl MockSeqWriter {
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
impl SeqWriter for MockSeqWriter {
    type Error = ();
    async fn write(&mut self, content: Bytes) -> Result<(), Self::Error> {
        self.receive.lock().await.extend_from_slice(&content);
        Ok(())
    }
}
