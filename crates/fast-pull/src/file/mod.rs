mod mmap;
mod std;

pub use mmap::*;
pub use std::*;

use mmap_io::MmapIoError;
use tokio::io;

#[derive(thiserror::Error, Debug)]
pub enum FilePusherError {
    #[error(transparent)]
    MmapIo(#[from] MmapIoError),
    #[error(transparent)]
    TokioIo(#[from] io::Error),
}
