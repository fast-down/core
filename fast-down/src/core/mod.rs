use crate::Event;
use crossbeam_channel::Receiver;
use std::thread::JoinHandle;

pub mod auto;
#[cfg(feature = "file")]
pub mod download_file;
#[cfg(feature = "file")]
pub mod file_writer;
pub mod multi;
pub mod prefetch;
pub mod single;
pub mod writer;

pub type CancelFn = Box<dyn FnOnce() + Send>;
pub struct DownloadResult {
    pub event_chain: Receiver<Event>,
    pub handle: JoinHandle<()>,
    pub cancel_fn: CancelFn,
}
