use crate::Event;
use color_eyre::eyre::Result;
use reqwest::blocking::Client;
extern crate std;
use std::{
    fs::File,
    io::{BufWriter, Read, Write},
    thread::{self, JoinHandle},
};
extern crate alloc;
use alloc::string::String;

pub struct DownloadSingleThreadOptions {
    pub url: String,
    pub file: File,
    pub client: Client,
    pub get_chunk_size: usize,
    pub write_chunk_size: usize,
}

pub fn download_single_thread(
    options: DownloadSingleThreadOptions,
) -> Result<(crossbeam_channel::Receiver<Event>, JoinHandle<()>)> {
    let (tx, rx) = crossbeam_channel::unbounded();
    let (tx_write, rx_write) = crossbeam_channel::unbounded();
    let tx_clone = tx.clone();
    thread::spawn(move || {
        let mut downloaded = 0;
        tx_clone.send(Event::Connecting(0)).unwrap();
        let mut response = loop {
            match options.client.get(&options.url).send() {
                Ok(response) => break response,
                Err(e) => tx_clone.send(Event::ConnectError(0, e.into())).unwrap(),
            }
        };
        tx_clone.send(Event::Downloading(0)).unwrap();
        let mut buffer = vec![0u8; options.get_chunk_size];
        loop {
            let len = loop {
                match response.read(&mut buffer) {
                    Ok(len) => break len,
                    Err(e) => tx_clone.send(Event::DownloadError(0, e.into())).unwrap(),
                }
            };
            if len == 0 {
                break;
            }
            tx_clone
                .send(Event::DownloadProgress(downloaded..(downloaded + len)))
                .unwrap();
            tx_write.send((downloaded, buffer[..len].to_vec())).unwrap();
            downloaded += len;
        }
        tx_clone.send(Event::Finished(0)).unwrap();
    });
    let handle = thread::spawn(move || {
        let mut writer = BufWriter::with_capacity(options.write_chunk_size, options.file);
        for (start, bytes) in rx_write {
            loop {
                match writer.write_all(&bytes) {
                    Ok(_) => break,
                    Err(e) => tx.send(Event::WriteError(e.into())).unwrap(),
                }
            }
            tx.send(Event::WriteProgress(start..(start + bytes.len())))
                .unwrap();
        }
        loop {
            match writer.flush() {
                Ok(_) => break,
                Err(e) => tx.send(Event::WriteError(e.into())).unwrap(),
            }
        }
    });
    Ok((rx, handle))
}

#[cfg(test)]
mod tests {
    use crate::total::Total;
    extern crate alloc;
    extern crate std;
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;
    use std::io::Read;
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

        let client = Client::new();
        let (rx, handle) = download_single_thread(DownloadSingleThreadOptions {
            url: server.url(),
            file,
            client,
            get_chunk_size: 8 * 1024,
            write_chunk_size: 8 * 1024 * 1024,
        })
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
        let (rx, handle) = download_single_thread(DownloadSingleThreadOptions {
            url: server.url(),
            file,
            client,
            get_chunk_size: 8 * 1024,
            write_chunk_size: 8 * 1024 * 1024,
        })
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

        let client = Client::new();
        let (rx, handle) = download_single_thread(DownloadSingleThreadOptions {
            url: server.url(),
            file,
            client,
            get_chunk_size: 8 * 1024,
            write_chunk_size: 8 * 1024 * 1024,
        })
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

        let client = Client::new();
        let (rx, handle) = download_single_thread(DownloadSingleThreadOptions {
            url: server.url(),
            file,
            client,
            get_chunk_size: 8 * 1024,
            write_chunk_size: 8 * 1024 * 1024,
        })
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
