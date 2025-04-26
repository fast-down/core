extern crate alloc;
extern crate std;
use super::read_response::read_response;
use super::writer::DownloadWriter;
use crate::{fmt_progress, Event};
use alloc::format;
use alloc::{sync::Arc, vec};
use bytes::BytesMut;
use color_eyre::eyre::eyre;
use color_eyre::Result;
use core::ops::Range;
use core::time::Duration;
use fast_steal::{Spawn, TaskList};
use reqwest::{blocking::Client, header, IntoUrl, StatusCode};
use std::thread::{self, JoinHandle};
use vec::Vec;

pub struct DownloadOptions {
    pub threads: usize,
    pub client: Client,
    pub get_chunk_size: usize,
    pub download_chunks: Vec<Range<usize>>,
    pub retry_gap: Duration,
}

pub fn download<Writer: DownloadWriter + 'static>(
    url: impl IntoUrl,
    writer: Writer,
    options: DownloadOptions,
) -> Result<(crossbeam_channel::Receiver<Event>, Vec<JoinHandle<()>>)> {
    let url = url.into_url()?;
    let (tx, rx) = crossbeam_channel::unbounded();
    let tasks: Arc<TaskList> = Arc::new(options.download_chunks.into());
    let tasks_clone = tasks.clone();
    let handles = tasks.spawn(
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
            let range_str = fmt_progress::fmt_progress(&tasks_clone.get_range(start..end));
            let mut response = loop {
                tx.send(Event::Connecting(id)).unwrap();
                match options
                    .client
                    .get(url.clone())
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
            let mut buffer = BytesMut::with_capacity(options.get_chunk_size);
            loop {
                let end = task.end();
                if start >= end {
                    break;
                }
                let remain = end - start;
                task.fetch_add_start(remain.min(options.get_chunk_size));
                let len = read_response(&mut response, &mut buffer, options.retry_gap, |err| {
                    tx.send(Event::DownloadError(0, err.into())).unwrap();
                });
                let chunk_end = (start + len).min(task.end());
                let len = chunk_end - start;
                let span = start..chunk_end;
                tx.send(Event::DownloadProgress(span.clone())).unwrap();

                // SAFETY: fast-steal 2.6.0 guarantee it won't write the same place twice
                match unsafe {
                    writer.write_part_unchecked(span.clone(), buffer.clone().split_to(len).freeze())
                } {
                    Ok(_) => {
                        tx.send(Event::WriteProgress(span)).unwrap();
                    }
                    Err(e) => tx.send(Event::WriteError(e)).unwrap(),
                }
                start += len;
            }
        },
    );
    Ok((rx, handles))
}

#[cfg(test)]
#[cfg(feature = "file")]
mod tests {
    use super::*;
    use crate::core::file_writer::FileWriter;
    use crate::{MergeProgress, Progress, Total};
    use std::fs::File;
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
        let (rx, handles) = download(
            server.url(),
            FileWriter::new::<4>(file).unwrap(),
            DownloadOptions {
                client,
                threads: 8,
                get_chunk_size: 8 * 1024,
                download_chunks: vec![0..mock_body.len()],
                retry_gap: Duration::from_secs(1),
            },
        )
        .unwrap();

        for handle in handles {
            handle.join().unwrap();
        }

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
