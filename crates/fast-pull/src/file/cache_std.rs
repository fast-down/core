use crate::{CacheSeqPusher, ProgressEntry, Pusher, file::StdFilePusher};
use bytes::Bytes;

#[derive(Debug)]
pub struct CacheFilePusher {
    inner: CacheSeqPusher<StdFilePusher>,
}

impl CacheFilePusher {
    /// # Errors
    /// 1. 当 `fs::set_len` 失败时返回错误。
    /// 2. 当 `FilePusher` 初始化失败时返回错误。
    pub async fn new(
        file: tokio::fs::File,
        size: u64,
        high_watermark: usize,
        low_watermark: usize,
        buffer_size: usize,
    ) -> std::io::Result<Self> {
        let file_pusher = StdFilePusher::new(file, size, buffer_size).await?;
        let inner = CacheSeqPusher::new(file_pusher, high_watermark, low_watermark);
        Ok(Self { inner })
    }
}

impl Pusher for CacheFilePusher {
    type Error = std::io::Error;

    #[inline]
    fn push(&mut self, range: &ProgressEntry, bytes: Bytes) -> Result<(), (Self::Error, Bytes)> {
        self.inner.push(range, bytes)
    }

    #[inline]
    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::{fs::File, io::Read, vec::Vec};
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_cache_file_pusher() {
        let temp_file = NamedTempFile::new().unwrap();
        let file_path = temp_file.path();

        // 初始化包装器：
        // 内存缓存设为 1MB，水位线 512KB，BufWriter 设为 8KB
        let mut pusher = CacheFilePusher::new(
            temp_file.reopen().unwrap().into(),
            10,
            1024 * 1024,
            512 * 1024,
            8 * 1024,
        )
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
