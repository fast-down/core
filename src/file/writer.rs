extern crate alloc;
use crate::{ProgressEntry, RandWriter, SeqWriter};
use alloc::vec::Vec;
use bytes::Bytes;
use memmap2::MmapMut;
use tokio::{
    fs::File,
    io::{self, AsyncSeekExt, AsyncWriteExt, BufWriter},
};

#[derive(Debug)]
pub struct SeqFileWriter {
    buffer: BufWriter<File>,
}
impl SeqFileWriter {
    pub fn new(file: File, buffer_size: usize) -> Self {
        Self {
            buffer: BufWriter::with_capacity(buffer_size, file),
        }
    }
}
impl SeqWriter for SeqFileWriter {
    type Error = io::Error;
    async fn write(&mut self, content: Bytes) -> Result<(), Self::Error> {
        self.buffer.write_all(&content).await
    }
    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.buffer.flush().await
    }
}

#[derive(Debug)]
pub struct RandFileWriterMmap {
    mmap: MmapMut,
    downloaded: usize,
    buffer_size: usize,
}
impl RandFileWriterMmap {
    pub async fn new(file: File, size: u64, buffer_size: usize) -> Result<Self, io::Error> {
        file.set_len(size).await?;
        Ok(Self {
            mmap: unsafe { MmapMut::map_mut(&file) }?,
            downloaded: 0,
            buffer_size,
        })
    }
}
impl RandWriter for RandFileWriterMmap {
    type Error = io::Error;
    async fn write(&mut self, range: ProgressEntry, bytes: Bytes) -> Result<(), Self::Error> {
        self.mmap[range.start as usize..range.end as usize].copy_from_slice(&bytes);
        self.downloaded += bytes.len();
        if self.downloaded >= self.buffer_size {
            self.mmap.flush()?;
            self.downloaded = 0;
        }
        Ok(())
    }
    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.mmap.flush_async()?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct RandFileWriterStd {
    buffer: BufWriter<File>,
    cache: Vec<(u64, Bytes)>,
    p: u64,
    cache_size: usize,
    buffer_size: usize,
}
impl RandFileWriterStd {
    pub async fn new(file: File, size: u64, buffer_size: usize) -> Result<Self, io::Error> {
        file.set_len(size).await?;
        Ok(Self {
            buffer: BufWriter::with_capacity(buffer_size, file),
            cache: Vec::new(),
            p: 0,
            cache_size: 0,
            buffer_size,
        })
    }
}
impl RandWriter for RandFileWriterStd {
    type Error = io::Error;
    async fn write(&mut self, range: ProgressEntry, bytes: Bytes) -> Result<(), Self::Error> {
        let pos = self.cache.partition_point(|(i, _)| i < &range.start);
        self.cache_size += bytes.len();
        self.cache.insert(pos, (range.start, bytes));
        if self.cache_size >= self.buffer_size {
            self.flush().await?;
        }
        Ok(())
    }
    async fn flush(&mut self) -> Result<(), Self::Error> {
        for (start, bytes) in self.cache.drain(..) {
            let len = bytes.len();
            self.cache_size -= len;
            if start != self.p {
                self.buffer.seek(io::SeekFrom::Start(start)).await?;
                self.p = start;
            }
            self.buffer.write_all(&bytes).await?;
            self.p += len as u64;
        }
        self.buffer.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use tempfile::NamedTempFile;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn test_seq_file_writer() -> Result<(), io::Error> {
        // 创建一个临时文件用于测试
        let temp_file = NamedTempFile::new()?;
        let file_path = temp_file.path().to_path_buf();

        // 初始化 SeqFileWriter
        let mut writer = SeqFileWriter::new(temp_file.reopen()?.into(), 1024);

        // 写入数据
        let data1 = Bytes::from("Hello, ");
        let data2 = Bytes::from("world!");
        writer.write(data1).await?;
        writer.write(data2).await?;
        writer.flush().await?;

        // 验证文件内容
        let mut file_content = Vec::new();
        File::open(&file_path)
            .await?
            .read_to_end(&mut file_content)
            .await?;
        assert_eq!(file_content, b"Hello, world!");

        Ok(())
    }

    #[tokio::test]
    async fn test_rand_file_writer() -> Result<(), io::Error> {
        // 创建一个临时文件用于测试
        let temp_file = NamedTempFile::new()?;
        let file_path = temp_file.path().to_path_buf();

        // 初始化 RandFileWriter，假设文件大小为 10 字节
        let mut writer =
            RandFileWriterMmap::new(temp_file.reopen()?.into(), 10, 8 * 1024 * 1024).await?;

        // 写入数据
        let data = Bytes::from("234");
        let range = 2..5;
        writer.write(range, data).await?;
        writer.flush().await?;

        // 验证文件内容
        let mut file_content = Vec::new();
        File::open(&file_path)
            .await?
            .read_to_end(&mut file_content)
            .await?;
        assert_eq!(file_content, b"\0\x00234\0\0\0\0\0");

        Ok(())
    }
}
