extern crate std;
use crate::{ProgressEntry, RandPusher, Total, file::FilePusherError};
use mmap_io::{MemoryMappedFile, MmapMode, flush::FlushPolicy};
use std::path::Path;
use tokio::fs::OpenOptions;

#[derive(Debug)]
pub struct MmapFilePusher {
    mmap: MemoryMappedFile,
    downloaded: usize,
    buffer_size: usize,
}
impl MmapFilePusher {
    pub async fn new(
        path: impl AsRef<Path>,
        size: u64,
        buffer_size: usize,
    ) -> Result<Self, FilePusherError> {
        let mmap_builder = MemoryMappedFile::builder(&path)
            .mode(MmapMode::ReadWrite)
            .huge_pages(true)
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
impl RandPusher for MmapFilePusher {
    type Error = FilePusherError;
    async fn push(&mut self, range: ProgressEntry, bytes: &[u8]) -> Result<(), Self::Error> {
        self.mmap
            .as_slice_mut(range.start, range.total())?
            .as_mut()
            .copy_from_slice(bytes);
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
        let mut pusher = MmapFilePusher::new(file_path, 10, 8 * 1024 * 1024)
            .await
            .unwrap();

        // 写入数据
        let data = b"234";
        let range = 2..5;
        pusher.push(range, &data[..]).await.unwrap();
        pusher.flush().await.unwrap();

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
