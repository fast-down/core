use crate::{ProgressEntry, ProgressListener, Pusher};
use bytes::Bytes;
use std::{
    fs::File,
    io::{Seek, Write},
};
use tokio::io::SeekFrom;

/// File pusher using a raw `std::fs::File` with random-access writes and **no**
/// userspace write buffer.
///
/// Every [`push`](Pusher::push) seeks to `range.start` and writes directly to the
/// OS. It is intended to be wrapped by a buffering decorator such as
/// [`crate::BufWriterPusher`] when syscall batching is desired, or used directly
/// when writes are already large/sequential.
///
/// The `sync_all` flag controls whether `fsync` is called on
/// [`flush`](Pusher::flush). A bare `std::fs::File` has no userspace buffer, so
/// `flush` itself performs no copying — it only issues `fsync` when requested.
pub struct StdFilePusher {
    file: File,
    p: u64,
    sync_all: bool,
    listener: Option<ProgressListener>,
}

impl std::fmt::Debug for StdFilePusher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StdFilePusher")
            .field("p", &self.p)
            .field("sync_all", &self.sync_all)
            .finish_non_exhaustive()
    }
}

impl StdFilePusher {
    /// # Errors
    /// Returns an error if `fs::set_len` fails.
    pub async fn new(file: tokio::fs::File, size: u64, sync_all: bool) -> std::io::Result<Self> {
        file.set_len(size).await?;
        Ok(Self {
            file: file.into_std().await,
            p: 0,
            sync_all,
            listener: None,
        })
    }

    /// # Errors
    /// Returns an error if `Seek`, `Write`, or `WriteZero` occurs.
    pub fn write_at(&mut self, start: u64, mut bytes: &[u8]) -> std::io::Result<()> {
        if self.p != start {
            if let Err(e) = self.file.seek(SeekFrom::Start(start)) {
                self.p = u64::MAX;
                return Err(e);
            }
            self.p = start;
        }
        while !bytes.is_empty() {
            match self.file.write(bytes) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "failed to write any data",
                    ));
                }
                Ok(n) => {
                    let old = self.p;
                    self.p += n as u64;
                    if let Some(l) = &mut self.listener {
                        l(old..self.p);
                    }
                    bytes = &bytes[n..];
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.p = u64::MAX;
                    return Err(e);
                }
            }
        }
        Ok(())
    }
}

impl Pusher for StdFilePusher {
    type Error = std::io::Error;

    fn set_listener(&mut self, cb: ProgressListener) {
        self.listener = Some(cb);
    }

    fn push(&mut self, range: &ProgressEntry, bytes: Bytes) -> Result<(), (Self::Error, Bytes)> {
        if bytes.is_empty() {
            return Ok(());
        }
        let start = range.start;
        if let Err(e) = self.write_at(start, &bytes) {
            #[allow(clippy::cast_possible_truncation)]
            let written_len = if self.p >= start && self.p <= start + bytes.len() as u64 {
                (self.p - start) as usize
            } else {
                0
            };
            let remaining_bytes = if written_len < bytes.len() {
                bytes.slice(written_len..)
            } else {
                Bytes::new()
            };
            return Err((e, remaining_bytes));
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        if self.sync_all {
            self.file.sync_all()?;
        }
        Ok(())
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
        // Create a temp file for testing
        let temp_file = NamedTempFile::new().unwrap();
        let file_path = temp_file.path();

        // Initialize StdFilePusher with a file size of 10 bytes
        let mut pusher = StdFilePusher::new(temp_file.reopen().unwrap().into(), 10, false)
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
