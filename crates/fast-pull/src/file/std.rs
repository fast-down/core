extern crate std;
use crate::{ProgressEntry, RandPusher, SeqPusher};
use bytes::Bytes;
use std::collections::BTreeMap;
use tokio::{
    fs::File,
    io::{AsyncSeekExt, AsyncWriteExt, BufWriter, SeekFrom},
};

#[derive(Debug)]
pub struct FilePusher {
    buffer: BufWriter<File>,
    cache: BTreeMap<u64, Bytes>,
    p: u64,
    cache_size: usize,
    buffer_size: usize,
}
impl FilePusher {
    pub async fn new(file: File, size: u64, buffer_size: usize) -> tokio::io::Result<Self> {
        file.set_len(size).await?;
        Ok(Self {
            buffer: BufWriter::with_capacity(buffer_size, file),
            cache: BTreeMap::new(),
            p: 0,
            cache_size: 0,
            buffer_size,
        })
    }
}
impl SeqPusher for FilePusher {
    type Error = tokio::io::Error;
    async fn push(&mut self, content: Bytes) -> Result<(), (Self::Error, Bytes)> {
        self.buffer
            .write_all(&content)
            .await
            .map_err(|e| (e, content))
    }
    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.buffer.flush().await
    }
}
impl RandPusher for FilePusher {
    type Error = tokio::io::Error;
    async fn push(
        &mut self,
        range: ProgressEntry,
        bytes: Bytes,
    ) -> Result<(), (Self::Error, Bytes)> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.cache_size += bytes.len();
        self.cache.insert(range.start, bytes);
        if self.cache_size >= self.buffer_size {
            RandPusher::flush(self).await.map_err(|e| {
                let bytes = self.cache.remove(&range.start);
                (e, bytes.unwrap_or_default())
            })?;
        }
        Ok(())
    }
    async fn flush(&mut self) -> Result<(), Self::Error> {
        while let Some(entry) = self.cache.first_entry() {
            let start = *entry.key();
            let bytes = entry.get();
            let len = bytes.len();
            if start != self.p {
                self.buffer.seek(SeekFrom::Start(start)).await?;
                self.p = start;
            }
            self.buffer.write_all(bytes).await?;
            entry.remove_entry();
            self.cache_size -= len;
            self.p += len as u64;
        }
        self.buffer.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;
    use tempfile::NamedTempFile;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn test_seq_file_pusher() {
        // 创建一个临时文件用于测试
        let temp_file = NamedTempFile::new().unwrap();
        let file_path = temp_file.path().to_path_buf();

        // 初始化 SeqFilePusher
        let mut pusher = FilePusher::new(temp_file.reopen().unwrap().into(), 0, 1024)
            .await
            .unwrap();

        // 写入数据
        let data1 = b"Hello, ";
        let data2 = b"world!";
        SeqPusher::push(&mut pusher, data1[..].into())
            .await
            .unwrap();
        SeqPusher::push(&mut pusher, data2[..].into())
            .await
            .unwrap();
        SeqPusher::flush(&mut pusher).await.unwrap();

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
    async fn test_rand_file_pusher() {
        // 创建一个临时文件用于测试
        let temp_file = NamedTempFile::new().unwrap();
        let file_path = temp_file.path();

        // 初始化 RandFilePusher，假设文件大小为 10 字节
        let mut pusher = FilePusher::new(temp_file.reopen().unwrap().into(), 10, 8 * 1024 * 1024)
            .await
            .unwrap();

        // 写入数据
        let data = b"234";
        let range = 2..5;
        RandPusher::push(&mut pusher, range, data[..].into())
            .await
            .unwrap();
        RandPusher::flush(&mut pusher).await.unwrap();

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
