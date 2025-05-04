mod build_headers;
mod draw_progress;
mod fmt_size;
mod fmt_time;
mod persist;
mod reverse_progress;
mod str_to_progress;

use build_headers::build_headers;
use clap::Parser;
use color_eyre::eyre::{eyre, Result};
use fast_down::{DownloadOptions, DownloadResult, Event, MergeProgress, Progress, Total};
use fmt_size::format_file_size;
use path_clean::clean;
use persist::{init_db, init_progress, update_progress};
use reqwest::{blocking::Client, Proxy};
use reverse_progress::reverse_progress;
use std::{
    env,
    io::{self, Write},
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

/// 超级快的下载器
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// 强制覆盖已有文件
    #[arg(short, long = "allow-overwrite", default_value_t = false)]
    pub force: bool,

    /// 断点续传
    #[arg(short = 'c', long = "continue", default_value_t = false)]
    pub resume: bool,

    /// 要下载的URL
    #[arg(required = true)]
    pub url: String,

    /// 保存目录
    #[arg(short = 'd', long = "dir", default_value = ".")]
    pub save_folder: String,

    /// 下载线程数
    #[arg(short, long)]
    pub threads: Option<usize>,

    /// 自定义文件名
    #[arg(short = 'o', long = "out")]
    pub file_name: Option<String>,

    /// 代理地址 (格式: http://proxy:port 或 socks5://proxy:port)
    #[arg(short, long = "all-proxy")]
    pub proxy: Option<String>,

    /// 自定义请求头 (可多次使用)
    #[arg(short = 'H', long = "header", value_name = "Key: Value")]
    pub headers: Vec<String>,

    /// 下载缓冲区大小 (单位: B)
    #[arg(long, default_value_t = 8 * 1024)]
    pub download_buffer_size: usize,

    /// 写入缓冲区大小 (单位: B)
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    pub write_buffer_size: usize,

    /// 进度条显示宽度
    #[arg(long, default_value_t = 50)]
    pub progress_width: usize,

    /// 重试间隔 (单位: ms)
    #[arg(long, default_value_t = 500)]
    pub retry_gap: u64,

    /// 数据库存储路径
    #[arg(long, default_value = "state.db")]
    pub db_path: String,
}

fn main() -> Result<()> {
    color_eyre::install()?;
    let args = Args::parse();
    let headers = build_headers(&args.headers)?;
    let mut client = Client::builder().default_headers(headers);
    if let Some(proxy) = args.proxy {
        client = client.proxy(Proxy::all(proxy)?);
    }
    let client = client.build()?;
    let conn = init_db(&args.db_path)?;

    let info = loop {
        match fast_down::get_url_info(&args.url, &client) {
            Ok(info) => break info,
            Err(err) => {
                println!("获取文件信息失败: {}", err);
                thread::sleep(Duration::from_millis(args.retry_gap));
            }
        }
    };
    let threads = if info.can_fast_download {
        args.threads
            .unwrap_or_else(|| info.file_size.checked_div(2 * 1024 * 1024).unwrap_or(4))
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
        "文件名: {}\n文件大小: {} ({} 字节) \n文件路径: {}\n线程数量: {}",
        info.file_name,
        format_file_size(info.file_size as f64),
        info.file_size,
        save_path.to_str().unwrap(),
        threads
    );

    // 检查是否有未完成的下载
    let mut download_chunks = vec![0..info.file_size];
    let mut resume_download = false;
    let mut write_progress: Vec<Progress> = Vec::new();
    let mut get_progress: Vec<Progress> = Vec::new();

    if save_path.try_exists()? {
        if args.resume && info.can_fast_download {
            if let Ok(Some(progress)) = persist::get_progress(&conn, &save_path_str) {
                if progress.total_size == info.file_size {
                    let downloaded = progress.progress.total();
                    if downloaded < info.file_size {
                        download_chunks = reverse_progress(&progress.progress, progress.total_size);
                        write_progress = progress.progress.clone();
                        get_progress = progress.progress;
                        if !download_chunks.is_empty() {
                            resume_download = true;
                            println!("发现未完成的下载，将继续下载剩余部分");
                            println!(
                                "已下载: {} / {} ({}%)",
                                format_file_size(downloaded as f64),
                                format_file_size(info.file_size as f64),
                                downloaded * 100 / info.file_size
                            );
                        }
                    }
                }
            }
        }
        if !resume_download && !args.force {
            eprint!("文件已存在，是否覆盖？(y/N) ");
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

    let start = Instant::now();
    let DownloadResult {
        event_chain,
        handle,
        cancel_fn,
    } = fast_down::download_file(
        &info.final_url,
        &save_path,
        DownloadOptions {
            threads,
            can_fast_download: info.can_fast_download,
            file_size: info.file_size,
            client,
            download_buffer_size: args.download_buffer_size,
            download_chunks,
            retry_gap: Duration::from_millis(args.retry_gap),
            write_buffer_size: args.write_buffer_size,
        },
    )?;

    let cancel_fn = Arc::new(Mutex::new(Some(cancel_fn)));
    ctrlc::set_handler(move || {
        if let Some(f) = cancel_fn.lock().unwrap().take() {
            f();
        }
    })
    .expect("Error setting Ctrl-C handler");

    let mut last_get_size = 0;
    let mut last_get_time = Instant::now();
    let mut avg_get_speed = 0.0;

    let mut last_progress_update = Instant::now();
    let mut last_db_update = Instant::now();

    if !resume_download {
        init_progress(&conn, &save_path_str, info.file_size)?;
    }

    eprint!("\n\n");
    for e in event_chain {
        match e {
            Event::DownloadProgress(ps) => {
                for p in ps {
                    get_progress.merge_progress(p);
                }
                if last_progress_update.elapsed().as_millis() > 50 {
                    last_progress_update = Instant::now();
                    let get_size = get_progress.total();
                    draw_progress::draw_progress(
                        start,
                        info.file_size,
                        &get_progress,
                        last_get_size,
                        last_get_time,
                        args.progress_width,
                        get_size,
                        &mut avg_get_speed,
                    );
                    last_get_size = get_size;
                    last_get_time = Instant::now();
                }
            }
            Event::WriteProgress(ps) => {
                for p in ps {
                    write_progress.merge_progress(p);
                }
                if last_db_update.elapsed().as_secs() >= 1 {
                    last_db_update = Instant::now();
                    update_progress(&conn, &save_path_str, &write_progress)?;
                }
            }
            Event::ConnectError(id, err) => {
                print!(
                    "\x1b[1A\r\x1B[K\x1b[1A\r\x1B[K线程 {} 连接失败, 错误原因: {:?}\n\n",
                    id, err
                );
            }
            Event::DownloadError(id, err) => {
                print!(
                    "\x1b[1A\r\x1B[K\x1b[1A\r\x1B[K线程 {} 下载失败, 错误原因: {:?}\n\n",
                    id, err
                );
            }
            Event::WriteError(err) => {
                print!(
                    "\x1b[1A\r\x1B[K\x1b[1A\r\x1B[K写入文件失败, 错误原因: {:?}\n\n",
                    err
                );
            }
            _ => {}
        }
    }
    handle.join().unwrap();
    update_progress(&conn, &save_path_str, &write_progress)?;
    draw_progress::draw_progress(
        start,
        info.file_size,
        &get_progress,
        last_get_size,
        last_get_time,
        args.progress_width,
        get_progress.total(),
        &mut avg_get_speed,
    );
    Ok(())
}
