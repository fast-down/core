use crate::{ProgressEntry, Pusher};
use bytes::Bytes;
use std::{
    fs::File,
    io::{Seek, Write},
};
use tokio::io::SeekFrom;

#[derive(Debug)]
pub struct FilePusher {
    file: File,
}

impl FilePusher {
    /// # Errors
    /// 当 `fs::set_len` 失败时返回错误。
    pub async fn new(file: tokio::fs::File, size: u64) -> std::io::Result<Self> {
        file.set_len(size).await?;
        Ok(Self {
            file: file.into_std().await,
        })
    }

    /// # Errors
    /// 当 `Seek` 或 `Write` 失败时返回错误。
    pub fn write_at(&mut self, start: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.file.seek(SeekFrom::Start(start))?;
        self.file.write_all(bytes)?;
        Ok(())
    }
}

impl Pusher for FilePusher {
    type Error = std::io::Error;
    fn push(&mut self, range: &ProgressEntry, bytes: Bytes) -> Result<(), (Self::Error, Bytes)> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.write_at(range.start, &bytes).map_err(|e| (e, bytes))?;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
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
        let mut pusher = FilePusher::new(temp_file.reopen().unwrap().into(), 10)
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
