use crate::{RandWriter, SeqWriter};
use bytes::Bytes;
use color_eyre::Result;
use memmap2::MmapMut;
use std::{
    fs::File,
    io::{BufWriter, Write},
    ops::Range,
};

#[derive(Debug)]
pub struct SeqFileWriter {
    buffer: BufWriter<File>,
}

impl SeqFileWriter {
    pub fn new(file: File, write_buffer_size: usize) -> Result<Self> {
        let buffer = BufWriter::with_capacity(write_buffer_size, file);
        Ok(Self { buffer })
    }
}

impl SeqWriter for SeqFileWriter {
    fn write_sequentially(&mut self, bytes: Bytes) -> Result<()> {
        self.buffer.write_all(&bytes)?;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.buffer.flush()?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct RandFileWriter {
    mmap: MmapMut,
}

impl RandFileWriter {
    pub fn new(file: File, size: usize) -> Result<Self> {
        file.set_len(size as u64)?;
        let mmap = unsafe { MmapMut::map_mut(&file) }?;
        Ok(Self { mmap })
    }
}

impl RandWriter for RandFileWriter {
    fn write_randomly(&mut self, range: Range<usize>, bytes: Bytes) -> Result<()> {
        self.mmap[range].copy_from_slice(&bytes);
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.mmap.flush()?;
        Ok(())
    }
}
