//! Memory-mapped file pusher (feature `file`).

use crate::{ProgressEntry, ProgressListener, Pusher};
use bytes::Bytes;
use memmap2::MmapMut;

#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

/// File pusher using memory-mapped I/O for zero-copy writes.
///
/// Delegates writes to the OS via `MmapMut`. The file size is fixed at
/// construction time via `file.set_len(size)`. On [`flush`](Pusher::flush),
/// if `sync_all` is true an `fsync` is performed; otherwise an async flush
/// is issued.
pub struct MmapFilePusher {
    mmap: MmapMut,
    sync_all: bool,
    listener: Option<ProgressListener>,
}

impl std::fmt::Debug for MmapFilePusher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MmapFilePusher")
            .field("sync_all", &self.sync_all)
            .finish_non_exhaustive()
    }
}
impl MmapFilePusher {
    /// # Errors
    /// 1. Returns an error if `fs::set_len` fails.
    /// 2. Returns an error if `MmapMut::map_mut` fails.
    pub async fn new(file: &tokio::fs::File, size: u64, sync_all: bool) -> std::io::Result<Self> {
        file.set_len(size).await?;
        // `MmapMut::map_mut` performs a synchronous `mmap()` syscall, but it only
        // needs the raw fd/handle — it never takes ownership of the `File`. Moving
        // just the raw descriptor (always `Send`) into the blocking pool satisfies
        // `spawn_blocking`'s `'static` bound with zero extra `dup`/`CloneFile`.
        // (`RawHandle` is a `*mut c_void` and `!Send` on Windows, so it is carried
        // as `usize` and reconstructed inside the blocking closure.)
        #[cfg(unix)]
        let raw = file.as_raw_fd();
        #[cfg(windows)]
        let raw = file.as_raw_handle() as usize;
        let mmap = tokio::task::spawn_blocking(move || unsafe {
            #[cfg(unix)]
            let desc = raw;
            #[cfg(windows)]
            let desc = raw as std::os::windows::io::RawHandle;
            MmapMut::map_mut(desc)
        })
        .await
        .map_err(std::io::Error::other)??;
        Ok(Self {
            mmap,
            sync_all,
            listener: None,
        })
    }
}
impl Pusher for MmapFilePusher {
    type Error = std::io::Error;

    fn set_listener(&mut self, cb: ProgressListener) {
        self.listener = Some(cb);
    }

    fn push(&mut self, range: &ProgressEntry, bytes: Bytes) -> Result<(), (Self::Error, Bytes)> {
        #[allow(clippy::cast_possible_truncation)]
        self.mmap[range.start as usize..range.end as usize].copy_from_slice(&bytes);
        if let Some(l) = &mut self.listener {
            l(range.clone());
        }
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
    use std::sync::{Arc, Mutex};
    use std::{fs::File, io::Read, vec::Vec};
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_rand_file_pusher() {
        // Create a temp file for testing
        let temp_file = NamedTempFile::new().unwrap();
        let file_path = temp_file.path();

        // Initialize MmapFilePusher with a file size of 10 bytes
        let mut pusher = MmapFilePusher::new(&temp_file.reopen().unwrap().into(), 10, false)
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

    #[tokio::test]
    async fn test_debug_and_sync_all_flush() {
        // Lines 25-29: `Debug` impl. Line 80: the `sync_all == true` branch of `flush`.
        let temp_file = NamedTempFile::new().unwrap();
        let file_path = temp_file.path().to_path_buf();
        let mut pusher = MmapFilePusher::new(&temp_file.reopen().unwrap().into(), 10, true)
            .await
            .unwrap();
        let _ = format!("{:?}", &pusher);
        pusher.push(&(2..5), b"234"[..].into()).unwrap();
        pusher.flush().unwrap();
        let mut file_content = Vec::new();
        File::open(&file_path)
            .unwrap()
            .read_to_end(&mut file_content)
            .unwrap();
        assert_eq!(file_content, b"\0\x00234\0\0\0\0\0");
    }

    #[tokio::test]
    async fn test_listener_invoked_on_push() {
        // Lines 66-68 (`set_listener`) and 73-75 (listener call inside `push`).
        let temp_file = NamedTempFile::new().unwrap();
        let mut pusher = MmapFilePusher::new(&temp_file.reopen().unwrap().into(), 10, false)
            .await
            .unwrap();
        let seen = Arc::new(Mutex::new(None::<ProgressEntry>));
        let seen2 = seen.clone();
        pusher.set_listener(Box::new(move |r| {
            *seen2.lock().unwrap() = Some(r);
        }));
        pusher.push(&(2..5), b"234"[..].into()).unwrap();
        assert_eq!(*seen.lock().unwrap(), Some(2..5));
    }
}
