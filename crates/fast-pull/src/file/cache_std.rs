use crate::{CacheSeqPusher, ProgressEntry, Pusher, file::StdFilePusher};
use bytes::Bytes;

/// File pusher combining [`CacheSeqPusher`] with [`StdFilePusher`].
///
/// Provides out-of-order reordering on top of standard file I/O.
/// The write buffer, watermark levels, and sync-all behavior are forwarded
/// to [`StdFilePusher::new`].
#[derive(Debug)]
pub struct CacheFilePusher {
    inner: CacheSeqPusher<StdFilePusher>,
}

impl CacheFilePusher {
    /// # Errors
    /// 1. Returns an error if `fs::set_len` fails.
    /// 2. Returns an error if [`StdFilePusher`] initialization fails.
    pub async fn new(
        file: tokio::fs::File,
        size: u64,
        sync_all: bool,
        high_watermark: usize,
        low_watermark: usize,
        buffer_size: usize,
    ) -> std::io::Result<Self> {
        let file_pusher = StdFilePusher::new(file, size, buffer_size, sync_all).await?;
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

        // Initialize the wrapper:
        // cache size = 1MB, watermark = 512KB, BufWriter = 8KB
        let mut pusher = CacheFilePusher::new(
            temp_file.reopen().unwrap().into(),
            10,
            false,
            1024 * 1024,
            512 * 1024,
            8 * 1024,
        )
        .await
        .unwrap();

        // Write data
        let data = b"234";
        let range = 2..5;
        pusher.push(&range, data[..].into()).unwrap();
        pusher.flush().unwrap();

        // Verify file content
        let mut file_content = Vec::new();
        File::open(file_path)
            .unwrap()
            .read_to_end(&mut file_content)
            .unwrap();
        assert_eq!(file_content, b"\0\x00234\0\0\0\0\0");
    }
}
