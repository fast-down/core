use crate::{ProgressEntry, Pusher};
use bytes::Bytes;
use std::{
    fs::File,
    io::{BufWriter, Seek, Write},
};
use tokio::io::SeekFrom;

#[derive(Debug)]
pub struct StdFilePusher {
    buf: BufWriter<File>,
    p: u64,
    sync_all: bool,
}

impl StdFilePusher {
    /// # Errors
    /// Returns an error if `fs::set_len` fails.
    pub async fn new(
        file: tokio::fs::File,
        size: u64,
        buffer_size: usize,
        sync_all: bool,
    ) -> std::io::Result<Self> {
        file.set_len(size).await?;
        Ok(Self {
            buf: BufWriter::with_capacity(buffer_size, file.into_std().await),
            p: 0,
            sync_all,
        })
    }

    /// # Errors
    /// Returns an error if `Seek` or `Write` fails.
    pub fn write_at(&mut self, start: u64, mut bytes: &[u8]) -> std::io::Result<()> {
        if self.p != start {
            if let Err(e) = self.buf.seek(SeekFrom::Start(start)) {
                self.p = u64::MAX;
                return Err(e);
            }
            self.p = start;
        }
        while !bytes.is_empty() {
            match self.buf.write(bytes) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "failed to write any data",
                    ));
                }
                Ok(n) => {
                    self.p += n as u64;
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
    fn push(&mut self, range: &ProgressEntry, bytes: Bytes) -> Result<(), (Self::Error, Bytes)> {
        if bytes.is_empty() {
            return Ok(());
        }
        let start = range.start;
        match self.write_at(start, &bytes) {
            Ok(()) => Ok(()),
            Err(e) => {
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
                Err((e, remaining_bytes))
            }
        }
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.buf.flush()?;
        if self.sync_all {
            self.buf.get_ref().sync_all()?;
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

        // Initialize RandFilePusher with a file size of 10 bytes
        let mut pusher =
            StdFilePusher::new(temp_file.reopen().unwrap().into(), 10, 8 * 1024, false)
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
