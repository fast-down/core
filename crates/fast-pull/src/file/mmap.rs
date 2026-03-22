use crate::{ProgressEntry, Pusher};
use bytes::Bytes;
use memmap2::MmapMut;

#[derive(Debug)]
pub struct MmapFilePusher {
    mmap: MmapMut,
}
impl MmapFilePusher {
    /// # Errors
    /// 1. 当 `fs::try_exists` 失败时返回错误。
    /// 2. 当 `fs::open` 失败时返回错误。
    /// 3. 当 `fs::set_len` 失败时返回错误。
    /// 4. 当 `mmap_io::open` 失败时返回错误。
    pub async fn new(file: tokio::fs::File, size: u64) -> std::io::Result<Self> {
        file.set_len(size).await?;
        let mmap = unsafe { MmapMut::map_mut(&file)? };
        Ok(Self { mmap })
    }
}
impl Pusher for MmapFilePusher {
    type Error = std::io::Error;
    fn push(&mut self, range: &ProgressEntry, bytes: Bytes) -> Result<(), (Self::Error, Bytes)> {
        #[allow(clippy::cast_possible_truncation)]
        self.mmap[range.start as usize..range.end as usize].copy_from_slice(&bytes);
        Ok(())
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        self.mmap.flush()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::{fs::File, io::Read, vec::Vec};
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_rand_file_pusher() {
        // 创建一个临时文件用于测试
        let temp_file = NamedTempFile::new().unwrap();
        let file_path = temp_file.path();

        // 初始化 RandFilePusher，假设文件大小为 10 字节
        let mut pusher = MmapFilePusher::new(temp_file.reopen().unwrap().into(), 10)
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
