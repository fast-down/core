use super::{auto, DownloadResult};
use crate::{Progress, RandFileWriter, SeqFileWriter};
use color_eyre::eyre::Result;
use reqwest::{blocking::Client, IntoUrl};
use std::{
    fs::{self, OpenOptions},
    io::ErrorKind,
    path::Path,
    time::Duration,
};

pub struct DownloadOptions {
    pub threads: usize,
    pub client: Client,
    pub can_fast_download: bool,
    pub download_buffer_size: usize,
    pub write_buffer_size: usize,
    pub download_chunks: Vec<Progress>,
    pub retry_gap: Duration,
    pub file_size: usize,
}

pub fn download_file(
    url: impl IntoUrl,
    save_path: &Path,
    options: DownloadOptions,
) -> Result<DownloadResult> {
    let save_folder = save_path.parent().unwrap();
    if let Err(e) = fs::create_dir_all(save_folder) {
        if e.kind() != ErrorKind::AlreadyExists {
            return Err(e.into());
        }
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&save_path)?;
    let seq_file_writer = SeqFileWriter::new(file.try_clone()?, options.write_buffer_size)?;
    let rand_file_writer = RandFileWriter::new(file, options.file_size)?;
    auto::download(
        url,
        seq_file_writer.clone(),
        rand_file_writer.clone(),
        auto::DownloadOptions {
            threads: options.threads,
            client: options.client,
            can_fast_download: options.can_fast_download,
            download_buffer_size: options.download_buffer_size,
            download_chunks: options.download_chunks,
            retry_gap: options.retry_gap,
            file_size: options.file_size,
        },
    )
}

#[cfg(test)]
#[cfg(feature = "file")]
mod tests {
    use super::*;
    use crate::{Flush, RandWriter, SeqWriter};
    use bytes::Bytes;
    use std::{fs::File, io::Read};
    use tempfile::NamedTempFile;

    #[test]
    fn test_seq_file_writer() -> Result<()> {
        // 创建一个临时文件用于测试
        let temp_file = NamedTempFile::new()?;
        let file_path = temp_file.path().to_path_buf();

        // 初始化 SeqFileWriter
        let mut writer = SeqFileWriter::new(temp_file.reopen()?, 1024)?;

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
    fn test_rand_file_writer() -> Result<()> {
        // 创建一个临时文件用于测试
        let temp_file = NamedTempFile::new()?;
        let file_path = temp_file.path().to_path_buf();

        // 初始化 RandFileWriter，假设文件大小为 10 字节
        let mut writer = RandFileWriter::new(temp_file.reopen()?, 10)?;

        // 写入数据
        let data = Bytes::from("012345");
        let range = vec![2..5, 6..7, 8..10];
        writer.write_randomly(range, data)?;
        writer.flush()?;

        // 验证文件内容
        let mut file_content = Vec::new();
        File::open(&file_path)?.read_to_end(&mut file_content)?;
        assert_eq!(file_content, b"\0\0012\03\045");

        Ok(())
    }
}
