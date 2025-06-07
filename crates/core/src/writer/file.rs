use super::{RandWriter, SeqWriter};
use crate::ProgressEntry;
use bytes::Bytes;
use std::{
    fs::File,
    io::{BufWriter, Write},
};

#[derive(Debug)]
pub struct SeqFileWriter {
    buffer: BufWriter<File>,
}

impl SeqFileWriter {
    pub fn new(file: File, write_buffer_size: usize) -> Self {
        Self {
            buffer: BufWriter::with_capacity(write_buffer_size, file),
        }
    }
}

impl SeqWriter for SeqFileWriter {
    fn write_sequentially(&mut self, bytes: Bytes) -> Result<(), std::io::Error> {
        self.buffer.write_all(&bytes)?;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), std::io::Error> {
        self.buffer.flush()?;
        Ok(())
    }
}

pub mod rand_file_writer_mmap {
    use super::*;
    use memmap2::MmapMut;

    #[derive(Debug)]
    pub struct RandFileWriter {
        mmap: MmapMut,
        downloaded: usize,
        write_buffer_size: usize,
    }

    impl RandFileWriter {
        pub fn new(
            file: File,
            size: u64,
            write_buffer_size: usize,
        ) -> Result<Self, std::io::Error> {
            file.set_len(size)?;
            Ok(Self {
                mmap: unsafe { MmapMut::map_mut(&file) }?,
                downloaded: 0,
                write_buffer_size,
            })
        }
    }

    impl RandWriter for RandFileWriter {
        fn write_randomly(
            &mut self,
            range: ProgressEntry,
            bytes: Bytes,
        ) -> Result<(), std::io::Error> {
            self.mmap[range.start as usize..range.end as usize].copy_from_slice(&bytes);
            self.downloaded += bytes.len();
            if self.downloaded >= self.write_buffer_size {
                self.mmap.flush()?;
                self.downloaded = 0;
            }
            Ok(())
        }

        fn flush(&mut self) -> Result<(), std::io::Error> {
            self.mmap.flush()?;
            Ok(())
        }
    }
}

pub mod rand_file_writer_std {
    use std::io::Seek;

    use super::*;

    #[derive(Debug)]
    pub struct RandFileWriter {
        buffer: BufWriter<File>,
        cache: Vec<(u64, Bytes)>,
        p: u64,
        cache_size: usize,
        write_buffer_size: usize,
    }

    impl RandFileWriter {
        pub fn new(
            file: File,
            size: u64,
            write_buffer_size: usize,
        ) -> Result<Self, std::io::Error> {
            file.set_len(size)?;
            Ok(Self {
                buffer: BufWriter::with_capacity(write_buffer_size, file),
                cache: Vec::new(),
                p: 0,
                cache_size: 0,
                write_buffer_size,
            })
        }
    }

    impl RandWriter for RandFileWriter {
        fn write_randomly(
            &mut self,
            range: ProgressEntry,
            bytes: Bytes,
        ) -> Result<(), std::io::Error> {
            let pos = self.cache.partition_point(|(i, _)| i < &range.start);
            self.cache_size += bytes.len();
            self.cache.insert(pos, (range.start, bytes));
            if self.cache_size >= self.write_buffer_size {
                self.flush()?;
            }
            Ok(())
        }

        fn flush(&mut self) -> Result<(), std::io::Error> {
            for (start, bytes) in self.cache.drain(..) {
                let len = bytes.len();
                self.cache_size -= len;
                if start != self.p {
                    self.buffer.seek(std::io::SeekFrom::Start(start))?;
                    self.p = start;
                }
                self.buffer.write_all(&bytes)?;
                self.p += len as u64;
            }
            self.buffer.flush()?;
            Ok(())
        }
    }
}

#[cfg(test)]
#[cfg(feature = "file")]
mod tests {
    use super::*;
    use crate::{RandWriter, SeqWriter};
    use bytes::Bytes;
    use std::{fs::File, io::Read};
    use tempfile::NamedTempFile;

    #[test]
    fn test_seq_file_writer() -> Result<(), std::io::Error> {
        // 创建一个临时文件用于测试
        let temp_file = NamedTempFile::new()?;
        let file_path = temp_file.path().to_path_buf();

        // 初始化 SeqFileWriter
        let mut writer = SeqFileWriter::new(temp_file.reopen()?, 1024);

        // 写入数据
        let data1 = Bytes::from("Hello, ");
        let data2 = Bytes::from("world!");
        writer.write_sequentially(data1)?;
        writer.write_sequentially(data2)?;
        writer.flush()?;

        // 验证文件内容
        let mut file_content = Vec::new();
        File::open(&file_path)?.read_to_end(&mut file_content)?;
        assert_eq!(file_content, b"Hello, world!");

        Ok(())
    }

    #[test]
    fn test_rand_file_writer() -> Result<(), std::io::Error> {
        // 创建一个临时文件用于测试
        let temp_file = NamedTempFile::new()?;
        let file_path = temp_file.path().to_path_buf();

        // 初始化 RandFileWriter，假设文件大小为 10 字节
        let mut writer =
            rand_file_writer_mmap::RandFileWriter::new(temp_file.reopen()?, 10, 8 * 1024 * 1024)?;

        // 写入数据
        let data = Bytes::from("234");
        let range = 2..5;
        writer.write_randomly(range, data)?;
        writer.flush()?;

        // 验证文件内容
        let mut file_content = Vec::new();
        File::open(&file_path)?.read_to_end(&mut file_content)?;
        assert_eq!(file_content, b"\0\0234\0\0\0\0\0");

        Ok(())
    }
}
