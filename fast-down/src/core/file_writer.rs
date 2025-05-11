use crate::{Flush, Progress, RandWriter, SeqWriter};
use bytes::Bytes;
use color_eyre::Result;
use memmap2::MmapMut;
use std::{
    fs::File,
    io::{BufWriter, Write},
};

#[derive(Debug)]
pub struct SeqFileWriter {
    buffer: BufWriter<File>,
}

impl SeqFileWriter {
    pub fn new(file: File, write_buffer_size: usize) -> Result<Self> {
        Ok(Self {
            buffer: BufWriter::with_capacity(write_buffer_size, file),
        })
    }
}

impl SeqWriter for SeqFileWriter {
    fn write_sequentially(&mut self, bytes: Bytes) -> Result<()> {
        self.buffer.write_all(&bytes)?;
        Ok(())
    }
}

impl Flush for SeqFileWriter {
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
        Ok(Self {
            mmap: unsafe { MmapMut::map_mut(&file) }?,
        })
    }
}

impl RandWriter for RandFileWriter {
    fn write_randomly(&mut self, range: Progress, bytes: Bytes) -> Result<()> {
        self.mmap[range].copy_from_slice(&bytes);
        Ok(())
    }
}

impl Flush for RandFileWriter {
    fn flush(&mut self) -> Result<()> {
        self.mmap.flush()?;
        Ok(())
    }
}

#[cfg(test)]
#[cfg(feature = "file")]
mod tests {
    use super::*;
    use crate::{Flush, RandWriter, SeqWriter};
    use bytes::Bytes;
    use std::{fs::File, io::Read};
    use tempfile::NamedTempFile;

    #[test]
    fn test_seq_file_writer() -> Result<()> {
        // 创建一个临时文件用于测试
        let temp_file = NamedTempFile::new()?;
        let file_path = temp_file.path().to_path_buf();

        // 初始化 SeqFileWriter
        let mut writer = SeqFileWriter::new(temp_file.reopen()?, 1024)?;

        // 写入数据
        let data1 = Bytes::from("Hello, ");
        let data2 = Bytes::from("world!");
        writer.write_sequentially(data1)?;
        writer.write_sequentially(data2)?;
        writer.flush()?;

        // 验证文件内容
        let mut file_content = Vec::new();
        File::open(&file_path)?.read_to_end(&mut file_content)?;
        assert_eq!(file_content, b"Hello, world!");

        Ok(())
    }

    #[test]
    fn test_rand_file_writer() -> Result<()> {
        // 创建一个临时文件用于测试
        let temp_file = NamedTempFile::new()?;
        let file_path = temp_file.path().to_path_buf();

        // 初始化 RandFileWriter，假设文件大小为 10 字节
        let mut writer = RandFileWriter::new(temp_file.reopen()?, 10)?;

        // 写入数据
        let data = Bytes::from("234");
        let range = 2..5;
        writer.write_randomly(range, data)?;
        writer.flush()?;

        // 验证文件内容
        let mut file_content = Vec::new();
        File::open(&file_path)?.read_to_end(&mut file_content)?;
        assert_eq!(file_content, b"\0\0234\0\0\0\0\0");

        Ok(())
    }
}
