use super::DownloadResult;
use crate::{Event, Progress, SeqWriter};
use bytes::{Bytes, BytesMut};
use color_eyre::eyre::Result;
use reqwest::{blocking::Client, IntoUrl};
use std::{
    io::{ErrorKind, Read},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

pub struct DownloadOptions {
    pub client: Client,
    pub retry_gap: Duration,
    pub download_buffer_size: usize,
}

pub fn download(
    url: impl IntoUrl,
    mut writer: impl SeqWriter + 'static,
    options: DownloadOptions,
) -> Result<DownloadResult> {
    let url = url.into_url()?;
    let (tx, event_chain) = crossbeam_channel::unbounded();
    let (tx_write, rx_write) = crossbeam_channel::unbounded::<(Progress, Bytes)>();
    let tx_clone = tx.clone();
    let handle = thread::spawn(move || {
        for (spin, data) in rx_write {
            loop {
                match writer.write_sequentially(data.clone()) {
                    Ok(_) => break,
                    Err(e) => tx_clone.send(Event::WriteError(e.into())).unwrap(),
                }
                thread::sleep(options.retry_gap);
            }
            tx_clone.send(Event::WriteProgress(spin)).unwrap();
        }
        loop {
            match writer.flush() {
                Ok(_) => break,
                Err(e) => tx_clone.send(Event::WriteError(e.into())).unwrap(),
            };
            thread::sleep(options.retry_gap);
        }
    });
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();
    thread::spawn(move || {
        let mut downloaded: u64 = 0;
        let mut response = loop {
            if !running.load(Ordering::Relaxed) {
                tx.send(Event::Abort(0)).unwrap();
                return;
            }
            tx.send(Event::Connecting(0)).unwrap();
            match options.client.get(url.clone()).send() {
                Ok(response) => break response,
                Err(e) => tx.send(Event::ConnectError(0, e.into())).unwrap(),
            }
            thread::sleep(options.retry_gap);
        };
        tx.send(Event::Downloading(0)).unwrap();
        let mut buffer = BytesMut::with_capacity(options.download_buffer_size);
        unsafe { buffer.set_len(options.download_buffer_size) };
        loop {
            let len = loop {
                if !running.load(Ordering::Relaxed) {
                    tx.send(Event::Abort(0)).unwrap();
                    return;
                }
                match response.read(&mut buffer) {
                    Ok(len) => break len,
                    Err(e) => {
                        let kind = e.kind();
                        tx.send(Event::DownloadError(0, e.into())).unwrap();
                        if kind != ErrorKind::Interrupted {
                            return;
                        }
                    }
                };
                thread::sleep(options.retry_gap);
            };
            if len == 0 {
                break;
            }
            let span = downloaded..(downloaded + len as u64);
            tx.send(Event::DownloadProgress(span.clone())).unwrap();
            tx_write
                .send((span, buffer.clone().split_to(len).freeze()))
                .unwrap();
            downloaded += len as u64;
        }
        tx.send(Event::Finished(0)).unwrap();
    });
    Ok(DownloadResult::new(
        event_chain,
        handle,
        Box::new(move || {
            running_clone.store(false, Ordering::Relaxed);
        }),
    ))
}

#[cfg(test)]
#[cfg(feature = "file")]
mod tests {
    use super::*;
    use crate::core::file_writer::SeqFileWriter;
    use crate::Total;
    use std::fs::File;
    use tempfile::NamedTempFile;

    fn build_mock_data(size: usize) -> Vec<u8> {
        (0..size).map(|i| (i % 256) as u8).collect()
    }

    #[test]
    fn test_downloads_small_file_correctly() {
        // 测试 9B 小文件
        let mock_body = b"test data";
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/small")
            .with_status(200)
            .with_body(mock_body)
            .create();

        let temp_file = NamedTempFile::new().unwrap();
        let file = temp_file.reopen().unwrap();

        let client = Client::new();
        let result = download(
            format!("{}/small", server.url()),
            SeqFileWriter::new(file, 8 * 1024 * 1024).unwrap(),
            DownloadOptions {
                client,
                retry_gap: Duration::from_secs(1),
                download_buffer_size: 8 * 1024,
            },
        )
        .unwrap();

        let progress_events: Vec<_> = result.clone().collect();
        dbg!(&progress_events);
        result.join().unwrap();

        let mut file_content = Vec::new();
        File::open(temp_file.path())
            .unwrap()
            .read_to_end(&mut file_content)
            .unwrap();
        assert_eq!(file_content, mock_body);

        assert_eq!(
            progress_events
                .iter()
                .map(|m| if let Event::DownloadProgress(p) = m {
                    p.total()
                } else {
                    0
                })
                .sum::<u64>(),
            mock_body.len() as u64
        );
        assert_eq!(
            progress_events
                .iter()
                .map(|m| if let Event::WriteProgress(p) = m {
                    p.total()
                } else {
                    0
                })
                .sum::<u64>(),
            mock_body.len() as u64
        );
        mock.assert();
    }

    #[test]
    fn test_downloads_empty_file_correctly() {
        // 测试空文件下载
        let mock_body = b"";
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/empty")
            .with_status(200)
            .with_body(mock_body)
            .create();

        let temp_file = NamedTempFile::new().unwrap();
        let file = temp_file.reopen().unwrap();

        let client = Client::new();
        let result = download(
            format!("{}/empty", server.url()),
            SeqFileWriter::new(file, 8 * 1024 * 1024).unwrap(),
            DownloadOptions {
                client,
                retry_gap: Duration::from_secs(1),
                download_buffer_size: 8 * 1024,
            },
        )
        .unwrap();

        let progress_events: Vec<_> = result.clone().collect();
        result.join().unwrap();

        // 验证空文件
        let mut file_content = Vec::new();
        File::open(temp_file.path())
            .unwrap()
            .read_to_end(&mut file_content)
            .unwrap();
        assert!(file_content.is_empty());

        // 验证无进度事件
        assert_eq!(
            progress_events
                .iter()
                .map(|m| if let Event::DownloadProgress(p) = m {
                    p.total()
                } else {
                    0
                })
                .sum::<u64>(),
            mock_body.len() as u64
        );
        assert_eq!(
            progress_events
                .iter()
                .map(|m| if let Event::WriteProgress(p) = m {
                    p.total()
                } else {
                    0
                })
                .sum::<u64>(),
            mock_body.len() as u64
        );
        mock.assert();
    }

    #[test]
    fn test_downloads_large_file_correctly() {
        let mock_body = build_mock_data(5000);
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/large")
            .with_status(200)
            .with_body(&mock_body)
            .create();

        let temp_file = NamedTempFile::new().unwrap();
        let file = temp_file.reopen().unwrap();

        let client = Client::new();
        let result = download(
            format!("{}/large", server.url()),
            SeqFileWriter::new(file, 8 * 1024 * 1024).unwrap(),
            DownloadOptions {
                client,
                retry_gap: Duration::from_secs(1),
                download_buffer_size: 8 * 1024,
            },
        )
        .unwrap();

        let progress_events: Vec<_> = result.clone().collect();
        result.join().unwrap();

        // 验证文件内容
        let mut file_content = Vec::new();
        File::open(temp_file.path())
            .unwrap()
            .read_to_end(&mut file_content)
            .unwrap();
        assert_eq!(file_content, mock_body);

        // 验证进度事件总和
        assert_eq!(
            progress_events
                .iter()
                .map(|m| if let Event::DownloadProgress(p) = m {
                    p.total()
                } else {
                    0
                })
                .sum::<u64>(),
            mock_body.len() as u64
        );
        assert_eq!(
            progress_events
                .iter()
                .map(|m| if let Event::WriteProgress(p) = m {
                    p.total()
                } else {
                    0
                })
                .sum::<u64>(),
            mock_body.len() as u64
        );
        mock.assert();
    }

    #[test]
    fn test_downloads_exact_buffer_size_file() {
        let mock_body = build_mock_data(4096);
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/exact_buffer_size_file")
            .with_status(200)
            .with_body(&mock_body)
            .create();

        let temp_file = NamedTempFile::new().unwrap();
        let file = temp_file.reopen().unwrap();

        let client = Client::new();
        let result = download(
            format!("{}/exact_buffer_size_file", server.url()),
            SeqFileWriter::new(file, 8 * 1024 * 1024).unwrap(),
            DownloadOptions {
                client,
                retry_gap: Duration::from_secs(1),
                download_buffer_size: 8 * 1024,
            },
        )
        .unwrap();

        let progress_events: Vec<_> = result.clone().collect();
        result.join().unwrap();

        // 验证文件内容
        let mut file_content = Vec::new();
        File::open(temp_file.path())
            .unwrap()
            .read_to_end(&mut file_content)
            .unwrap();
        assert_eq!(file_content, mock_body);

        // 验证进度事件完整性
        assert_eq!(
            progress_events
                .iter()
                .map(|m| if let Event::DownloadProgress(p) = m {
                    p.total()
                } else {
                    0
                })
                .sum::<u64>(),
            mock_body.len() as u64
        );
        assert_eq!(
            progress_events
                .iter()
                .map(|m| if let Event::WriteProgress(p) = m {
                    p.total()
                } else {
                    0
                })
                .sum::<u64>(),
            mock_body.len() as u64
        );
        mock.assert();
    }
}
