extern crate std;

use super::read_response::read_response;
use super::writer::DownloadWriter;
use crate::Event;
use bytes::BytesMut;
use color_eyre::eyre::Result;
use core::time::Duration;
use reqwest::{blocking::Client, IntoUrl};
use std::thread::{self, JoinHandle};

pub struct DownloadOptions {
    pub client: Client,
    pub get_chunk_size: usize,
    pub retry_gap: Duration,
}

pub fn download<Writer: DownloadWriter + 'static>(
    url: impl IntoUrl,
    writer: Writer,
    options: DownloadOptions,
) -> Result<(crossbeam_channel::Receiver<Event>, JoinHandle<()>)> {
    let url = url.into_url()?;
    let (tx, rx) = crossbeam_channel::unbounded();
    let tx_clone = tx.clone();
    let handle = thread::spawn(move || {
        let mut downloaded: usize = 0;
        let mut response = loop {
            tx_clone.send(Event::Connecting(0)).unwrap();
            match options.client.get(url.clone()).send() {
                Ok(response) => break response,
                Err(e) => tx_clone.send(Event::ConnectError(0, e.into())).unwrap(),
            }
            thread::sleep(options.retry_gap);
        };
        tx_clone.send(Event::Downloading(0)).unwrap();
        // dbg!("Response: {:?}", response);
        // dbg!("@status: {:?}", response.status());
        // dbg!("@length: {:?}", response.headers().get("content-length").unwrap());
        let mut buffer = BytesMut::with_capacity(options.get_chunk_size);
        loop {
            let len = read_response(&mut response, &mut buffer, options.retry_gap, |err| {
                tx_clone.send(Event::DownloadError(0, err.into())).unwrap();
            });
            // dbg!("@read: {:?}", len);
            if len == 0 {
                break;
            }
            let span = downloaded..(downloaded + len);
            tx_clone
                .send(Event::DownloadProgress(span.clone()))
                .unwrap();

            // SAFETY: single thread
            match unsafe {
                writer.write_part_unchecked(span.clone(), buffer.clone().split_to(len).freeze())
            } {
                Ok(_) => {
                    tx_clone.send(Event::WriteProgress(span)).unwrap();
                }
                Err(e) => tx_clone.send(Event::DownloadError(0, e.into())).unwrap(),
            }

            downloaded += len;
        }
        tx_clone.send(Event::Finished(0)).unwrap();
    });
    Ok((rx, handle))
}

#[cfg(test)]
#[cfg(feature = "file")]
mod tests {
    use crate::total::Total;
    extern crate std;
    use super::*;
    use crate::core::file_writer::FileWriter;
    use std::fs::File;
    use std::vec::Vec;
    use std::{dbg, io::Read};
    use tempfile::NamedTempFile;

    #[test]
    fn test_downloads_small_file_correctly() {
        // 测试 9B 小文件
        let mock_body = b"test data";
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/")
            .with_status(200)
            .with_body(mock_body)
            .create();

        let temp_file = NamedTempFile::new().unwrap();
        let file = temp_file.reopen().unwrap();
        file.set_len(mock_body.len() as u64).unwrap();

        let client = Client::new();
        let (rx, handle) = download(
            server.url(),
            FileWriter::new::<4>(file).unwrap(),
            DownloadOptions {
                client,
                get_chunk_size: 8 * 1024,
                retry_gap: Duration::from_secs(1),
            },
        )
        .unwrap();

        let progress_events: Vec<_> = rx.iter().collect();
        dbg!(&progress_events);
        handle.join().unwrap();

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
                .sum::<usize>(),
            mock_body.len()
        );
        assert_eq!(
            progress_events
                .iter()
                .map(|m| if let Event::WriteProgress(p) = m {
                    p.total()
                } else {
                    0
                })
                .sum::<usize>(),
            mock_body.len()
        );
        mock.assert();
    }

    #[test]
    fn test_downloads_empty_file_correctly() {
        // 测试空文件下载
        let mock_body = b"";
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/")
            .with_status(200)
            .with_body(mock_body)
            .create();

        let temp_file = NamedTempFile::new().unwrap();
        let file = temp_file.reopen().unwrap();

        let client = Client::new();
        let (rx, handle) = download(
            server.url(),
            FileWriter::new::<4>(file).unwrap(),
            DownloadOptions {
                client,
                get_chunk_size: 8 * 1024,
                retry_gap: Duration::from_secs(1),
            },
        )
        .unwrap();

        let progress_events: Vec<_> = rx.iter().collect();
        handle.join().unwrap();

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
                .sum::<usize>(),
            mock_body.len()
        );
        assert_eq!(
            progress_events
                .iter()
                .map(|m| if let Event::WriteProgress(p) = m {
                    p.total()
                } else {
                    0
                })
                .sum::<usize>(),
            mock_body.len()
        );
        mock.assert();
    }

    #[test]
    fn test_downloads_large_file_correctly() {
        let mock_body = [b'a'; 5000];
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/")
            .with_status(200)
            .with_body(&mock_body)
            .create();

        let temp_file = NamedTempFile::new().unwrap();
        let file = temp_file.reopen().unwrap();
        file.set_len(mock_body.len() as u64).unwrap();

        let client = Client::new();
        let (rx, handle) = download(
            server.url(),
            FileWriter::new::<4>(file).unwrap(),
            DownloadOptions {
                client,
                get_chunk_size: 8 * 1024,
                retry_gap: Duration::from_secs(1),
            },
        )
        .unwrap();

        let progress_events: Vec<_> = rx.iter().collect();
        handle.join().unwrap();

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
                .sum::<usize>(),
            mock_body.len()
        );
        assert_eq!(
            progress_events
                .iter()
                .map(|m| if let Event::WriteProgress(p) = m {
                    p.total()
                } else {
                    0
                })
                .sum::<usize>(),
            mock_body.len()
        );
        mock.assert();
    }

    #[test]
    fn test_downloads_exact_buffer_size_file() {
        let mock_body = vec![b'x'; 4096];
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/")
            .with_status(200)
            .with_body(&mock_body)
            .create();

        let temp_file = NamedTempFile::new().unwrap();
        let file = temp_file.reopen().unwrap();

        file.set_len(mock_body.len() as u64).unwrap();

        let client = Client::new();
        let (rx, handle) = download(
            server.url(),
            FileWriter::new::<4>(file).unwrap(),
            DownloadOptions {
                client,
                get_chunk_size: 8 * 1024,
                retry_gap: Duration::from_secs(1),
            },
        )
        .unwrap();

        let progress_events: Vec<_> = rx.iter().collect();
        handle.join().unwrap();

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
                .sum::<usize>(),
            mock_body.len()
        );
        assert_eq!(
            progress_events
                .iter()
                .map(|m| if let Event::WriteProgress(p) = m {
                    p.total()
                } else {
                    0
                })
                .sum::<usize>(),
            mock_body.len()
        );
        mock.assert();
    }
}
