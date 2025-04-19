extern crate alloc;
extern crate std;
use crate::display_progress;
use crate::Event;
use alloc::format;
use alloc::string::String;
use alloc::{sync::Arc, vec};
use color_eyre::eyre::eyre;
use color_eyre::Result;
use core::ops::Range;
use core::time::Duration;
use fast_steal::{Spawn, TaskList};
use reqwest::{blocking::Client, header, StatusCode};
use std::{
    fs::File,
    io::{BufWriter, Read, Seek, SeekFrom, Write},
    thread::{self, JoinHandle},
};
use vec::Vec;

pub struct DownloadMultiThreadsOptions {
    pub url: String,
    pub threads: usize,
    pub file: File,
    pub client: Client,
    pub get_chunk_size: usize,
    pub write_chunk_size: usize,
    pub download_chunks: Vec<Range<usize>>,
    pub retry_gap: Duration,
}

pub fn download_multi_threads(
    options: DownloadMultiThreadsOptions,
) -> Result<(crossbeam_channel::Receiver<Event>, JoinHandle<()>)> {
    let (tx, rx) = crossbeam_channel::unbounded();
    let (tx_write, rx_write) = crossbeam_channel::unbounded::<(usize, Vec<u8>)>();
    let tasks: Arc<TaskList> = Arc::new(options.download_chunks.into());
    let tasks_clone = tasks.clone();
    let tx_clone = tx.clone();
    let handle = thread::spawn(move || {
        let mut writer = BufWriter::with_capacity(options.write_chunk_size, options.file);
        for (start, data) in rx_write {
            loop {
                match writer.seek(SeekFrom::Start(start as u64)) {
                    Ok(_) => break,
                    Err(e) => tx_clone.send(Event::WriteError(e.into())).unwrap(),
                }
                thread::sleep(options.retry_gap);
            }
            loop {
                match writer.write_all(&data) {
                    Ok(_) => break,
                    Err(e) => tx_clone.send(Event::WriteError(e.into())).unwrap(),
                }
                thread::sleep(options.retry_gap);
            }
            tx_clone
                .send(Event::WriteProgress(start..(start + data.len())))
                .unwrap();
        }
        loop {
            match writer.flush() {
                Ok(_) => break,
                Err(e) => tx_clone.send(Event::WriteError(e.into())).unwrap(),
            }
            thread::sleep(options.retry_gap);
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
            let mut response = loop {
                tx.send(Event::Connecting(id)).unwrap();
                match options
                    .client
                    .get(&options.url)
                    .header(header::RANGE, format!("bytes={}", range_str))
                    .send()
                {
                    Ok(response) if response.status() == StatusCode::PARTIAL_CONTENT => {
                        break response
                    }
                    Ok(response) => tx
                        .send(Event::ConnectError(
                            id,
                            eyre!("Expect to get 206, but got {}", response.status()),
                        ))
                        .unwrap(),
                    Err(e) => tx.send(Event::ConnectError(id, e.into())).unwrap(),
                }
                thread::sleep(options.retry_gap);
            };
            tx.send(Event::Downloading(id)).unwrap();
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
                    thread::sleep(options.retry_gap);
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
    use crate::{MergeProgress, Progress, Total};
    use std::{io::Read, println};
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
            threads: 4,
            get_chunk_size: 8 * 1024,
            write_chunk_size: 8 * 1024 * 1024,
            download_chunks: vec![0..mock_body.len()],
            retry_gap: Duration::from_secs(1),
        })
        .unwrap();

        handle.join().unwrap();

        let mut file_content = Vec::new();
        File::open(temp_file.path())
            .unwrap()
            .read_to_end(&mut file_content)
            .unwrap();
        assert_eq!(file_content, mock_body);
        let mut download_progress: Vec<Progress> = Vec::new();
        let mut write_progress: Vec<Progress> = Vec::new();
        for e in rx {
            match e {
                Event::DownloadProgress(p) => {
                    download_progress.merge_progress(p);
                }
                Event::WriteProgress(p) => {
                    write_progress.merge_progress(p);
                }
                _ => {}
            }
        }
        assert_eq!(download_progress.total(), mock_body.len());
        assert_eq!(write_progress.total(), mock_body.len());
    }
}
