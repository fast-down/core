//! In-memory pusher backed by a shared `Vec<u8>` (feature `mem`).

use crate::{ProgressEntry, ProgressListener, Pusher};
use bytes::Bytes;
use parking_lot::Mutex;
use std::{sync::Arc, vec::Vec};

/// In-memory pusher for testing or buffer-based workflows.
///
/// All pushed data is stored in a shared `Vec<u8>` protected by a mutex.
/// Supports random-access writes via `copy_from_slice` for non-sequential ranges.
#[derive(Default)]
pub struct MemPusher {
    pub receive: Arc<Mutex<Vec<u8>>>,
    listener: Option<ProgressListener>,
}
impl Clone for MemPusher {
    fn clone(&self) -> Self {
        Self {
            receive: self.receive.clone(),
            listener: None,
        }
    }
}
impl MemPusher {
    #[must_use]
    pub fn new() -> Self {
        Self {
            receive: Arc::new(Mutex::new(Vec::new())),
            listener: None,
        }
    }
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            receive: Arc::new(Mutex::new(Vec::with_capacity(capacity))),
            listener: None,
        }
    }
}
impl std::fmt::Debug for MemPusher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemPusher")
            .field("receive", &self.receive)
            .finish_non_exhaustive()
    }
}
impl Pusher for MemPusher {
    type Error = std::convert::Infallible;

    fn set_listener(&mut self, cb: ProgressListener) {
        self.listener = Some(cb);
    }

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
        drop(guard);
        if let Some(l) = &mut self.listener {
            l(range.clone());
        }
        Ok(())
    }
}
