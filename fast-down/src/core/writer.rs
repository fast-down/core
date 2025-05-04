use crate::Progress;
use bytes::Bytes;
use color_eyre::Result;

pub trait SeqWriter: Send {
    fn write_sequentially(&mut self, bytes: Bytes) -> Result<()>;
    fn flush(&mut self) -> Result<()>;
}

pub trait RandWriter: Send {
    fn write_randomly(&mut self, range: Progress, bytes: Bytes) -> Result<()>;
    fn flush(&mut self) -> Result<()>;
}
