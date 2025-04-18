use crate::download_single_thread::DownloadSingleThreadOptions;
use crate::Event;
use crate::{display_progress, download_single_thread};
use color_eyre::{eyre::eyre, Result};
use fast_steal::{Spawn, TaskList};
use memmap2::MmapMut;
use reqwest::{blocking::Client, header, StatusCode};
extern crate alloc;
use alloc::format;
use alloc::{sync::Arc, vec};
use vec::Vec;
extern crate std;
use alloc::string::String;
use std::{
    fs::File,
    io::Read,
    thread::{self, JoinHandle},
};

pub struct DownloadMultiThreadsOptions {
    pub url: String,
    pub threads: usize,
    pub file_size: usize,
    pub file: File,
    pub client: Client,
    pub get_chunk_size: usize,
    pub write_chunk_size: usize,
}

pub fn download_multi_threads(
    options: DownloadMultiThreadsOptions,
) -> Result<(crossbeam_channel::Receiver<Event>, JoinHandle<()>)> {
    let download_chunk = vec![0..options.file_size];
    if download_chunk.is_empty() {
        return Err(eyre!("Download completed"));
    } else if options.threads < 2
        && download_chunk.len() == 1
        && download_chunk[0].start == 0
        && download_chunk[0].end == options.file_size
    {
        return download_single_thread::download_single_thread(DownloadSingleThreadOptions {
            url: options.url,
            file: options.file,
            client: options.client,
            get_chunk_size: options.get_chunk_size,
            write_chunk_size: options.write_chunk_size,
        });
    }
    let (tx, rx) = crossbeam_channel::unbounded();
    let (tx_write, rx_write) = crossbeam_channel::unbounded::<(usize, Vec<u8>)>();
    let tasks: Arc<TaskList> = Arc::new(download_chunk.into());
    let tasks_clone = tasks.clone();
    let tx_clone = tx.clone();
    let handle = thread::spawn(move || {
        let mut mmap = loop {
            match unsafe { MmapMut::map_mut(&options.file) } {
                Ok(mmap) => break mmap,
                Err(e) => tx_clone.send(Event::WriteError(e.into())).unwrap(),
            }
        };
        let mut downloaded = 0usize;
        for (start, data) in rx_write {
            let len = data.len();
            mmap[start..(start + len)].copy_from_slice(&data);
            downloaded += len;
            if downloaded >= options.write_chunk_size {
                if let Err(e) = mmap.flush() {
                    tx_clone.send(Event::WriteError(e.into())).unwrap()
                }
                downloaded = 0;
            }
            tx_clone
                .send(Event::WriteProgress(start..(start + len)))
                .unwrap();
        }
        loop {
            match mmap.flush() {
                Ok(_) => break,
                Err(e) => tx_clone.send(Event::WriteError(e.into())).unwrap(),
            }
        }
    });
    tasks.spawn(
        options.threads,
        |closure| thread::spawn(move || closure()),
        move |id, task, get_task| loop {
            let mut start = task.start();
            let end = task.end();
            if start >= end {
                if get_task() {
                    continue;
                }
                tx.send(Event::Finished(id)).unwrap();
                break;
            }
            let range_str = display_progress::display_progress(&tasks_clone.get_range(start..end));
            tx.send(Event::Connecting(id)).unwrap();
            let mut response = loop {
                match options
                    .client
                    .get(&options.url)
                    .header(header::RANGE, format!("bytes={}", range_str))
                    .send()
                {
                    Ok(response) => break response,
                    Err(e) => tx.send(Event::ConnectError(id, e.into())).unwrap(),
                }
            };
            tx.send(Event::Downloading(id)).unwrap();
            if response.status() != StatusCode::PARTIAL_CONTENT {
                panic!(
                    "Error: response code is {}, not 206\n{}",
                    response.status(),
                    range_str
                );
            }
            let mut buffer = vec![0u8; options.get_chunk_size];
            loop {
                let end = task.end();
                if start >= end {
                    break;
                }
                let remain = end - start;
                task.fetch_add_start(remain.min(options.get_chunk_size));
                let len = loop {
                    match response.read(&mut buffer) {
                        Ok(len) => break len,
                        Err(e) => tx.send(Event::DownloadError(id, e.into())).unwrap(),
                    }
                };
                let chunk_end = (start + len).min(task.end());
                let len = chunk_end - start;
                tx.send(Event::DownloadProgress(start..chunk_end)).unwrap();
                tx_write.send((start, buffer[..len].to_vec())).unwrap();
                start += len;
            }
        },
    );
    Ok((rx, handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Total;
    use std::io::Read;
    use tempfile::NamedTempFile;

    #[test]
    fn test_multi_thread_regular_download() {
        let mock_body = vec![b'a'; 3 * 1024];
        let mock_body_clone = mock_body.clone();
        let mut server = mockito::Server::new();
        server
            .mock("GET", "/")
            .with_status(206)
            .with_body_from_request(move |request| {
                if !request.has_header("Range") {
                    return mock_body_clone.clone();
                }
                let range = request.header("Range")[0];
                println!("range: {:?}", range);
                let mut parts = range
                    .to_str()
                    .unwrap()
                    .rsplit('=')
                    .next()
                    .unwrap()
                    .splitn(2, '-');
                let start = parts.next().unwrap().parse::<usize>().unwrap();
                let end = parts.next().unwrap().parse::<usize>().unwrap();
                mock_body_clone[start..=end].to_vec()
            })
            .create();

        let temp_file = NamedTempFile::new().unwrap();
        let file = temp_file.reopen().unwrap();
        file.set_len(mock_body.len() as u64).unwrap();

        let client = Client::new();
        let (rx, handle) = download_multi_threads(DownloadMultiThreadsOptions {
            client,
            url: server.url(),
            file,
            file_size: mock_body.len(),
            threads: 4,
            get_chunk_size: 8 * 1024,
            write_chunk_size: 8 * 1024 * 1024,
        })
        .unwrap();

        let progress_events: Vec<_> = rx.iter().collect();
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
    }
}
