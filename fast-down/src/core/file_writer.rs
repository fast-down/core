use crate::{Flush, Progress, RandWriter, SeqWriter, Total};
use bytes::Bytes;
use color_eyre::Result;
use memmap2::MmapMut;
use std::{
    fs::File,
    io::{BufWriter, Write},
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone)]
pub struct SeqFileWriter {
    buffer: Arc<Mutex<BufWriter<File>>>,
}

impl SeqFileWriter {
    pub fn new(file: File, write_buffer_size: usize) -> Result<Self> {
        let buffer = Arc::new(Mutex::new(BufWriter::with_capacity(
            write_buffer_size,
            file,
        )));
        Ok(Self { buffer })
    }
}

impl SeqWriter for SeqFileWriter {
    fn write_sequentially(&mut self, bytes: Bytes) -> Result<()> {
        self.buffer.lock().unwrap().write_all(&bytes)?;
        Ok(())
    }
}

impl Flush for SeqFileWriter {
    fn flush(&mut self) -> Result<()> {
        self.buffer.lock().unwrap().flush()?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct RandFileWriter {
    mmap: Arc<Mutex<MmapMut>>,
}

impl RandFileWriter {
    pub fn new(file: File, size: usize) -> Result<Self> {
        file.set_len(size as u64)?;
        let mmap = Arc::new(Mutex::new(unsafe { MmapMut::map_mut(&file) }?));
        Ok(Self { mmap })
    }
}

impl RandWriter for RandFileWriter {
    fn write_randomly(&mut self, range: Vec<Progress>, mut bytes: Bytes) -> Result<()> {
        for progress in range {
            let len = progress.total();
            self.mmap.lock().unwrap()[progress].copy_from_slice(&bytes.split_to(len));
        }
        Ok(())
    }
}

impl Flush for RandFileWriter {
    fn flush(&mut self) -> Result<()> {
        self.mmap.lock().unwrap().flush()?;
        Ok(())
    }
}
