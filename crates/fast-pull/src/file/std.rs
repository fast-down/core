use crate::{ProgressEntry, Pusher};
use bytes::Bytes;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufWriter, Seek, Write},
};
use tokio::io::SeekFrom;

#[derive(Debug)]
pub struct FilePusher {
    buffer: BufWriter<File>,
    cache: BTreeMap<u64, Bytes>,
    p: u64,
    cache_size: usize,
    buffer_size: usize,
}
impl FilePusher {
    /// # Errors
    /// 1. 当 `fs::set_len` 失败时返回错误。
    /// 2. 当 `BufWriter::with_capacity` 失败时返回错误。
    pub async fn new(
        file: tokio::fs::File,
        size: u64,
        buffer_size: usize,
    ) -> tokio::io::Result<Self> {
        file.set_len(size).await?;
        Ok(Self {
            buffer: BufWriter::with_capacity(buffer_size, file.into_std().await),
            cache: BTreeMap::new(),
            p: 0,
            cache_size: 0,
            buffer_size,
        })
    }
}
impl Pusher for FilePusher {
    type Error = tokio::io::Error;
    fn push(&mut self, range: &ProgressEntry, bytes: Bytes) -> Result<(), (Self::Error, Bytes)> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.cache_size += bytes.len();
        self.cache.insert(range.start, bytes);
        if self.cache_size >= self.buffer_size {
            self.flush().map_err(|e| {
                let bytes = self.cache.remove(&range.start);
                (e, bytes.unwrap_or_default())
            })?;
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        while let Some(entry) = self.cache.first_entry() {
            let start = *entry.key();
            let bytes = entry.get();
            let len = bytes.len();
            if start != self.p {
                self.buffer.seek(SeekFrom::Start(start))?;
                self.p = start;
            }
            self.buffer.write_all(bytes)?;
            entry.remove_entry();
            self.cache_size -= len;
            self.p += len as u64;
        }
        self.buffer.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::{io::Read, vec::Vec};
    use tempfile::NamedTempFile;

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
        pusher.push(&range, data[..].into()).unwrap();
        pusher.flush().unwrap();

        // 验证文件内容
        let mut file_content = Vec::new();
        File::open(file_path)
            .unwrap()
            .read_to_end(&mut file_content)
            .unwrap();
        assert_eq!(file_content, b"\0\x00234\0\0\0\0\0");
    }
}
