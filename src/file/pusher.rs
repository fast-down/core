use crate::ProgressEntry;
use crate::base::pusher::Pusher;
use bytes::Bytes;
use std::io;
use tokio::{
    fs::File,
    io::{AsyncWriteExt, BufWriter},
};

#[derive(Debug)]
pub struct SeqFileWriter {
    buffer: BufWriter<File>,
}

impl SeqFileWriter {
    pub fn new(file: File, write_buffer_size: usize) -> Self {
        Self {
            buffer: BufWriter::with_capacity(write_buffer_size, file),
        }
    }
}

impl Pusher for SeqFileWriter {
    type Error = io::Error;

    async fn push(&mut self, bytes: Bytes) -> Result<(), io::Error> {
        self.buffer.write_all(&bytes).await?;
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), io::Error> {
        self.buffer.flush().await?;
        Ok(())
    }
}

pub mod rand_file_writer_mmap {
    use super::*;
    use crate::base::pusher::RandomPusher;
    use memmap2::MmapMut;

    #[derive(Debug)]
    pub struct RandFileWriter {
        mmap: MmapMut,
        downloaded: usize,
        write_buffer_size: usize,
    }

    impl RandFileWriter {
        pub async fn new(
            file: File,
            size: u64,
            write_buffer_size: usize,
        ) -> Result<Self, std::io::Error> {
            file.set_len(size).await?;
            Ok(Self {
                mmap: unsafe { MmapMut::map_mut(&file) }?,
                downloaded: 0,
                write_buffer_size,
            })
        }
    }

    impl Pusher for RandFileWriter {
        type Error = io::Error;

        async fn push(&mut self, bytes: Bytes) -> Result<(), io::Error> {
            self.mmap.copy_from_slice(&bytes);
            Ok(())
        }

        async fn flush(&mut self) -> Result<(), io::Error> {
            self.mmap.flush_async()?;
            Ok(())
        }
    }

    impl RandomPusher for RandFileWriter {
        async fn push_range(
            &mut self,
            range: ProgressEntry,
            bytes: Bytes,
        ) -> Result<(), io::Error> {
            self.mmap[range.start as usize..range.end as usize].copy_from_slice(&bytes);
            self.downloaded += bytes.len();
            if self.downloaded >= self.write_buffer_size {
                self.mmap.flush()?;
                self.downloaded = 0;
            }
            Ok(())
        }
    }
}

pub mod rand_file_writer_std {
    use super::*;
    use crate::base::pusher::RandomPusher;
    use tokio::io::AsyncSeekExt;

    #[derive(Debug)]
    pub struct RandFileWriter {
        buffer: BufWriter<File>,
        cache: Vec<(u64, Bytes)>,
        p: u64,
        cache_size: usize,
        write_buffer_size: usize,
    }

    impl RandFileWriter {
        pub async fn new(
            file: File,
            size: u64,
            write_buffer_size: usize,
        ) -> Result<Self, io::Error> {
            file.set_len(size).await?;
            Ok(Self {
                buffer: BufWriter::with_capacity(write_buffer_size, file),
                cache: Vec::new(),
                p: 0,
                cache_size: 0,
                write_buffer_size,
            })
        }
    }

    impl Pusher for RandFileWriter {
        type Error = io::Error;

        async fn push(&mut self, content: Bytes) -> Result<(), Self::Error> {
            self.buffer.write(&content).await.map(|_| ())
        }

        async fn flush(&mut self) -> Result<(), io::Error> {
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

    impl RandomPusher for RandFileWriter {
        async fn push_range(
            &mut self,
            range: ProgressEntry,
            bytes: Bytes,
        ) -> Result<(), io::Error> {
            let pos = self.cache.partition_point(|(i, _)| i < &range.start);
            self.cache_size += bytes.len();
            self.cache.insert(pos, (range.start, bytes.clone()));
            if self.cache_size >= self.write_buffer_size {
                self.flush().await?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Pusher, RandomPusher};
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
        writer.push(data1).await?;
        writer.push(data2).await?;
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
    async fn test_rand_file_writer() -> Result<(), std::io::Error> {
        // 创建一个临时文件用于测试
        let temp_file = NamedTempFile::new()?;
        let file_path = temp_file.path().to_path_buf();

        // 初始化 RandFileWriter，假设文件大小为 10 字节
        let mut writer = rand_file_writer_mmap::RandFileWriter::new(
            temp_file.reopen()?.into(),
            10,
            8 * 1024 * 1024,
        )
        .await?;

        // 写入数据
        let data = Bytes::from("234");
        let range = 2..5;
        writer.push_range(range, data).await?;
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
