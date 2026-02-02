extern crate std;
use crate::{ProgressEntry, Pusher};
use bytes::Bytes;
use mmap_io::{MemoryMappedFile, MmapIoError, MmapMode, flush::FlushPolicy};
use std::path::Path;
use tokio::fs::{self, OpenOptions};

#[derive(Debug)]
pub struct MmapFilePusher {
    mmap: MemoryMappedFile,
}
impl MmapFilePusher {
    pub async fn new(path: impl AsRef<Path>, size: u64) -> Result<Self, MmapIoError> {
        let mmap_builder = MemoryMappedFile::builder(&path)
            .mode(MmapMode::ReadWrite)
            .huge_pages(true)
            .flush_policy(FlushPolicy::Manual);
        Ok(Self {
            mmap: if fs::try_exists(&path).await? {
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
        })
    }
}
impl Pusher for MmapFilePusher {
    type Error = MmapIoError;
    fn push(&mut self, range: &ProgressEntry, bytes: Bytes) -> Result<(), (Self::Error, Bytes)> {
        self.mmap
            .update_region(range.start, &bytes)
            .map_err(|e| (e, bytes))
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        self.mmap.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;
    use tempfile::NamedTempFile;
    use tokio::{fs::File, io::AsyncReadExt};

    #[tokio::test]
    async fn test_rand_file_pusher() {
        // 创建一个临时文件用于测试
        let temp_file = NamedTempFile::new().unwrap();
        let file_path = temp_file.path();

        // 初始化 RandFilePusher，假设文件大小为 10 字节
        let mut pusher = MmapFilePusher::new(file_path, 10).await.unwrap();

        // 写入数据
        let data = b"234";
        let range = 2..5;
        pusher.push(&range, data[..].into()).unwrap();
        pusher.flush().unwrap();

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
