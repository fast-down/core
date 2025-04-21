use clap::Parser;
use color_eyre::eyre::{eyre, Result};
use fast_down::{DownloadOptions, DownloadResult, Event, MergeProgress, Progress, Total};
use fast_down_cli::{build_headers, format_time};
use reqwest::{blocking::Client, Proxy};
use std::{
    io::{self, Write},
    path::Path,
    time::{Duration, Instant},
};

/// 超级快的下载器
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// 强制覆盖已有文件
    #[arg(short, long = "allow-overwrite", default_value_t = false)]
    pub force: bool,

    /// 要下载的URL
    #[arg(required = true)]
    pub url: String,

    /// 保存目录
    #[arg(short = 'd', long = "dir", default_value = ".")]
    pub save_folder: String,

    /// 下载线程数
    #[arg(short, long, default_value_t = 32)]
    pub threads: usize,

    /// 自定义文件名
    #[arg(short = 'o', long = "out")]
    pub file_name: Option<String>,

    /// 代理地址 (格式: http://proxy:port 或 socks5://proxy:port)
    #[arg(short, long = "all-proxy")]
    pub proxy: Option<String>,

    /// 自定义请求头 (可多次使用)
    #[arg(short = 'H', long = "header", value_name = "Key: Value")]
    pub headers: Vec<String>,

    /// 下载分块大小 (单位: B)
    #[arg(long, default_value_t = 8 * 1024)]
    pub get_chunk_size: usize,
    /// 进度条显示宽度
    #[arg(long, default_value_t = 50)]
    pub progress_width: usize,

    /// 重试间隔 (单位: ms)
    #[arg(long, default_value_t = 500)]
    pub retry_gap: u64,
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

    let info = fast_down::get_url_info(&args.url, &client)?;
    let threads = if info.can_fast_download {
        args.threads
    } else {
        1
    };
    let save_path =
        Path::new(&args.save_folder).join(args.file_name.as_ref().unwrap_or(&info.file_name));

    println!(
        "文件名: {}\n文件大小: {} ({} 字节) \n文件路径: {}\n线程数量: {}",
        info.file_name,
        fast_down::format_file_size(info.file_size as f64),
        info.file_size,
        save_path.to_str().unwrap(),
        threads
    );

    if save_path.exists() && !args.force {
        print!("文件已存在，是否覆盖？(y/N) ");
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

    let start = Instant::now();
    let result = fast_down::download(DownloadOptions {
        url: info.final_url,
        threads: args.threads,
        save_path: &save_path,
        can_fast_download: info.can_fast_download,
        file_size: info.file_size,
        client,
        get_chunk_size: args.get_chunk_size,
        download_chunks: vec![0..info.file_size],
        retry_gap: Duration::from_millis(args.retry_gap),
    })?;

    let mut get_progress: Vec<Progress> = Vec::new();
    let mut last_get_size = 0;
    let mut last_get_time = Instant::now();
    let mut avg_get_speed = 0.0;

    let mut last_progress_update = Instant::now();

    let DownloadResult { event_chan: rx, handler } = result;

    print!("\n\n");
    for e in rx {
        match e {
            Event::DownloadProgress(p) => {
                get_progress.merge_progress(p);
                if last_progress_update.elapsed().as_millis() > 50 {
                    last_progress_update = Instant::now();
                    let get_size = get_progress.total();
                    draw_progress(
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

    handler.unwrap().join();

    Ok(())
}

fn draw_progress(
    start: Instant,
    total: usize,
    get_progress: &Vec<Progress>,
    last_get_size: usize,
    last_get_time: Instant,
    progress_width: usize,
    get_size: usize,
    avg_get_speed: &mut f64,
) {
    // 计算瞬时速度
    let get_elapsed_ms = last_get_time.elapsed().as_millis();
    let get_speed = if get_elapsed_ms > 0 {
        (get_size - last_get_size) as f64 * 1e3 / get_elapsed_ms as f64
    } else {
        0.0
    };

    // 更新下载速度队列
    const ALPHA: f64 = 0.9;
    *avg_get_speed = *avg_get_speed * ALPHA + get_speed * (1.0 - ALPHA);

    // 计算百分比
    let get_percent = (get_size as f64 / total as f64) * 1e2;

    // 计算已用时间
    let elapsed = start.elapsed();

    // 下载剩余时间
    let get_remaining = if *avg_get_speed > 0.0 {
        (total as f64 - get_size as f64) / *avg_get_speed
    } else {
        0.0
    };

    // 格式化文件大小
    let formatted_get_size = fast_down::format_file_size(get_size as f64);
    let formatted_total_size = fast_down::format_file_size(total as f64);
    let formatted_get_speed = fast_down::format_file_size(*avg_get_speed);
    let formatted_get_remaining = format_time(get_remaining as u64);
    let formatted_elapsed = format_time(elapsed.as_secs());

    // 创建合并的进度条
    const BLOCK_CHARS: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

    // 构建进度条字符串
    let mut bar_str = String::with_capacity(progress_width);

    // 计算每个位置对应的字节范围
    let bytes_per_position = total as f64 / progress_width as f64;

    // 对于每个进度条位置，计算其对应的字节范围
    for pos in 0..progress_width {
        let pos_start = (pos as f64 * bytes_per_position) as usize;
        let pos_end = ((pos + 1) as f64 * bytes_per_position) as usize;

        // 计算这个位置有多少字节已经被下载/写入
        let mut get_completed_bytes = 0;

        // 计算下载完成的字节
        for p in get_progress {
            // 如果进度块与当前位置没有重叠，跳过
            if p.end <= pos_start || p.start >= pos_end {
                continue;
            }

            // 计算重叠部分
            let overlap_start = p.start.max(pos_start);
            let overlap_end = p.end.min(pos_end);
            get_completed_bytes += overlap_end - overlap_start;
        }

        // 计算这个位置的完成度（0-8）
        let position_total = pos_end - pos_start;
        if position_total > 0 && get_completed_bytes > 0 {
            // 计算完成度
            let get_ratio = get_completed_bytes as f64 / position_total as f64;
            let fill_level = (get_ratio * 8.0).round() as usize;
            bar_str.push(BLOCK_CHARS[fill_level]);
        } else {
            bar_str.push(BLOCK_CHARS[0]);
        }
    }

    print!(
        "\x1b[1A\x1b[1A\r\x1B[K|{}| {:>6.2}% ({:>8}/{})\n\x1B[K已用时间: {} | 速度: {:>8}/s | 剩余: {}\n",
        bar_str,
        get_percent,
        formatted_get_size,
        formatted_total_size,
        formatted_elapsed,
        formatted_get_speed,
        formatted_get_remaining
    );
}
