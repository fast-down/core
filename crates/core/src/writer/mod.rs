#[cfg(feature = "file")]
pub mod file;

use crate::ProgressEntry;
use bytes::Bytes;

pub trait SeqWriter: Send {
    fn write_sequentially(&mut self, bytes: Bytes) -> Result<(), std::io::Error>;
    fn flush(&mut self) -> Result<(), std::io::Error>;
}

pub trait RandWriter: Send {
    fn write_randomly(&mut self, range: ProgressEntry, bytes: Bytes) -> Result<(), std::io::Error>;
    fn flush(&mut self) -> Result<(), std::io::Error>;
}
