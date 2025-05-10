use super::DownloadResult;
use crate::{Event, Flush, Progress, RandWriter, Total};
use bytes::{Bytes, BytesMut};
use color_eyre::eyre::eyre;
use color_eyre::Result;
use fast_steal::{sync::action, sync::Spawn, TaskList};
use reqwest::{blocking::Client, header, IntoUrl, StatusCode};
use std::io::{ErrorKind, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub struct DownloadOptions {
    pub threads: usize,
    pub client: Client,
    pub download_chunks: Vec<Progress>,
    pub retry_gap: Duration,
    pub download_buffer_size: usize,
}

pub fn download(
    url: impl IntoUrl,
    mut writer: impl RandWriter + Flush + 'static,
    options: DownloadOptions,
) -> Result<DownloadResult> {
    let url = url.into_url()?;
    let (tx, event_chain) = crossbeam_channel::unbounded();
    let (tx_write, rx_write) = crossbeam_channel::unbounded::<(Progress, Bytes)>();
    let tx_clone = tx.clone();
    let handle = thread::spawn(move || {
        for (spin, data) in rx_write {
            loop {
                match writer.write_randomly(spin.clone(), data.clone()) {
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
    let tasks: Arc<TaskList> = Arc::new(options.download_chunks.into());
    let tasks_clone = tasks.clone();
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();
    tasks.spawn(
        options.threads,
        |executor| thread::spawn(move || executor.run()),
        action::from_fn(move |id, task, get_task| 'retry: loop {
            if !running.load(Ordering::Relaxed) {
                tx.send(Event::Abort(id)).unwrap();
                return;
            }
            let mut start = task.start();
            if start >= task.end() {
                if get_task() {
                    continue;
                }
                tx.send(Event::Finished(id)).unwrap();
                return;
            }
            let download_range = &tasks_clone.get_range(start..task.end());
            let mut range_size = 0;
            let start_point = start;
            for range in download_range {
                range_size += range.total();
                let header_range_value = format!("bytes={}-{}", range.start, range.end - 1);
                let mut response = loop {
                    if !running.load(Ordering::Relaxed) {
                        tx.send(Event::Abort(id)).unwrap();
                        return;
                    }
                    tx.send(Event::Connecting(id)).unwrap();
                    match options
                        .client
                        .get(url.clone())
                        .header(header::RANGE, &header_range_value)
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
                    if !running.load(Ordering::Relaxed) {
                        tx.send(Event::Abort(id)).unwrap();
                        return;
                    }
                    let end = task.end().min(start_point + range_size);
                    if start >= end {
                        break;
                    }
                    let expect_len = options.download_buffer_size.min(end - start);
                    task.fetch_add_start(expect_len);
                    unsafe { buffer.set_len(expect_len) }
                    let len = loop {
                        if !running.load(Ordering::Relaxed) {
                            task.fetch_sub_start(expect_len);
                            tx.send(Event::Abort(id)).unwrap();
                            return;
                        }
                        match response.read(&mut buffer) {
                            Ok(len) => break len,
                            Err(e) => {
                                let kind = e.kind();
                                tx.send(Event::DownloadError(id, e.into())).unwrap();
                                if kind != ErrorKind::Interrupted {
                                    task.fetch_sub_start(expect_len);
                                    continue 'retry;
                                }
                            }
                        };
                        thread::sleep(options.retry_gap);
                    };
                    if expect_len > len {
                        task.fetch_sub_start(expect_len - len);
                    }
                    todo!("此处逻辑有问题");
                    let span = range.start..range.start + len;
                    tx.send(Event::DownloadProgress(span.clone())).unwrap();
                    tx_write
                        .send((span, buffer.clone().split_to(len).freeze()))
                        .unwrap();
                    start += len;
                }
            }
        }),
    );
    Ok(DownloadResult {
        event_chain,
        handle,
        cancel_fn: Box::new(move || {
            running_clone.store(false, Ordering::Relaxed);
        }),
    })
}

#[cfg(test)]
#[cfg(feature = "file")]
mod tests {
    use super::*;
    use crate::{MergeProgress, Progress, RandFileWriter};
    use std::fs::File;
    use std::io::Read;
    use tempfile::NamedTempFile;

    fn build_mock_data(size: usize) -> Vec<u8> {
        (0..size).map(|i| (i % 256) as u8).collect()
    }

    pub fn reverse_progress(progress: &[Progress], total_size: usize) -> Vec<Progress> {
        if progress.is_empty() {
            return vec![0..total_size];
        }
        let mut result = Vec::with_capacity(progress.len());
        let mut prev_end = 0;
        for range in progress {
            if range.start > prev_end {
                result.push(prev_end..range.start);
            }
            prev_end = range.end;
        }
        if prev_end < total_size {
            result.push(prev_end..total_size);
        }
        result
    }

    #[test]
    fn test_multi_thread_regular_download() {
        let mock_body = build_mock_data(3 * 1024);
        let mock_body_clone = mock_body.clone();
        let mut server = mockito::Server::new();
        server
            .mock("GET", "/mutli-2")
            .with_status(206)
            .with_body_from_request(move |request| {
                if !request.has_header("Range") {
                    return mock_body_clone.clone();
                }
                let range = request.header("Range")[0];
                println!("range: {:?}", range);
                range
                    .to_str()
                    .unwrap()
                    .rsplit('=')
                    .next()
                    .unwrap()
                    .split(',')
                    .map(|p| p.trim().splitn(2, '-'))
                    .map(|mut p| {
                        let start = p.next().unwrap().parse::<usize>().unwrap();
                        let end = p.next().unwrap().parse::<usize>().unwrap();
                        start..=end
                    })
                    .flat_map(|p| mock_body_clone[p].to_vec())
                    .collect()
            })
            .create();

        let temp_file = NamedTempFile::new().unwrap();
        let file = temp_file.reopen().unwrap();

        let client = Client::new();
        let download_chunks = vec![0..mock_body.len()];
        let DownloadResult {
            event_chain,
            handle,
            cancel_fn: _,
        } = download(
            format!("{}/mutli-2", server.url()),
            RandFileWriter::new(file, mock_body.len()).unwrap(),
            DownloadOptions {
                client,
                threads: 32,
                download_buffer_size: 8 * 1024,
                download_chunks: download_chunks.clone(),
                retry_gap: Duration::from_secs(1),
            },
        )
        .unwrap();

        let mut download_progress: Vec<Progress> = Vec::new();
        let mut write_progress: Vec<Progress> = Vec::new();
        for e in event_chain {
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
        dbg!(&download_progress);
        dbg!(&write_progress);
        assert_eq!(download_progress, download_chunks);
        assert_eq!(write_progress, download_chunks);

        handle.join().unwrap();

        let output = {
            let mut data = Vec::with_capacity(mock_body.len());
            for _ in 0..mock_body.len() {
                data.push(0);
            }
            for chunk in download_chunks.clone() {
                for i in chunk {
                    data[i] = mock_body[i];
                }
            }
            data
        };
        let mut file_content = Vec::new();
        File::open(temp_file.path())
            .unwrap()
            .read_to_end(&mut file_content)
            .unwrap();
        assert_eq!(file_content, output);
    }

    #[test]
    fn test_multi_thread_download_chunk() {
        let mock_body = build_mock_data(3 * 1024);
        let mock_body_clone = mock_body.clone();
        let mut server = mockito::Server::new();
        server
            .mock("GET", "/mutli-2")
            .with_status(206)
            .with_body_from_request(move |request| {
                if !request.has_header("Range") {
                    return mock_body_clone.clone();
                }
                let range = request.header("Range")[0];
                println!("range: {:?}", range);
                range
                    .to_str()
                    .unwrap()
                    .rsplit('=')
                    .next()
                    .unwrap()
                    .split(',')
                    .map(|p| p.trim().splitn(2, '-'))
                    .map(|mut p| {
                        let start = p.next().unwrap().parse::<usize>().unwrap();
                        let end = p.next().unwrap().parse::<usize>().unwrap();
                        start..=end
                    })
                    .flat_map(|p| mock_body_clone[p].to_vec())
                    .collect()
            })
            .create();

        let temp_file = NamedTempFile::new().unwrap();
        let file = temp_file.reopen().unwrap();

        let client = Client::new();
        let download_chunks = vec![10..80, 100..300, 1000..2000];
        let DownloadResult {
            event_chain,
            handle,
            cancel_fn: _,
        } = download(
            format!("{}/mutli-2", server.url()),
            RandFileWriter::new(file, mock_body.len()).unwrap(),
            DownloadOptions {
                client,
                threads: 32,
                download_buffer_size: 8 * 1024,
                download_chunks: download_chunks.clone(),
                retry_gap: Duration::from_secs(1),
            },
        )
        .unwrap();

        let mut download_progress: Vec<Progress> = Vec::new();
        let mut write_progress: Vec<Progress> = Vec::new();
        for e in event_chain {
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
        dbg!(&download_progress);
        dbg!(&write_progress);
        assert_eq!(download_progress, download_chunks);
        assert_eq!(write_progress, download_chunks);

        handle.join().unwrap();

        let output = {
            let mut data = Vec::with_capacity(mock_body.len());
            for _ in 0..mock_body.len() {
                data.push(0);
            }
            for chunk in download_chunks.clone() {
                for i in chunk {
                    data[i] = mock_body[i];
                }
            }
            data
        };
        let mut file_content = Vec::new();
        File::open(temp_file.path())
            .unwrap()
            .read_to_end(&mut file_content)
            .unwrap();
        assert_eq!(file_content, output);
    }

    #[test]
    fn test_multi_thread_break_point() {
        let mock_body = build_mock_data(200 * 1024 * 1024);
        let mock_body_clone = mock_body.clone();
        let mut server = mockito::Server::new();
        server
            .mock("GET", "/mutli-3")
            .with_status(206)
            .with_body_from_request(move |request| {
                if !request.has_header("Range") {
                    return mock_body_clone.clone();
                }
                let range = request.header("Range")[0];
                println!("range: {:?}", range);
                thread::sleep(Duration::from_millis(20));
                range
                    .to_str()
                    .unwrap()
                    .rsplit('=')
                    .next()
                    .unwrap()
                    .split(',')
                    .map(|p| p.trim().splitn(2, '-'))
                    .map(|mut p| {
                        let start = p.next().unwrap().parse::<usize>().unwrap();
                        let end = p.next().unwrap().parse::<usize>().unwrap();
                        start..=end
                    })
                    .flat_map(|p| mock_body_clone[p].to_vec())
                    .collect()
            })
            .create();

        let temp_file = NamedTempFile::new().unwrap();
        let file = temp_file.reopen().unwrap();

        let client = Client::new();
        let DownloadResult {
            event_chain,
            handle,
            cancel_fn,
        } = download(
            format!("{}/mutli-3", server.url()),
            RandFileWriter::new(file, mock_body.len()).unwrap(),
            DownloadOptions {
                client,
                threads: 32,
                download_buffer_size: 8 * 1024,
                download_chunks: vec![0..mock_body.len()],
                retry_gap: Duration::from_secs(1),
            },
        )
        .unwrap();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(1));
            cancel_fn()
        });

        let mut download_progress: Vec<Progress> = Vec::new();
        let mut write_progress: Vec<Progress> = Vec::new();
        for e in event_chain {
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
        dbg!(&download_progress);
        dbg!(&write_progress);
        assert_eq!(download_progress, write_progress);

        handle.join().unwrap();
        let mut file_content = Vec::new();
        File::open(temp_file.path())
            .unwrap()
            .read_to_end(&mut file_content)
            .unwrap();
        let output = {
            let mut data = Vec::with_capacity(mock_body.len());
            for _ in 0..mock_body.len() {
                data.push(0);
            }
            for chunk in write_progress.clone() {
                for i in chunk {
                    data[i] = mock_body[i];
                }
            }
            data
        };
        assert_eq!(file_content, output);
        println!("开始续传");

        // 开始续传
        let file = temp_file.reopen().unwrap();
        let client = Client::new();
        let download_chunks = reverse_progress(&write_progress, mock_body.len());
        let DownloadResult {
            event_chain,
            handle,
            cancel_fn: _,
        } = download(
            format!("{}/mutli-3", server.url()),
            RandFileWriter::new(file, mock_body.len()).unwrap(),
            DownloadOptions {
                client,
                threads: 8,
                download_buffer_size: 8 * 1024,
                download_chunks: download_chunks.clone(),
                retry_gap: Duration::from_secs(1),
            },
        )
        .unwrap();

        let mut download_progress: Vec<Progress> = Vec::new();
        let mut write_progress: Vec<Progress> = Vec::new();
        for e in event_chain {
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
        dbg!(&download_progress);
        dbg!(&write_progress);
        assert_eq!(download_progress, download_chunks);
        assert_eq!(write_progress, download_chunks);

        handle.join().unwrap();

        let mut file_content = Vec::new();
        File::open(temp_file.path())
            .unwrap()
            .read_to_end(&mut file_content)
            .unwrap();
        assert_eq!(file_content, mock_body);
    }
}
