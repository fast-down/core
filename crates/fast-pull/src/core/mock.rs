extern crate alloc;
use crate::{ProgressEntry, Puller, Pusher, ReadStream, SliceOrBytes, WriteStream};
use alloc::vec;
use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::Mutex;

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

struct MockPullStream(Arc<[u8]>, AtomicUsize, usize);
impl ReadStream for MockPullStream {
    type Error = core::convert::Infallible;

    async fn read_with<'a, F, Fut, Ret>(&'a mut self, read_fn: F) -> Result<Ret, Self::Error>
    where
        F: FnOnce(SliceOrBytes<'a>) -> Fut,
        Fut: Future<Output = Ret>,
    {
        let start = self.1.fetch_add(1, Ordering::SeqCst);
        let end = core::cmp::min(start + 1, self.2);
        if start >= end {
            return Ok(read_fn(SliceOrBytes::empty()).await);
        }
        Ok(read_fn(self.0[start..end].into()).await)
    }
}
impl Puller for MockRandPuller {
    type StreamError = core::convert::Infallible;
    type Error = core::convert::Infallible;
    async fn init_read(
        &self,
        maybe_entry: Option<&ProgressEntry>,
    ) -> Result<impl ReadStream<Error = Self::Error> + Send + Unpin, Self::Error> {
        Ok(MockPullStream(
            self.0.clone(),
            AtomicUsize::new(maybe_entry.map(|entry| entry.start as usize).unwrap_or(0)),
            maybe_entry
                .map(|entry| entry.end as usize)
                .unwrap_or(self.0.len()),
        ))
    }
}

pub struct MockSeqPuller(pub Vec<u8>, AtomicUsize);
impl MockSeqPuller {
    pub fn new(data: Vec<u8>) -> Self {
        Self(data, AtomicUsize::new(0))
    }
}

impl ReadStream for MockSeqPuller {
    type Error = core::convert::Infallible;
    async fn read_with<'a, F, Fut, Ret>(&'a mut self, read_fn: F) -> Result<Ret, Self::Error>
    where
        F: FnOnce(SliceOrBytes<'a>) -> Fut,
        Fut: Future<Output = Ret>,
    {
        let idx = self.1.fetch_add(1, Ordering::SeqCst);
        if idx >= self.0.len() {
            return Ok(read_fn(SliceOrBytes::empty()).await);
        }
        Ok(read_fn(self.0[idx..(idx + 1)].into()).await)
    }
}

#[derive(Clone)]
pub struct MockRandPusher(pub Arc<Mutex<Vec<u8>>>);
impl MockRandPusher {
    pub fn new(capacity: usize) -> Self {
        Self(Arc::new(Mutex::new(vec![0x42; capacity])))
    }

    pub fn into_vec(self) -> Vec<u8> {
        Arc::try_unwrap(self.0).unwrap().into_inner()
    }
}
pub struct MockRandPushStream(Arc<Mutex<Vec<u8>>>, usize);
impl WriteStream for MockRandPushStream {
    type Error = core::convert::Infallible;
    async fn write(&mut self, data: SliceOrBytes<'_>) -> Result<(), Self::Error> {
        self.0.lock()[self.1..(self.1 + data.len())].copy_from_slice(&data);
        self.1 += data.len();
        Ok(())
    }
}
impl Pusher for MockRandPusher {
    type Error = core::convert::Infallible;
    type StreamError = core::convert::Infallible;
    async fn init_write(
        &self,
        start_point: u64,
    ) -> Result<impl WriteStream<Error = Self::StreamError>, Self::Error> {
        Ok(MockRandPushStream(self.0.clone(), start_point as usize))
    }
}

#[derive(Clone)]
pub struct MockSeqPusher(Arc<Mutex<Vec<u8>>>);
impl MockSeqPusher {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }

    pub fn into_vec(self) -> Vec<u8> {
        Arc::try_unwrap(self.0).unwrap().into_inner()
    }
}
impl WriteStream for MockSeqPusher {
    type Error = core::convert::Infallible;
    async fn write(&mut self, buf: SliceOrBytes<'_>) -> Result<(), Self::Error> {
        self.0.lock().extend_from_slice(&buf);
        Ok(())
    }
}
