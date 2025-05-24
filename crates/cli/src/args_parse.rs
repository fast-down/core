use clap::Parser;
use color_eyre::Result;
use config::{Config, Environment, File};
use reqwest::header::{HeaderMap, HeaderName};
use std::{env, str::FromStr, time::Duration};

/// 超级快的下载器
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// 要下载的URL
    #[arg(required = true)]
    pub url: String,

    /// 强制覆盖已有文件
    #[arg(short, long = "allow-overwrite")]
    pub force: bool,

    /// 不强制覆盖已有文件
    #[arg(long = "no-allow-overwrite")]
    pub no_force: bool,

    /// 断点续传
    #[arg(short = 'c', long = "continue")]
    pub resume: bool,

    /// 不断点续传
    #[arg(long = "no-continue")]
    pub no_resume: bool,

    /// 保存目录
    #[arg(short = 'd', long = "dir")]
    pub save_folder: Option<String>,

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
    #[arg(long)]
    pub download_buffer_size: Option<usize>,

    /// 写入缓冲区大小 (单位: B)
    #[arg(long)]
    pub write_buffer_size: Option<usize>,

    /// 进度条显示宽度
    #[arg(long)]
    pub progress_width: Option<usize>,

    /// 重试间隔 (单位: ms)
    #[arg(long)]
    pub retry_gap: Option<u64>,

    /// 模拟浏览器行为
    #[arg(long)]
    pub browser: bool,

    /// 不模拟浏览器行为
    #[arg(long)]
    pub no_browser: bool,

    /// 全部确认
    #[arg(short, long)]
    pub yes: bool,

    /// 不全部确认
    #[arg(long)]
    pub no_yes: bool,

    /// 全部拒绝
    #[arg(long)]
    pub no: bool,

    /// 不全部拒绝
    #[arg(long)]
    pub no_no: bool,

    /// 详细输出
    #[arg(short, long)]
    pub verbose: bool,

    /// 不详细输出
    #[arg(long)]
    pub no_verbose: bool,
}

#[derive(Debug)]
pub struct Args {
    pub url: String,
    pub force: bool,
    pub resume: bool,
    pub save_folder: String,
    pub threads: usize,
    pub file_name: Option<String>,
    pub proxy: Option<String>,
    pub headers: HeaderMap,
    pub download_buffer_size: usize,
    pub write_buffer_size: usize,
    pub progress_width: usize,
    pub retry_gap: Duration,
    pub browser: bool,
    pub yes: bool,
    pub no: bool,
    pub verbose: bool,
}

impl Args {
    pub fn parse() -> Result<Args> {
        let cli = Cli::parse();
        let mut args = Args {
            url: cli.url,
            force: false,
            resume: false,
            save_folder: ".".to_string(),
            threads: 32,
            file_name: cli.file_name,
            proxy: None,
            headers: HeaderMap::new(),
            download_buffer_size: 8 * 1024,
            write_buffer_size: 8 * 1024 * 1024,
            progress_width: 50,
            retry_gap: Duration::from_millis(500),
            browser: true,
            yes: false,
            no: false,
            verbose: false,
        };
        let self_config_path = env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .map(|p| p.join("config.toml"));
        let mut config = Config::builder();
        if let Some(config_path) = self_config_path {
            config = config.add_source(File::from(config_path).required(false));
        }
        let config = config
            .add_source(File::with_name("fast-down.toml").required(false))
            .add_source(Environment::with_prefix("FD"))
            .build()?;

        // 从General部分获取配置
        if let Ok(value) = config.get_bool("General.force") {
            args.force = value;
        }
        if let Ok(value) = config.get_bool("General.resume") {
            args.resume = value;
        }
        if let Ok(value) = config.get_string("General.save_folder") {
            args.save_folder = value;
        }
        if let Ok(value) = config.get_int("General.threads") {
            args.threads = value.try_into()?;
        }
        if let Ok(value) = config.get_string("General.proxy") {
            if !value.is_empty() {
                args.proxy = Some(value);
            }
        }
        if let Ok(value) = config.get_int("General.download_buffer_size") {
            args.download_buffer_size = value.try_into()?;
        }
        if let Ok(value) = config.get_int("General.write_buffer_size") {
            args.write_buffer_size = value.try_into()?;
        }
        if let Ok(value) = config.get_int("General.progress_width") {
            args.progress_width = value.try_into()?;
        }
        if let Ok(value) = config.get_int("General.retry_gap") {
            args.retry_gap = Duration::from_millis(value.try_into()?);
        }
        if let Ok(value) = config.get_bool("General.browser") {
            args.browser = value;
        }
        if let Ok(value) = config.get_bool("General.yes") {
            args.yes = value;
        }
        if let Ok(value) = config.get_bool("General.no") {
            args.no = value;
        }
        if let Ok(value) = config.get_bool("General.verbose") {
            args.verbose = value;
        }

        // 处理Headers部分
        if let Ok(table) = config.get_table("Headers") {
            for (key, value) in table {
                let value_str = value.to_string();
                match HeaderName::from_str(&key) {
                    Ok(header_name) => match value_str.parse() {
                        Ok(header_value) => {
                            args.headers.insert(header_name, header_value);
                        }
                        Err(e) => {
                            eprintln!(
                                "无法解析请求头值\n请求头: {}: {}\n错误原因: {:?}",
                                key, value_str, e
                            );
                        }
                    },
                    Err(e) => {
                        eprintln!("无法解析请求头名称\n请求头: {}\n错误原因: {:?}", key, e);
                    }
                }
            }
        }

        // 命令行参数覆盖配置文件设置
        if cli.force {
            args.force = true;
        } else if cli.no_force {
            args.force = false;
        }
        if cli.resume {
            args.resume = true;
        } else if cli.no_resume {
            args.resume = false;
        }
        if let Some(value) = cli.save_folder {
            args.save_folder = value;
        }
        if let Some(value) = cli.threads {
            args.threads = value;
        }
        if let Some(value) = cli.proxy {
            args.proxy = Some(value);
        }
        if let Some(value) = cli.download_buffer_size {
            args.download_buffer_size = value;
        }
        if let Some(value) = cli.write_buffer_size {
            args.write_buffer_size = value;
        }
        if let Some(value) = cli.progress_width {
            args.progress_width = value;
        }
        if let Some(value) = cli.retry_gap {
            args.retry_gap = Duration::from_millis(value);
        }
        if cli.browser {
            args.browser = true;
        } else if cli.no_browser {
            args.browser = false;
        }
        if cli.yes {
            args.yes = true;
        } else if cli.no_yes {
            args.yes = false;
        }
        if cli.no {
            args.no = true;
        } else if cli.no_no {
            args.no = false;
        }
        if cli.verbose {
            args.verbose = true;
        } else if cli.no_verbose {
            args.verbose = false;
        }

        // 处理命令行指定的请求头
        for header in cli.headers {
            let parts: Vec<_> = header.splitn(2, ':').map(|t| t.trim()).collect();
            if parts.len() != 2 {
                eprintln!("请求头格式错误: {}", header);
                continue;
            }
            args.headers
                .insert(HeaderName::from_str(parts[0])?, parts[1].parse()?);
        }

        Ok(args)
    }
}
