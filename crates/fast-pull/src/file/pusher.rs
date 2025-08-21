extern crate std;

use crate::{Pusher, SliceOrBytes, WriteStream};
use alloc::sync::Arc;
use bytes::Bytes;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use mmap_io::{MemoryMappedFile, MmapIoError, MmapMode, flush::FlushPolicy};
use std::{path::Path, vec::Vec};
use thiserror::Error;
use tokio::{
    fs::{File, OpenOptions},
    io::{self, AsyncSeekExt, AsyncWriteExt, BufWriter, SeekFrom},
};

#[derive(Error, Debug)]
pub enum FilePusherError {
    #[error(transparent)]
    MmapIo(#[from] MmapIoError),
    #[error(transparent)]
    TokioIo(#[from] io::Error),
}

#[derive(Debug)]
pub struct SeqFilePusher {
    buffer: BufWriter<File>,
}
impl SeqFilePusher {
    pub fn new(file: File, buffer_size: usize) -> Self {
        Self {
            buffer: BufWriter::with_capacity(buffer_size, file),
        }
    }
}

impl WriteStream for SeqFilePusher {
    type Error = FilePusherError;
    async fn write(&mut self, data: SliceOrBytes<'_>) -> Result<(), Self::Error> {
        Ok(self.buffer.write_all(&data).await?)
    }
    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(self.buffer.flush().await?)
    }
}

#[derive(Debug)]
struct MmapPusherShared {
    threshold: usize,
    current: AtomicUsize,
}

impl MmapPusherShared {
    fn new(threshold: usize) -> Self {
        Self {
            threshold,
            current: AtomicUsize::new(0),
        }
    }
}

#[derive(Debug)]
pub struct MmapFilePusher {
    mmap: MemoryMappedFile,
    shared: Arc<MmapPusherShared>,
}

impl MmapFilePusher {
    pub async fn new(
        path: impl AsRef<Path>,
        size: u64,
        buffer_size: usize,
    ) -> Result<Self, FilePusherError> {
        let mmap_builder = MemoryMappedFile::builder(&path)
            .mode(MmapMode::ReadWrite)
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
            shared: Arc::new(MmapPusherShared::new(buffer_size)),
        })
    }
}

struct MmapFilePushStream {
    mmap: MemoryMappedFile,
    shared: Arc<MmapPusherShared>,
    start_point: u64,
}

impl Pusher for MmapFilePusher {
    type Error = FilePusherError;
    type StreamError = FilePusherError;

    async fn init_write(
        &self,
        start_point: u64,
    ) -> Result<impl WriteStream<Error = Self::StreamError>, Self::Error> {
        Ok(MmapFilePushStream {
            mmap: self.mmap.clone(),
            shared: self.shared.clone(),
            start_point,
        })
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(self.mmap.flush_async().await?)
    }
}

impl WriteStream for MmapFilePushStream {
    type Error = FilePusherError;
    async fn write(&mut self, data: SliceOrBytes<'_>) -> Result<(), Self::Error> {
        self.mmap
            .as_slice_mut(self.start_point, data.len() as u64)?
            .as_mut()
            .copy_from_slice(&data);
        self.start_point += data.len() as u64;
        let current = self.shared.current.fetch_add(data.len(), Ordering::SeqCst) + data.len();
        if current >= self.shared.threshold {
            self.mmap.flush_async().await?;
            self.shared.current.store(0, Ordering::Relaxed);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct FilePusherStd(Arc<StdPusherShared>);
#[derive(Debug)]
struct StdPusherShared {
    buffer: tokio::sync::Mutex<BufWriter<File>>,
    cache: spin::Mutex<Vec<(u64, Bytes)>>,
    p: AtomicU64,
    cache_size: AtomicUsize,
    threshold: usize,
}

struct StdFilePushStream {
    shared: Arc<StdPusherShared>,
    start_point: u64,
}

impl FilePusherStd {
    pub async fn new(file: File, size: u64, buffer_size: usize) -> Result<Self, FilePusherError> {
        file.set_len(size).await?;
        Ok(Self(Arc::new(StdPusherShared {
            buffer: tokio::sync::Mutex::new(BufWriter::with_capacity(buffer_size, file)),
            cache: spin::Mutex::new(Vec::new()),
            p: AtomicU64::new(0),
            cache_size: AtomicUsize::new(0),
            threshold: buffer_size,
        })))
    }
}

impl StdPusherShared {
    async fn flush_with_cache(
        &self,
        mut cache_guard: spin::MutexGuard<'_, Vec<(u64, Bytes)>>,
    ) -> Result<(), FilePusherError> {
        let mut guard = self.buffer.lock().await;
        let mut p = self.p.load(Ordering::Acquire);
        for (start, bytes) in cache_guard.drain(..) {
            let len = bytes.len();
            self.cache_size.fetch_sub(len, Ordering::SeqCst);
            if start != p {
                guard.seek(SeekFrom::Start(start)).await?;
                p = start;
            }
            guard.write_all(&bytes).await?;
            p += len as u64;
        }
        guard.flush().await?;
        self.p.store(p, Ordering::Release);
        Ok(())
    }
}
impl Pusher for FilePusherStd {
    type Error = FilePusherError;
    type StreamError = FilePusherError;

    async fn init_write(
        &self,
        start_point: u64,
    ) -> Result<impl WriteStream<Error = Self::StreamError>, Self::Error> {
        Ok(StdFilePushStream {
            shared: self.0.clone(),
            start_point,
        })
    }

    fn flush(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.0.flush_with_cache(self.0.cache.lock())
    }
}
impl WriteStream for StdFilePushStream {
    type Error = FilePusherError;
    async fn write(&mut self, chunk: SliceOrBytes<'_>) -> Result<(), Self::Error> {
        let mut cache_guard = self.shared.cache.lock();
        let pos = cache_guard.partition_point(|(i, _)| i < &self.start_point);
        let size = self
            .shared
            .cache_size
            .fetch_add(chunk.len(), Ordering::SeqCst)
            + chunk.len();
        cache_guard.insert(pos, (self.start_point, chunk.into_bytes()));
        if size >= self.shared.threshold {
            self.shared.flush_with_cache(cache_guard).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn test_seq_file_pusher() {
        // 创建一个临时文件用于测试
        let temp_file = NamedTempFile::new().unwrap();
        let file_path = temp_file.path().to_path_buf();

        // 初始化 SeqFilePusher
        let mut pusher = SeqFilePusher::new(temp_file.reopen().unwrap().into(), 1024);

        // 写入数据
        pusher.write(b"Hello, "[..].into()).await.unwrap();
        pusher.write(b"world!"[..].into()).await.unwrap();
        pusher.flush().await.unwrap();

        // 验证文件内容
        let mut file_content = Vec::new();
        File::open(&file_path)
            .await
            .unwrap()
            .read_to_end(&mut file_content)
            .await
            .unwrap();
        assert_eq!(file_content, b"Hello, world!");
    }

    #[tokio::test]
    async fn test_rand_file_pusher() {
        // 创建一个临时文件用于测试
        let temp_file = NamedTempFile::new().unwrap();
        let file_path = temp_file.path();

        // 初始化 RandFilePusher，假设文件大小为 10 字节
        let pusher = MmapFilePusher::new(file_path, 10, 8 * 1024 * 1024)
            .await
            .unwrap();

        // 写入数据
        let mut writer = pusher.init_write(2).await.unwrap();
        writer.write(b"234"[..].into()).await.unwrap();
        writer.flush().await.unwrap();

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
