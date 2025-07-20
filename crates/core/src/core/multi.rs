use super::DownloadResult;
use crate::{ConnectErrorKind, Event, ProgressEntry, RandWriter, Total};
use bytes::Bytes;
use fast_steal::{SplitTask, StealTask, Task, TaskList};
use reqwest::{header, Client, IntoUrl, StatusCode};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Mutex};

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub threads: usize,
    pub client: Client,
    pub download_chunks: Vec<ProgressEntry>,
    pub retry_gap: Duration,
}

pub async fn download(
    url: impl IntoUrl,
    mut writer: impl RandWriter + 'static,
    options: DownloadOptions,
) -> Result<DownloadResult, reqwest::Error> {
    let url = url.into_url()?;
    let (tx, event_chain) = mpsc::channel(1024);
    let (tx_write, mut rx_write) = mpsc::channel::<(ProgressEntry, Bytes)>(1024);
    let tx_clone = tx.clone();
    let handle = tokio::spawn(async move {
        while let Some((spin, data)) = rx_write.recv().await {
            loop {
                match writer.write_randomly(spin.clone(), &data).await {
                    Ok(_) => break,
                    Err(e) => tx_clone.send(Event::WriteError(e)).await.unwrap(),
                }
                tokio::time::sleep(options.retry_gap).await;
            }
            tx_clone.send(Event::WriteProgress(spin)).await.unwrap();
        }
        loop {
            match writer.flush().await {
                Ok(_) => break,
                Err(e) => tx_clone.send(Event::WriteError(e)).await.unwrap(),
            };
            tokio::time::sleep(options.retry_gap).await;
        }
    });
    let mutex = Arc::new(Mutex::new(()));
    let task_list = Arc::new(TaskList::from(options.download_chunks));
    let tasks = Arc::new(
        Task::from(&*task_list)
            .split_task(options.threads as u64)
            .map(|t| Arc::new(t))
            .collect::<Vec<_>>(),
    );
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();
    let client = Arc::new(options.client);
    let url = Arc::new(url);
    for (id, task) in tasks.iter().enumerate() {
        let task = task.clone();
        let tasks = tasks.clone();
        let task_list = task_list.clone();
        let mutex = mutex.clone();
        let tx = tx.clone();
        let running = running.clone();
        let client = client.clone();
        let url = url.clone();
        let tx_write = tx_write.clone();
        tokio::spawn(async move {
            'a: loop {
                if !running.load(Ordering::Relaxed) {
                    tx.send(Event::Abort(id)).await.unwrap();
                    return;
                }
                let mut start = task.start();
                if start >= task.end() {
                    let guard = mutex.lock().await;
                    if task.steal(&tasks, 2) {
                        continue;
                    }
                    drop(guard);
                    tx.send(Event::Finished(id)).await.unwrap();
                    return;
                }
                let download_range = &task_list.get_range(start..task.end());
                for range in download_range {
                    let header_range_value = format!("bytes={}-{}", range.start, range.end - 1);
                    let mut response = loop {
                        if !running.load(Ordering::Relaxed) {
                            tx.send(Event::Abort(id)).await.unwrap();
                            return;
                        }
                        tx.send(Event::Connecting(id)).await.unwrap();
                        match client
                            .get(url.as_str())
                            .header(header::RANGE, &header_range_value)
                            .send()
                            .await
                        {
                            Ok(response) if response.status() == StatusCode::PARTIAL_CONTENT => {
                                break response
                            }
                            Ok(response) => tx.send(Event::ConnectError(
                                id,
                                ConnectErrorKind::StatusCode(response.status()),
                            )),
                            Err(e) => {
                                tx.send(Event::ConnectError(id, ConnectErrorKind::Reqwest(e)))
                            }
                        }
                        .await
                        .unwrap();
                        tokio::time::sleep(options.retry_gap).await;
                    };
                    tx.send(Event::Downloading(id)).await.unwrap();
                    let mut downloaded = 0;
                    loop {
                        let chunk = loop {
                            if !running.load(Ordering::Relaxed) {
                                tx.send(Event::Abort(id)).await.unwrap();
                                return;
                            }
                            match response.chunk().await {
                                Ok(chunk) => break chunk,
                                Err(e) => tx.send(Event::DownloadError(id, e)).await.unwrap(),
                            }
                            tokio::time::sleep(options.retry_gap).await;
                        };
                        if chunk.is_none() {
                            break;
                        }
                        let mut chunk = chunk.unwrap();
                        let len = chunk.len() as u64;
                        task.fetch_add_start(len);
                        start += len;
                        let range_start = range.start + downloaded;
                        downloaded += len;
                        let range_end = range.start + downloaded;
                        let span = range_start..range_end.min(task_list.get(task.end()));
                        let len = span.total();
                        tx.send(Event::DownloadProgress(span.clone()))
                            .await
                            .unwrap();
                        tx_write
                            .send((span, chunk.split_to(len as usize)))
                            .await
                            .unwrap();
                        if start >= task.end() {
                            continue 'a;
                        }
                    }
                }
            }
        });
    }
    Ok(DownloadResult::new(
        event_chain,
        handle,
        Box::new(move || {
            running_clone.store(false, Ordering::Relaxed);
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "file")]
    use crate::writer::file::rand_file_writer_mmap::RandFileWriter;
    use crate::{MergeProgress, ProgressEntry};
    use tempfile::NamedTempFile;

    fn build_mock_data(size: usize) -> Vec<u8> {
        (0..size).map(|i| (i % 256) as u8).collect()
    }

    pub fn reverse_progress(progress: &[ProgressEntry], total_size: u64) -> Vec<ProgressEntry> {
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

    #[cfg(feature = "file")]
    #[tokio::test]
    async fn test_multi_thread_regular_download() {
        use tokio::{fs::File, io::AsyncReadExt};

        let mock_body = build_mock_data(3 * 1024);
        let mock_body_clone = mock_body.clone();
        let mut server = mockito::Server::new_async().await;
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
            .create_async()
            .await;

        let temp_file = NamedTempFile::new().unwrap();
        let file = temp_file.reopen().unwrap().into();

        let client = Client::new();
        let download_chunks = vec![0..mock_body.len() as u64];
        let result = download(
            format!("{}/mutli-2", server.url()),
            RandFileWriter::new(file, mock_body.len() as u64, 8 * 1024 * 1024)
                .await
                .unwrap(),
            DownloadOptions {
                client,
                threads: 32,
                download_chunks: download_chunks.clone(),
                retry_gap: Duration::from_secs(1),
            },
        )
        .await
        .unwrap();

        let mut download_progress: Vec<ProgressEntry> = Vec::new();
        let mut write_progress: Vec<ProgressEntry> = Vec::new();
        let mut rx = result.event_chain.lock().await;
        while let Some(e) = rx.recv().await {
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

        result.join().await.unwrap();

        let output = {
            let mut data = Vec::with_capacity(mock_body.len());
            for _ in 0..mock_body.len() {
                data.push(0);
            }
            for chunk in download_chunks.clone() {
                for i in chunk {
                    data[i as usize] = mock_body[i as usize];
                }
            }
            data
        };
        let mut file_content = Vec::new();
        File::open(temp_file.path())
            .await
            .unwrap()
            .read_to_end(&mut file_content)
            .await
            .unwrap();
        assert_eq!(file_content, output);
    }

    #[cfg(feature = "file")]
    #[tokio::test]
    async fn test_multi_thread_download_chunk() {
        use tokio::{fs::File, io::AsyncReadExt};

        let mock_body = build_mock_data(3 * 1024);
        let mock_body_clone = mock_body.clone();
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/multi-2")
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
            .create_async()
            .await;

        let temp_file = NamedTempFile::new().unwrap();
        let file = temp_file.reopen().unwrap().into();

        let client = Client::new();
        let download_chunks = vec![10..80, 100..300, 1000..2000];
        let result = download(
            format!("{}/multi-2", server.url()),
            RandFileWriter::new(file, mock_body.len() as u64, 8 * 1024 * 1024)
                .await
                .unwrap(),
            DownloadOptions {
                client,
                threads: 32,
                download_chunks: download_chunks.clone(),
                retry_gap: Duration::from_secs(1),
            },
        )
        .await
        .unwrap();

        let mut download_progress: Vec<ProgressEntry> = Vec::new();
        let mut write_progress: Vec<ProgressEntry> = Vec::new();
        let mut rx = result.event_chain.lock().await;
        while let Some(e) = rx.recv().await {
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

        result.join().await.unwrap();

        let output = {
            let mut data = Vec::with_capacity(mock_body.len());
            for _ in 0..mock_body.len() {
                data.push(0);
            }
            for chunk in download_chunks.clone() {
                for i in chunk {
                    data[i as usize] = mock_body[i as usize];
                }
            }
            data
        };
        let mut file_content = Vec::new();
        File::open(temp_file.path())
            .await
            .unwrap()
            .read_to_end(&mut file_content)
            .await
            .unwrap();
        assert_eq!(file_content, output);
    }

    #[cfg(feature = "file")]
    #[tokio::test]
    async fn test_multi_thread_break_point() {
        use tokio::{fs::File, io::AsyncReadExt};

        let mock_body = build_mock_data(200 * 1024 * 1024);
        let mock_body_clone = mock_body.clone();
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/mutli-3")
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
            .create_async()
            .await;

        let temp_file = NamedTempFile::new().unwrap();
        let mut write_progress: Vec<ProgressEntry> = Vec::new();
        {
            let file = temp_file.reopen().unwrap().into();
            let client = Client::new();
            let result = download(
                format!("{}/mutli-3", server.url()),
                RandFileWriter::new(file, mock_body.len() as u64, 8 * 1024 * 1024)
                    .await
                    .unwrap(),
                DownloadOptions {
                    client,
                    threads: 32,
                    download_chunks: vec![0..mock_body.len() as u64],
                    retry_gap: Duration::from_secs(1),
                },
            )
            .await
            .unwrap();
            let result_clone = result.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(1000)).await;
                result_clone.cancel().await;
            });
            let mut download_progress: Vec<ProgressEntry> = Vec::new();
            let mut rx = result.event_chain.lock().await;
            while let Some(e) = rx.recv().await {
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
            result.join().await.unwrap();
            let mut file_content = Vec::new();
            File::open(temp_file.path())
                .await
                .unwrap()
                .read_to_end(&mut file_content)
                .await
                .unwrap();
            let output = {
                let mut data = vec![0; mock_body.len()];
                for chunk in write_progress.clone() {
                    for i in chunk {
                        data[i as usize] = mock_body[i as usize];
                    }
                }
                data
            };
            assert_eq!(file_content, output);
        }

        // 开始续传
        println!("开始续传");
        let file = temp_file.reopen().unwrap().into();
        let client = Client::new();
        let download_chunks = reverse_progress(&write_progress, mock_body.len() as u64);
        let result = download(
            format!("{}/mutli-3", server.url()),
            RandFileWriter::new(file, mock_body.len() as u64, 8 * 1024 * 1024)
                .await
                .unwrap(),
            DownloadOptions {
                client,
                threads: 8,
                download_chunks: download_chunks.clone(),
                retry_gap: Duration::from_secs(1),
            },
        )
        .await
        .unwrap();

        let mut download_progress: Vec<ProgressEntry> = Vec::new();
        let mut write_progress: Vec<ProgressEntry> = Vec::new();
        let mut rx = result.event_chain.lock().await;
        while let Some(e) = rx.recv().await {
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

        result.join().await.unwrap();

        let mut file_content = Vec::new();
        File::open(temp_file.path())
            .await
            .unwrap()
            .read_to_end(&mut file_content)
            .await
            .unwrap();
        assert_eq!(file_content, mock_body);
    }
}
