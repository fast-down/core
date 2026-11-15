//! Test doubles shared by more than one module's unit tests.
//!
//! Fakes used by a single test module stay next to that module.

#![allow(clippy::unwrap_used)]

use crate::{ProgressEntry, ProgressListener, Pusher};
use bytes::Bytes;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Bytes from a string literal.
pub fn bb(s: &str) -> Bytes {
    Bytes::copy_from_slice(s.as_bytes())
}

/// Records every push and can be told to fail the next one, returning the bytes
/// so the cache decorators can re-buffer them.
#[derive(Clone)]
pub struct RecordingSink {
    pub pushes: Arc<Mutex<Vec<(ProgressEntry, Bytes)>>>,
    pub fail_next: Arc<AtomicBool>,
    pub listener_set: Arc<AtomicBool>,
}

impl RecordingSink {
    pub fn new() -> Self {
        Self {
            pushes: Arc::new(Mutex::new(Vec::new())),
            fail_next: Arc::new(AtomicBool::new(false)),
            listener_set: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Default for RecordingSink {
    fn default() -> Self {
        Self::new()
    }
}

impl Pusher for RecordingSink {
    type Error = std::io::Error;

    fn set_listener(&mut self, _: ProgressListener) {
        self.listener_set.store(true, Ordering::SeqCst);
    }

    fn push(&mut self, range: &ProgressEntry, bytes: Bytes) -> Result<(), (Self::Error, Bytes)> {
        if self.fail_next.fetch_and(false, Ordering::SeqCst) {
            return Err((std::io::Error::other("boom"), bytes));
        }
        self.pushes.lock().unwrap().push((range.clone(), bytes));
        Ok(())
    }
}

/// Inner sink that persists only the first 2 bytes of its first chunk, then
/// fails with the unwritten tail, to exercise partial-write re-buffering.
#[derive(Clone)]
pub struct PartialSink {
    pub pushes: Arc<Mutex<Vec<(ProgressEntry, Bytes)>>>,
    partial: Arc<AtomicBool>,
}

impl PartialSink {
    pub fn new() -> Self {
        Self {
            pushes: Arc::new(Mutex::new(Vec::new())),
            partial: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Default for PartialSink {
    fn default() -> Self {
        Self::new()
    }
}

impl Pusher for PartialSink {
    type Error = std::io::Error;

    fn push(&mut self, range: &ProgressEntry, bytes: Bytes) -> Result<(), (Self::Error, Bytes)> {
        self.pushes
            .lock()
            .unwrap()
            .push((range.clone(), bytes.clone()));
        if !self.partial.swap(true, Ordering::SeqCst) {
            let rem = bytes.slice(2..);
            return Err((std::io::Error::other("partial"), rem));
        }
        Ok(())
    }
}
