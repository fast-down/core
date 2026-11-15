//! Memory-mapped file pusher (feature `file`).

use crate::{ProgressEntry, ProgressListener, Pusher};
use bytes::Bytes;
use memmap2::MmapMut;

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
        // Map from an independent handle owned by the blocking closure: the
        // caller's `file` can be dropped if the build future is cancelled while
        // the mapping task is still queued, and a descriptor passed by value
        // could then be closed (or reused for an unrelated file) before the
        // mapping runs.
        let file = file.try_clone().await?;
        let std_file = file.into_std().await;
        let mmap = tokio::task::spawn_blocking(move || unsafe {
            // SAFETY: the file is owned by this closure and stays open for the
            // call; the returned mapping holds its own reference to the object.
            MmapMut::map_mut(&std_file)
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
        if bytes.is_empty() {
            return Ok(());
        }
        let in_bounds = usize::try_from(range.start)
            .ok()
            .zip(usize::try_from(range.end).ok())
            .is_some_and(|(start, end)| {
                start <= end && end <= self.mmap.len() && end - start == bytes.len()
            });
        if !in_bounds {
            // Indexing the mapping would panic. A caller-supplied range is a
            // contract violation, not a transient write failure: the engine
            // retries a returned error, so this branch must stay unreachable for
            // ranges derived from the mapped size.
            return Err((
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "push range does not match the mapped file",
                ),
                bytes,
            ));
        }
        #[allow(clippy::cast_possible_truncation)]
        let (start, end) = (range.start as usize, range.end as usize);
        self.mmap[start..end].copy_from_slice(&bytes);
        if let Some(l) = &mut self.listener {
            l(range.clone());
        }
        Ok(())
    }
    /// Data is already in the shared mapping, visible to any other reader.
    ///
    /// Only `sync_all` forces the dirty pages out; otherwise this is a no-op so
    /// the writeback is left to the OS instead of blocking the download on
    /// `msync`/`FlushViewOfFile`. The mapping keeps the pages reachable until the
    /// pusher is dropped, so the data is never lost.
    fn flush(&mut self) -> Result<(), Self::Error> {
        if self.sync_all {
            self.mmap.flush()
        } else {
            Ok(())
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
        // The `Debug` impl and the `sync_all == true` branch of `flush`.
        let temp_file = NamedTempFile::new().unwrap();
        let file_path = temp_file.path().to_path_buf();
        let mut pusher = MmapFilePusher::new(&temp_file.reopen().unwrap().into(), 10, true)
            .await
            .unwrap();
        let _ = format!("{pusher:?}");
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
        // `set_listener` stores the callback and `push` invokes it.
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

    #[tokio::test]
    async fn test_mmap_pusher_rejects_a_range_outside_the_mapping() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut pusher = MmapFilePusher::new(&temp_file.reopen().unwrap().into(), 10, false)
            .await
            .unwrap();
        let (err, unwritten) = pusher
            .push(&(8..13), Bytes::from_static(b"abcde"))
            .expect_err("a range past the mapping must be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(&unwritten[..], b"abcde", "the caller keeps its bytes");

        // A range whose length disagrees with the payload is refused too.
        assert!(pusher.push(&(0..2), Bytes::from_static(b"abc")).is_err());
    }

    #[tokio::test]
    async fn test_mmap_pusher_zero_length_file() {
        // An empty download (size == 0) must still construct a usable
        // pusher. `MmapMut::map_mut` on a zero-length file returns EINVAL on Unix,
        // so this probes whether `new(0)` is a latent break for 0-byte resources.
        let temp_file = NamedTempFile::new().unwrap();
        let result = MmapFilePusher::new(&temp_file.reopen().unwrap().into(), 0, false).await;
        assert!(
            result.is_ok(),
            "zero-length download should construct an mmap pusher, got: {result:?}"
        );
    }
}
