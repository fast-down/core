mod mmap;
mod std;

pub use mmap::*;
pub use std::*;

#[derive(thiserror::Error, Debug)]
pub enum FilePusherError {
    #[error(transparent)]
    MmapIo(#[from] mmap_io::MmapIoError),
    #[error(transparent)]
    TokioIo(#[from] tokio::io::Error),
}
