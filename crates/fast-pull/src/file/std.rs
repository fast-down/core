use crate::{ProgressEntry, Pusher};
use bytes::Bytes;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufWriter, Seek, Write},
    sync::Arc,
};
use tokio::io::SeekFrom;

#[derive(Debug)]
pub struct FilePusher {
    buffer: BufWriter<Arc<File>>,
    file: Arc<File>,
    cache: BTreeMap<u64, Bytes>,
    p: u64,
    cache_size: usize,
    buffer_size: usize,
}

impl FilePusher {
    /// # Errors
    /// 当 `fs::set_len` 失败时返回错误。
    pub async fn new(
        file: tokio::fs::File,
        size: u64,
        buffer_size: usize,
    ) -> std::io::Result<Self> {
        file.set_len(size).await?;
        let file = Arc::new(file.into_std().await);
        Ok(Self {
            buffer: BufWriter::with_capacity(buffer_size, file.clone()),
            cache: BTreeMap::new(),
            p: 0,
            cache_size: 0,
            file,
            buffer_size,
        })
    }

    /// 当缓存满时调用：合并相邻的块，并优先写入最大的连续块，
    /// 直到缓存大小回落到 "低水位线"
    fn evict_some(&mut self) -> Result<(), std::io::Error> {
        let low_watermark = self.buffer_size / 2;
        if self.cache_size <= low_watermark {
            return Ok(());
        }
        let mut runs: Vec<(usize, Vec<u64>)> = Vec::new();
        let mut current_keys = Vec::new();
        let mut current_len = 0;
        let mut expected_next = 0;
        for (&start, bytes) in &self.cache {
            if current_keys.is_empty() {
                current_keys.push(start);
                current_len = bytes.len();
            } else if start == expected_next {
                current_keys.push(start);
                current_len += bytes.len();
            } else {
                runs.push((current_len, current_keys));
                current_keys = vec![start];
                current_len = bytes.len();
            }
            expected_next = start + bytes.len() as u64;
        }
        if !current_keys.is_empty() {
            runs.push((current_len, current_keys));
        }
        runs.sort_unstable_by_key(|b| std::cmp::Reverse(b.0));
        for (_, keys) in runs {
            let start = keys[0];
            if start != self.p {
                self.buffer.seek(SeekFrom::Start(start))?;
                self.p = start;
            }
            for key in keys {
                if let Some(bytes) = self.cache.remove(&key) {
                    if let Err(e) = self.buffer.write_all(&bytes) {
                        self.cache.insert(key, bytes);
                        return Err(e);
                    }
                    self.cache_size -= bytes.len();
                    self.p += bytes.len() as u64;
                }
            }
            if self.cache_size <= low_watermark {
                break;
            }
        }
        Ok(())
    }

    /// 清空并写入全部剩余数据
    fn flush_all(&mut self) -> Result<(), std::io::Error> {
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

impl Pusher for FilePusher {
    type Error = std::io::Error;
    fn push(&mut self, range: &ProgressEntry, bytes: Bytes) -> Result<(), (Self::Error, Bytes)> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.cache_size += bytes.len();
        if let Some(old_bytes) = self.cache.insert(range.start, bytes) {
            self.cache_size -= old_bytes.len();
        }
        if self.cache_size >= self.buffer_size {
            self.evict_some().map_err(|e| (e, Bytes::new()))?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.flush_all()?;
        self.file.sync_all()
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
