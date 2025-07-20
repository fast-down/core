use crate::{
    args::DownloadArgs,
    fmt,
    persist::Database,
    progress::{self, Painter as ProgressPainter},
};
use color_eyre::eyre::{eyre, Result};
use fast_down::{file::DownloadOptions, Event, MergeProgress, ProgressEntry, Total};
use path_clean;
use reqwest::{
    header::{self, HeaderValue},
    Client, Proxy,
};
use std::{env, path::Path, sync::Arc, time::Instant};
use tokio::{
    io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader},
    runtime::Handle,
    sync::Mutex,
};
use url::Url;

enum AutoConfirm {
    Enable(bool),
    Disable,
}

macro_rules! predicate {
    ($args:expr) => {
        if ($args.yes) {
            AutoConfirm::Enable(true)
        } else if ($args.no) {
            AutoConfirm::Enable(false)
        } else {
            AutoConfirm::Disable
        }
    };
}

#[inline]
async fn confirm(predicate: impl Into<AutoConfirm>, prompt: &str, default: bool) -> Result<bool> {
    fn get_text(value: bool) -> u8 {
        match value {
            true => b'Y',
            false => b'N',
        }
    }
    let text = match default {
        true => b"(Y/n)",
        false => b"(y/N)",
    };
    let mut stderr = io::stderr();
    stderr.write_all(prompt.as_bytes()).await?;
    stderr.write_all(text).await?;
    if let AutoConfirm::Enable(value) = predicate.into() {
        stderr.write(&[get_text(value), b'\n']).await?;
        return Ok(value);
    }
    stderr.flush().await?;
    let mut input = String::with_capacity(4);
    BufReader::new(io::stdin()).read_line(&mut input).await?;
    match input.trim() {
        "y" | "Y" => Ok(true),
        "n" | "N" => Ok(false),
        "" => Ok(default),
        _ => Err(eyre!("无效输入，下载取消")),
    }
}

pub async fn download(mut args: DownloadArgs) -> Result<()> {
    if args.browser {
        let url = Url::parse(&args.url)?;
        args.headers
            .entry(header::ORIGIN)
            .or_insert(HeaderValue::from_str(
                url.origin().ascii_serialization().as_str(),
            )?);
        args.headers
            .entry(header::REFERER)
            .or_insert(HeaderValue::from_str(&args.url)?);
    }
    if args.verbose {
        dbg!(&args);
    }
    let mut client = Client::builder().default_headers(args.headers);
    if let Some(ref proxy) = args.proxy {
        client = client.proxy(Proxy::all(proxy)?);
    }
    let client = client.build()?;
    let db = Database::new().await?;

    let info = loop {
        match fast_down::get_url_info(&args.url, &client).await {
            Ok(info) => break info,
            Err(err) => println!("获取文件信息失败: {}", err),
        }
        tokio::time::sleep(args.retry_gap).await;
    };
    let threads = if info.can_fast_download {
        args.threads
    } else {
        1
    };
    let mut save_path =
        Path::new(&args.save_folder).join(args.file_name.as_ref().unwrap_or(&info.file_name));
    if save_path.is_relative() {
        if let Ok(current_dir) = env::current_dir() {
            save_path = current_dir.join(save_path);
        }
    }
    save_path = path_clean::clean(save_path);
    let save_path_str = Arc::new(save_path.to_str().unwrap().to_string());

    println!(
        "文件名: {}\n文件大小: {} ({} 字节) \n文件路径: {}\n线程数量: {}\nETag: {:?}\nLast-Modified: {:?}\n",
        info.file_name,
        fmt::format_size(info.file_size as f64),
        info.file_size,
        save_path.to_str().unwrap(),
        threads,
        info.etag,
        info.last_modified
    );

    let mut download_chunks = vec![0..info.file_size];
    let mut resume_download = false;
    let mut write_progress: Vec<ProgressEntry> = Vec::with_capacity(threads);

    if save_path.try_exists()? {
        if args.resume && info.can_fast_download {
            if let Ok(Some(progress)) = db.get_entry(save_path_str.clone()).await {
                let downloaded = progress.progress.total();
                if downloaded < info.file_size {
                    download_chunks = progress::invert(&progress.progress, info.file_size);
                    write_progress = progress.progress.clone();
                    resume_download = true;
                    println!("发现未完成的下载，将继续下载剩余部分");
                    println!(
                        "已下载: {} / {} ({}%)",
                        fmt::format_size(downloaded as f64),
                        fmt::format_size(info.file_size as f64),
                        downloaded * 100 / info.file_size
                    );
                    if !args.yes {
                        if progress.total_size != info.file_size {
                            if !confirm(
                                predicate!(args),
                                &format!(
                                    "原文件大小: {}\n现文件大小: {}\n文件大小不一致，是否继续？",
                                    progress.total_size, info.file_size
                                ),
                                false,
                            )
                            .await?
                            {
                                println!("下载取消");
                                return Ok(());
                            }
                        }
                        if progress.etag != info.etag {
                            if !confirm(predicate!(args), &format!(
                                "原文件 ETag: {:?}\n现文件 ETag: {:?}\n文件 ETag 不一致，是否继续？",
                                progress.etag, info.etag
                            ), false).await? {
                                println!("下载取消");
                                return Ok(());
                            }
                        } else if let Some(progress_etag) = progress.etag.as_ref() {
                            if progress_etag.starts_with("W/") {
                                if !confirm(
                                    predicate!(args),
                                    &format!(
                                        "使用弱 ETag: {}，无法保证文件一致是否继续？",
                                        progress_etag
                                    ),
                                    false,
                                )
                                .await?
                                {
                                    println!("下载取消");
                                    return Ok(());
                                }
                            }
                        } else {
                            if !confirm(
                                predicate!(args),
                                "此文件无 ETag，无法保证文件一致是否继续？",
                                false,
                            )
                            .await?
                            {
                                println!("下载取消");
                                return Ok(());
                            }
                        }
                        if progress.last_modified != info.last_modified {
                            if !confirm(
                                predicate!(args),
                                &format!("原文件最后编辑时间: {:?}\n现文件最后编辑时间: {:?}\n文件最后编辑时间不一致，是否继续？ ", progress.last_modified, info.last_modified),
                                false
                            ).await? {
                                println!("下载取消");
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
        if !args.yes && !resume_download && !args.force {
            if !confirm(predicate!(args), "文件已存在，是否覆盖？", false).await? {
                println!("下载取消");
                return Ok(());
            }
        }
    }

    let result = fast_down::file::download(
        info.final_url.clone(),
        &save_path,
        DownloadOptions {
            threads,
            can_fast_download: info.can_fast_download,
            file_size: info.file_size,
            client,
            download_chunks,
            retry_gap: args.retry_gap,
            write_buffer_size: args.write_buffer_size,
        },
    )
    .await?;

    let result_clone = result.clone();
    let rt_handle = Handle::current();
    ctrlc::set_handler(move || {
        rt_handle.block_on(async {
            result_clone.cancel().await;
        })
    })?;

    let mut last_db_update = Instant::now();

    if !resume_download {
        db.init_entry(
            save_path_str.clone(),
            info.file_size,
            info.etag,
            info.last_modified,
            info.file_name,
            info.final_url.to_string(),
        )
        .await?;
    }

    let painter = Arc::new(Mutex::new(ProgressPainter::new(
        write_progress.clone(),
        info.file_size,
        args.progress_width,
        0.9,
        args.repaint_gap,
    )));
    let cancel = ProgressPainter::start_update_thread(painter.clone());
    let start = Instant::now();
    let mut rx = result.event_chain.lock().await;
    while let Some(e) = rx.recv().await {
        match e {
            Event::DownloadProgress(p) => painter.lock().await.add(p),
            Event::WriteProgress(p) => {
                write_progress.merge_progress(p);
                if last_db_update.elapsed().as_secs() >= 1 {
                    last_db_update = Instant::now();
                    db.update_entry(
                        save_path_str.clone(),
                        write_progress.clone(),
                        start.elapsed().as_millis() as u64,
                    )
                    .await?;
                }
            }
            Event::ConnectError(id, err) => painter
                .lock()
                .await
                .print(&format!("线程 {} 连接失败, 错误原因: {:?}\n", id, err))?,
            Event::DownloadError(id, err) => painter
                .lock()
                .await
                .print(&format!("线程 {} 下载失败, 错误原因: {:?}\n", id, err))?,
            Event::WriteError(err) => painter
                .lock()
                .await
                .print(&format!("写入文件失败, 错误原因: {:?}\n", err))?,
            Event::Connecting(id) => {
                if args.verbose {
                    painter
                        .lock()
                        .await
                        .print(&format!("线程 {} 正在连接中……\n", id))?;
                }
            }
            Event::Finished(id) => {
                if args.verbose {
                    painter
                        .lock()
                        .await
                        .print(&format!("线程 {} 完成任务\n", id))?;
                }
            }
            Event::Abort(id) => {
                if args.verbose {
                    painter
                        .lock()
                        .await
                        .print(&format!("线程 {} 已中断\n", id))?;
                }
            }
            Event::Downloading(id) => {
                if args.verbose {
                    painter
                        .lock()
                        .await
                        .print(&format!("线程 {} 正在下载中……\n", id))?;
                }
            }
        }
    }
    db.update_entry(
        save_path_str.clone(),
        write_progress.clone(),
        start.elapsed().as_millis() as u64,
    )
    .await?;
    painter.lock().await.update()?;
    cancel();
    result.join().await.unwrap();
    Ok(())
}
