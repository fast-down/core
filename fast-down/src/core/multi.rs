use crate::base::fmt_progress;
use crate::{Event, RandWriter};
use bytes::{Bytes, BytesMut};
use color_eyre::eyre::eyre;
use color_eyre::Result;
use crossbeam_channel::Receiver;
use fast_steal::{sync::action, sync::Spawn, TaskList};
use reqwest::{blocking::Client, header, IntoUrl, StatusCode};
use std::io::Read;
use std::ops::Range;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub struct DownloadOptions {
    pub threads: usize,
    pub client: Client,
    pub download_chunks: Vec<Range<usize>>,
    pub retry_gap: Duration,
    pub download_buffer_size: usize,
}

pub fn download(
    url: impl IntoUrl,
    mut writer: impl RandWriter + 'static,
    options: DownloadOptions,
) -> Result<(Receiver<Event>, JoinHandle<()>)> {
    let url = url.into_url()?;
    let (tx, rx) = crossbeam_channel::unbounded();
    let (tx_write, rx_write) = crossbeam_channel::unbounded::<(Range<usize>, Bytes)>();
    let tx_clone = tx.clone();
    let handle = thread::spawn(move || {
        for (spin, data) in rx_write {
            match writer.write_randomly(spin.clone(), data) {
                Ok(_) => tx_clone.send(Event::WriteProgress(spin)),
                Err(e) => tx_clone.send(Event::WriteError(e.into())),
            }
            .unwrap()
        }
    });
    let tasks: Arc<TaskList> = Arc::new(options.download_chunks.into());
    let tasks_clone = tasks.clone();
    tasks.spawn(
        options.threads,
        |executor| thread::spawn(move || executor.run()),
        action::from_fn(move |id, task, get_task| 'retry: loop {
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
                    Ok(response) => tx.send(Event::ConnectError(
                        id,
                        eyre!("Expect to get 206, but got {}", response.status()),
                    )),
                    Err(e) => tx.send(Event::ConnectError(id, e.into())),
                }
                .unwrap();
                thread::sleep(options.retry_gap);
            };
            tx.send(Event::Downloading(id)).unwrap();
            let mut buffer = BytesMut::with_capacity(options.download_buffer_size);
            loop {
                let end = task.end();
                if start >= end {
                    break;
                }
                let remain = end - start;
                let expect_len = remain.min(options.download_buffer_size);
                task.fetch_add_start(expect_len);
                let mut retry_count = 0;
                let len = loop {
                    unsafe { buffer.set_len(options.download_buffer_size) };
                    match response.read(&mut buffer) {
                        Ok(len) => {
                            unsafe { buffer.set_len(len) };
                            break len;
                        }
                        Err(e) => tx.send(Event::DownloadError(0, e.into())).unwrap(),
                    };
                    thread::sleep(options.retry_gap);
                    retry_count += 1;
                    if retry_count > 3 {
                        task.fetch_sub_start(expect_len);
                        continue 'retry;
                    }
                };
                if expect_len != len {
                    task.fetch_sub_start(expect_len);
                    tx.send(Event::DownloadError(
                        0,
                        eyre!("Expected to read {expect_len} bytes of data, but actually read {len} bytes"),
                    ))
                    .unwrap();
                    continue 'retry;
                }
                let chunk_end = (start + len).min(task.end());
                let len = chunk_end - start;
                let span = start..chunk_end;
                tx.send(Event::DownloadProgress(span.clone())).unwrap();
                tx_write.send((span, buffer.clone().freeze())).unwrap();
                start += len;
            }
        }),
    );
    Ok((rx, handle))
}

#[cfg(test)]
#[cfg(feature = "file")]
mod tests {
    use super::*;
    use crate::{MergeProgress, Progress, RandFileWriter, Total};
    use std::fs::File;
    use std::io::Read;
    use tempfile::NamedTempFile;

    #[test]
    fn test_multi_thread_regular_download() {
        let mock_body = vec![b'a'; 3 * 1024];
        let mock_body_clone = mock_body.clone();
        let mut server = mockito::Server::new();
        server
            .mock("GET", "/mutli")
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

        let client = Client::new();
        let (rx, handle) = download(
            format!("{}/mutli", server.url()),
            RandFileWriter::new(file, mock_body.len()).unwrap(),
            DownloadOptions {
                client,
                threads: 1000,
                download_buffer_size: 8 * 1024,
                download_chunks: vec![0..mock_body.len()],
                retry_gap: Duration::from_secs(1),
            },
        )
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
