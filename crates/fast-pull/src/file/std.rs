use crate::{ProgressEntry, Pusher};
use bytes::Bytes;
use std::{
    fs::File,
    io::{BufWriter, Seek, Write},
};
use tokio::io::SeekFrom;

#[derive(Debug)]
pub struct FilePusher {
    buf: BufWriter<File>,
    p: u64,
}

impl FilePusher {
    /// # Errors
    /// 当 `fs::set_len` 失败时返回错误。
    pub async fn new(
        file: tokio::fs::File,
        size: u64,
        buffer_size: usize,
    ) -> std::io::Result<Self> {
        file.set_len(size).await?;
        Ok(Self {
            buf: BufWriter::with_capacity(buffer_size, file.into_std().await),
            p: 0,
        })
    }

    /// # Errors
    /// 当 `Seek` 或 `Write` 失败时返回错误。
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

impl Pusher for FilePusher {
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
        self.buf.get_ref().sync_all()
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
        // 创建一个临时文件用于测试
        let temp_file = NamedTempFile::new().unwrap();
        let file_path = temp_file.path();

        // 初始化 RandFilePusher，假设文件大小为 10 字节
        let mut pusher = FilePusher::new(temp_file.reopen().unwrap().into(), 10, 8 * 1024)
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
