use crate::Event;
use crossbeam_channel::Receiver;
use std::{
    any::Any,
    sync::{Arc, Mutex},
    thread::JoinHandle,
};

pub mod auto;
#[cfg(feature = "file")]
pub mod download_file;
pub mod multi;
pub mod prefetch;
pub mod single;

pub type CancelFn = Box<dyn FnOnce() + Send>;

#[derive(Clone)]
pub struct DownloadResult {
    event_chain: Receiver<Event>,
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    cancel_fn: Arc<Mutex<Option<CancelFn>>>,
}

impl DownloadResult {
    pub fn new(
        event_chain: Receiver<Event>,
        handle: JoinHandle<()>,
        cancel_fn: CancelFn,
    ) -> DownloadResult {
        Self {
            event_chain,
            handle: Arc::new(Mutex::new(Some(handle))),
            cancel_fn: Arc::new(Mutex::new(Some(cancel_fn))),
        }
    }

    pub fn join(&self) -> Result<(), Box<dyn Any + Send + 'static>> {
        if let Some(handle) = self.handle.lock().unwrap().take() {
            handle.join()?
        }
        Ok(())
    }

    pub fn cancel(&self) {
        if let Some(cancel_fn) = self.cancel_fn.lock().unwrap().take() {
            cancel_fn()
        }
    }
}

impl Iterator for DownloadResult {
    type Item = Event;

    fn next(&mut self) -> Option<Self::Item> {
        self.event_chain.recv().ok()
    }
}

impl<'a> IntoIterator for &'a DownloadResult {
    type Item = Event;
    type IntoIter = DownloadResultIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        DownloadResultIter {
            receiver: &self.event_chain,
        }
    }
}

pub struct DownloadResultIter<'a> {
    receiver: &'a Receiver<Event>,
}

impl<'a> Iterator for DownloadResultIter<'a> {
    type Item = Event;

    fn next(&mut self) -> Option<Self::Item> {
        self.receiver.recv().ok()
    }
}
