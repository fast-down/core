use crate::{ProgressEntry, Pusher};
use bytes::Bytes;
use memmap2::MmapMut;

/// File pusher using memory-mapped I/O for zero-copy writes.
///
/// Delegates writes to the OS via `MmapMut`. The file size is fixed at
/// construction time via `file.set_len(size)`. On [`flush`](Pusher::flush),
/// if `sync_all` is true an `fsync` is performed; otherwise an async flush
/// is issued.
#[derive(Debug)]
pub struct MmapFilePusher {
    mmap: MmapMut,
    sync_all: bool,
}
impl MmapFilePusher {
    /// # Errors
    /// 1. Returns an error if `fs::set_len` fails.
    /// 2. Returns an error if `MmapMut::map_mut` fails.
    pub async fn new(file: tokio::fs::File, size: u64, sync_all: bool) -> std::io::Result<Self> {
        file.set_len(size).await?;
        let mmap = unsafe { MmapMut::map_mut(&file)? };
        Ok(Self { mmap, sync_all })
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
        if self.sync_all {
            self.mmap.flush()
        } else {
            self.mmap.flush_async()
        }
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
        // Create a temp file for testing
        let temp_file = NamedTempFile::new().unwrap();
        let file_path = temp_file.path();

        // Initialize MmapFilePusher with a file size of 10 bytes
        let mut pusher = MmapFilePusher::new(temp_file.reopen().unwrap().into(), 10, false)
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
