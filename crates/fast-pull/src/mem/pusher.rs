use crate::{ProgressEntry, Pusher};
use bytes::Bytes;
use parking_lot::Mutex;
use std::{sync::Arc, vec::Vec};

#[derive(Debug, Default, Clone)]
pub struct MemPusher {
    pub receive: Arc<Mutex<Vec<u8>>>,
}
impl MemPusher {
    #[must_use]
    pub fn new() -> Self {
        Self {
            receive: Arc::new(Mutex::new(Vec::new())),
        }
    }
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            receive: Arc::new(Mutex::new(Vec::with_capacity(capacity))),
        }
    }
}
impl Pusher for MemPusher {
    type Error = ();
    fn push(&mut self, range: &ProgressEntry, content: Bytes) -> Result<(), (Self::Error, Bytes)> {
        #![allow(clippy::significant_drop_tightening, clippy::cast_possible_truncation)]
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
