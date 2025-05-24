mod args_parse;
mod draw_progress;
mod fmt_progress;
mod fmt_size;
mod fmt_time;
mod persist;
mod reverse_progress;
mod str_to_progress;

use args_parse::Args;
use color_eyre::eyre::{eyre, Result};
use draw_progress::ProgressPainter;
use fast_down::{DownloadOptions, Event, MergeProgress, Progress, Total};
use fmt_size::format_file_size;
use path_clean::clean;
use persist::{init_db, init_progress, update_progress};
use reqwest::{
    blocking::Client,
    header::{self, HeaderValue},
    Proxy,
};
use reverse_progress::reverse_progress;
use std::{
    env,
    io::{self, Write},
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use url::Url;

fn main() -> Result<()> {
    color_eyre::install()?;
    let mut args = Args::parse()?;
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
    let conn = init_db()?;

    let info = loop {
        match fast_down::get_url_info(&args.url, &client) {
            Ok(info) => break info,
            Err(err) => {
                println!("获取文件信息失败: {}", err);
                thread::sleep(args.retry_gap);
            }
        }
    };
    let threads = if info.can_fast_download {
        args.threads
    } else {
        1
    };
    let mut save_path =
        Path::new(&args.save_folder).join(args.file_name.as_ref().unwrap_or(&info.file_name));
    if save_path.is_relative() {
        save_path = env::current_dir()?.join(save_path);
    }
    save_path = clean(save_path);
    let save_path_str = save_path.to_str().unwrap().to_string();

    println!(
        "文件名: {}\n文件大小: {} ({} 字节) \n文件路径: {}\n线程数量: {}\nETag: {:?}\nLast-Modified: {:?}\n",
        info.file_name,
        format_file_size(info.file_size as f64),
        info.file_size,
        save_path.to_str().unwrap(),
        threads,
        info.etag,
        info.last_modified
    );

    let mut download_chunks = vec![0..info.file_size];
    let mut resume_download = false;
    let mut write_progress: Vec<Progress> = Vec::with_capacity(threads);

    if save_path.try_exists()? {
        if args.resume && info.can_fast_download {
            if let Ok(Some(progress)) = persist::get_progress(&conn, &save_path_str) {
                let downloaded = progress.progress.total();
                if downloaded < info.file_size {
                    download_chunks = reverse_progress(&progress.progress, info.file_size);
                    write_progress = progress.progress.clone();
                    resume_download = true;
                    println!("发现未完成的下载，将继续下载剩余部分");
                    println!(
                        "已下载: {} / {} ({}%)",
                        format_file_size(downloaded as f64),
                        format_file_size(info.file_size as f64),
                        downloaded * 100 / info.file_size
                    );
                    if !args.yes {
                        if progress.total_size != info.file_size {
                            eprint!(
                                "原文件大小: {}\n现文件大小: {}\n文件大小不一致，是否继续？(y/N) ",
                                progress.total_size, info.file_size
                            );
                            if args.no {
                                eprintln!("N");
                                return Ok(());
                            }
                            io::stdout().flush()?;
                            let mut input = String::new();
                            io::stdin().read_line(&mut input)?;
                            match input.trim().to_lowercase().as_str() {
                                "y" => {}
                                "n" | "" => {
                                    println!("下载取消");
                                    return Ok(());
                                }
                                _ => return Err(eyre!("无效输入，下载取消")),
                            }
                        }
                        if progress.etag != info.etag {
                            eprint!(
                                "原文件 ETag: {:?}\n现文件 ETag: {:?}\n文件 ETag 不一致，是否继续？(y/N) ",
                                progress.etag, info.etag
                            );
                            if args.no {
                                eprintln!("N");
                                return Ok(());
                            }
                            io::stdout().flush()?;
                            let mut input = String::new();
                            io::stdin().read_line(&mut input)?;
                            match input.trim().to_lowercase().as_str() {
                                "y" => {}
                                "n" | "" => {
                                    println!("下载取消");
                                    return Ok(());
                                }
                                _ => return Err(eyre!("无效输入，下载取消")),
                            }
                        } else if let Some(progress_etag) = progress.etag.as_ref() {
                            if progress_etag.starts_with("W/") {
                                eprint!(
                                    "使用弱 ETag: {}，无法保证文件一致是否继续？(y/N) ",
                                    progress_etag
                                );
                                if args.no {
                                    eprintln!("N");
                                    return Ok(());
                                }
                                io::stdout().flush()?;
                                let mut input = String::new();
                                io::stdin().read_line(&mut input)?;
                                match input.trim().to_lowercase().as_str() {
                                    "y" => {}
                                    "n" | "" => {
                                        println!("下载取消");
                                        return Ok(());
                                    }
                                    _ => return Err(eyre!("无效输入，下载取消")),
                                }
                            }
                        } else {
                            eprint!("此文件无 ETag，无法保证文件一致是否继续？(y/N) ");
                            if args.no {
                                eprintln!("N");
                                return Ok(());
                            }
                            io::stdout().flush()?;
                            let mut input = String::new();
                            io::stdin().read_line(&mut input)?;
                            match input.trim().to_lowercase().as_str() {
                                "y" => {}
                                "n" | "" => {
                                    println!("下载取消");
                                    return Ok(());
                                }
                                _ => return Err(eyre!("无效输入，下载取消")),
                            }
                        }
                        if progress.last_modified != info.last_modified {
                            eprint!("原文件最后编辑时间: {:?}\n现文件最后编辑时间: {:?}\n文件最后编辑时间不一致，是否继续？(y/N) ", progress.last_modified, info.last_modified);
                            if args.no {
                                eprintln!("N");
                                return Ok(());
                            }
                            io::stdout().flush()?;
                            let mut input = String::new();
                            io::stdin().read_line(&mut input)?;
                            match input.trim().to_lowercase().as_str() {
                                "y" => {}
                                "n" | "" => {
                                    println!("下载取消");
                                    return Ok(());
                                }
                                _ => return Err(eyre!("无效输入，下载取消")),
                            }
                        }
                    }
                }
            }
        }
        if !args.yes && !resume_download && !args.force {
            eprint!("文件已存在，是否覆盖？(y/N) ");
            if args.no {
                eprintln!("N");
                return Ok(());
            }
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            match input.trim().to_lowercase().as_str() {
                "y" => {}
                "n" | "" => {
                    println!("下载取消");
                    return Ok(());
                }
                _ => return Err(eyre!("无效输入，下载取消")),
            }
        }
    }

    let result = fast_down::download_file(
        &info.final_url,
        &save_path,
        DownloadOptions {
            threads,
            can_fast_download: info.can_fast_download,
            file_size: info.file_size,
            client,
            download_buffer_size: args.download_buffer_size,
            download_chunks,
            retry_gap: args.retry_gap,
            write_buffer_size: args.write_buffer_size,
        },
    )?;

    let result_clone = result.clone();
    ctrlc::set_handler(move || {
        result_clone.cancel();
    })?;

    let mut last_db_update = Instant::now();

    if !resume_download {
        init_progress(
            &conn,
            &save_path_str,
            info.file_size,
            info.etag,
            info.last_modified,
        )?;
    }

    let painter = Arc::new(Mutex::new(ProgressPainter::new(
        write_progress.clone(),
        info.file_size,
        args.progress_width,
        0.9,
        Duration::from_millis(100),
    )));
    let cancel = ProgressPainter::start_update_thread(painter.clone());

    for e in &result {
        match e {
            Event::DownloadProgress(p) => {
                painter.lock().unwrap().add(p);
            }
            Event::WriteProgress(p) => {
                write_progress.merge_progress(p);
                if last_db_update.elapsed().as_secs() >= 1 {
                    last_db_update = Instant::now();
                    update_progress(&conn, &save_path_str, &write_progress)?;
                }
            }
            Event::ConnectError(id, err) => {
                painter
                    .lock()
                    .unwrap()
                    .print(&format!("线程 {} 连接失败, 错误原因: {:?}\n", id, err))?;
            }
            Event::DownloadError(id, err) => {
                painter
                    .lock()
                    .unwrap()
                    .print(&format!("线程 {} 下载失败, 错误原因: {:?}\n", id, err))?;
            }
            Event::WriteError(err) => {
                painter
                    .lock()
                    .unwrap()
                    .print(&format!("写入文件失败, 错误原因: {:?}\n", err))?;
            }
            Event::Connecting(id) => {
                if args.verbose {
                    painter
                        .lock()
                        .unwrap()
                        .print(&format!("线程 {} 正在连接中……\n", id))?;
                }
            }
            Event::Finished(id) => {
                if args.verbose {
                    painter
                        .lock()
                        .unwrap()
                        .print(&format!("线程 {} 完成任务\n", id))?;
                }
            }
            Event::Abort(id) => {
                if args.verbose {
                    painter
                        .lock()
                        .unwrap()
                        .print(&format!("线程 {} 已中断\n", id))?;
                }
            }
            Event::Downloading(id) => {
                if args.verbose {
                    painter
                        .lock()
                        .unwrap()
                        .print(&format!("线程 {} 正在下载中……\n", id))?;
                }
            }
        }
    }
    update_progress(&conn, &save_path_str, &write_progress)?;
    painter.lock().unwrap().update()?;
    cancel();
    result.join().unwrap();
    Ok(())
}
