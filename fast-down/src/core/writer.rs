use crate::Progress;
use bytes::Bytes;
use color_eyre::Result;

pub trait SeqWriter: Send + Clone {
    fn write_sequentially(&mut self, bytes: Bytes) -> Result<()>;
}

pub trait RandWriter: Send + Clone {
    fn write_randomly(&mut self, range: Vec<Progress>, bytes: Bytes) -> Result<()>;
}

pub trait Flush: Send + Clone {
    fn flush(&mut self) -> Result<()>;
}
