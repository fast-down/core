extern crate std;
use crate::{ProgressEntry, RandWriter, SeqWriter};
use bytes::Bytes;
use mmap_io::{MemoryMappedFile, MmapIoError, MmapMode, flush::FlushPolicy};
use std::{path::Path, vec::Vec};
use thiserror::Error;
use tokio::{
    fs::{File, OpenOptions},
    io::{self, AsyncSeekExt, AsyncWriteExt, BufWriter, SeekFrom},
};

#[derive(Error, Debug)]
pub enum FileWriterError {
    #[error(transparent)]
    MmapIo(#[from] MmapIoError),
    #[error(transparent)]
    TokioIo(#[from] io::Error),
}

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
    type Error = FileWriterError;
    async fn write(&mut self, content: Bytes) -> Result<(), Self::Error> {
        Ok(self.buffer.write_all(&content).await?)
    }
    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(self.buffer.flush().await?)
    }
}

#[derive(Debug)]
pub struct RandFileWriterMmap {
    mmap: MemoryMappedFile,
    downloaded: usize,
    buffer_size: usize,
}
impl RandFileWriterMmap {
    pub async fn new(
        path: impl AsRef<Path>,
        size: u64,
        buffer_size: usize,
    ) -> Result<Self, FileWriterError> {
        let mmap_builder = MemoryMappedFile::builder(&path)
            .mode(MmapMode::ReadWrite)
            .flush_policy(FlushPolicy::Manual);
        Ok(Self {
            mmap: if path.as_ref().try_exists()? {
                OpenOptions::new()
                    .write(true)
                    .open(path)
                    .await?
                    .set_len(size)
                    .await?;
                mmap_builder.open()
            } else {
                mmap_builder.size(size).create()
            }?,
            downloaded: 0,
            buffer_size,
        })
    }
}
impl RandWriter for RandFileWriterMmap {
    type Error = FileWriterError;
    async fn write(&mut self, range: ProgressEntry, bytes: Bytes) -> Result<(), Self::Error> {
        self.mmap
            .as_slice_mut(range.start, bytes.len() as u64)?
            .as_mut()
            .copy_from_slice(&bytes);
        self.downloaded += bytes.len();
        if self.downloaded >= self.buffer_size {
            self.mmap.flush_async().await?;
            self.downloaded = 0;
        }
        Ok(())
    }
    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.mmap.flush_async().await?;
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
    pub async fn new(file: File, size: u64, buffer_size: usize) -> Result<Self, FileWriterError> {
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
    type Error = FileWriterError;
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
                self.buffer.seek(SeekFrom::Start(start)).await?;
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
    async fn test_seq_file_writer() {
        // 创建一个临时文件用于测试
        let temp_file = NamedTempFile::new().unwrap();
        let file_path = temp_file.path().to_path_buf();

        // 初始化 SeqFileWriter
        let mut writer = SeqFileWriter::new(temp_file.reopen().unwrap().into(), 1024);

        // 写入数据
        let data1 = Bytes::from("Hello, ");
        let data2 = Bytes::from("world!");
        writer.write(data1).await.unwrap();
        writer.write(data2).await.unwrap();
        writer.flush().await.unwrap();

        // 验证文件内容
        let mut file_content = Vec::new();
        File::open(&file_path)
            .await
            .unwrap()
            .read_to_end(&mut file_content)
            .await
            .unwrap();
        assert_eq!(file_content, b"Hello, world!");
    }

    #[tokio::test]
    async fn test_rand_file_writer() {
        // 创建一个临时文件用于测试
        let temp_file = NamedTempFile::new().unwrap();
        let file_path = temp_file.path();

        // 初始化 RandFileWriter，假设文件大小为 10 字节
        let mut writer = RandFileWriterMmap::new(file_path, 10, 8 * 1024 * 1024)
            .await
            .unwrap();

        // 写入数据
        let data = Bytes::from("234");
        let range = 2..5;
        writer.write(range, data).await.unwrap();
        writer.flush().await.unwrap();

        // 验证文件内容
        let mut file_content = Vec::new();
        File::open(&file_path)
            .await
            .unwrap()
            .read_to_end(&mut file_content)
            .await
            .unwrap();
        assert_eq!(file_content, b"\0\x00234\0\0\0\0\0");
    }
}
