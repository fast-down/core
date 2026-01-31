extern crate alloc;
use crate::{RandPusher, SeqPusher};
use alloc::{sync::Arc, vec::Vec};
use bytes::Bytes;
use parking_lot::Mutex;

#[derive(Debug, Default, Clone)]
pub struct MemPusher {
    pub receive: Arc<Mutex<Vec<u8>>>,
}
impl MemPusher {
    pub fn new() -> Self {
        Self {
            receive: Arc::new(Mutex::new(Vec::new())),
        }
    }
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            receive: Arc::new(Mutex::new(Vec::with_capacity(capacity))),
        }
    }
}
impl SeqPusher for MemPusher {
    type Error = ();
    async fn push(&mut self, content: Bytes) -> Result<(), (Self::Error, Bytes)> {
        self.receive.lock().extend_from_slice(&content);
        Ok(())
    }
}
impl RandPusher for MemPusher {
    type Error = ();
    async fn push(
        &mut self,
        range: crate::ProgressEntry,
        content: Bytes,
    ) -> Result<(), (Self::Error, Bytes)> {
        let mut guard = self.receive.lock();
        if range.start as usize == guard.len() {
            guard.extend_from_slice(&content);
        } else {
            if guard.len() < range.end as usize {
                guard.resize(range.end as usize, 0);
            }
            guard[range.start as usize..range.end as usize].copy_from_slice(&content);
        }
        Ok(())
    }
}
