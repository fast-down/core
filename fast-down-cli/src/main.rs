use clap::Parser;
use color_eyre::eyre::{eyre, Result};
use fast_down::{DownloadOptions, Event, MergeProgress, Progress, Total};
use fast_down_cli::{build_headers, sha256_file};
use reqwest::{blocking::Client, Proxy};
use std::{
    io::{self, Write},
    path::Path,
    time::Instant,
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

    /// 写入分块大小 (单位: B)
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    pub write_chunk_size: usize,

    /// 校验文件 sha256 校验和
    #[arg(long)]
    pub sha256: Option<String>,
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
    let (rx, handle) = fast_down::download(DownloadOptions {
        url: info.final_url,
        threads: args.threads,
        save_path: &save_path,
        can_fast_download: info.can_fast_download,
        file_size: info.file_size,
        client,
        get_chunk_size: args.get_chunk_size,
        write_chunk_size: args.write_chunk_size,
        download_chunks: vec![0..info.file_size],
    })?;

    let mut progress: Vec<Progress> = Vec::new();
    for e in rx {
        match e {
            Event::DownloadProgress(p) => {
                progress.merge_progress(p);
                draw_progress(info.file_size, &progress);
            }
            _ => {}
        }
    }
    handle.join().unwrap();
    println!("下载完成，用时 {:?}", start.elapsed());

    if let Some(target) = args.sha256 {
        let sha256 = sha256_file(save_path.to_str().unwrap())?;
        if sha256 != target {
            return Err(eyre!("文件 sha256 不同，下载内容有误"));
        }
    }
    // assert_eq!(progress, vec![0..info.file_size]);
    println!("{:#?}", progress);

    Ok(())
}

fn draw_progress(total: usize, progress: &Vec<Progress>) {
    let downloaded = progress.total();
    print!("\r{}%", downloaded as f64 / total as f64 * 100.0);
}
