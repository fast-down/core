use clap::{Parser, Subcommand};
use color_eyre::Result;
use config::{Config, Environment, File};
use reqwest::header::{HeaderMap, HeaderName};
use std::{env, str::FromStr, time::Duration};

/// 超级快的下载器
#[derive(Parser, Debug)]
#[command(name = "fast-down")]
#[command(author, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Parser, Debug)]
#[command(name = "fast-down")]
#[command(author, about)]
struct CliDefault {
    #[command(flatten)]
    cmd: DownloadCli,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// 下载文件 (默认)
    Download(DownloadCli),
    /// 清除已下载完成的链接
    Clean,
    /// 更新 fast-down
    Update,
}

#[derive(clap::Args, Debug)]
struct DownloadCli {
    /// 保存目录
    #[arg(short = 'd', long = "dir")]
    save_folder: Option<String>,

    /// 下载线程数
    #[arg(short, long)]
    threads: Option<usize>,

    /// 代理地址 (格式: http://proxy:port 或 socks5://proxy:port)
    #[arg(short, long = "all-proxy")]
    proxy: Option<String>,

    /// 自定义请求头 (可多次使用)
    #[arg(short = 'H', long = "header", value_name = "Key: Value")]
    headers: Vec<String>,

    /// 写入缓冲区大小 (单位: B)
    #[arg(long)]
    write_buffer_size: Option<usize>,

    /// 写入通道长度
    #[arg(long)]
    write_channel_size: Option<usize>,

    /// 重试间隔 (单位: ms)
    #[arg(long)]
    retry_gap: Option<u64>,

    /// 模拟浏览器行为
    #[arg(long)]
    browser: bool,

    /// 不模拟浏览器行为
    #[arg(long)]
    no_browser: bool,
}

#[derive(Debug)]
pub enum Args {
    Download(DownloadArgs),
    Update,
    Clean,
}

#[derive(Debug, Clone)]
pub struct DownloadArgs {
    pub save_folder: String,
    pub threads: usize,
    pub proxy: Option<String>,
    pub headers: HeaderMap,
    pub write_buffer_size: usize,
    pub write_channel_size: usize,
    pub retry_gap: Duration,
    pub browser: bool,
}

impl Args {
    pub fn parse() -> Result<Args> {
        match Cli::try_parse().or_else(|err| match err.kind() {
            clap::error::ErrorKind::InvalidSubcommand
            | clap::error::ErrorKind::UnknownArgument
            | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
                CliDefault::try_parse().map(|cli_default| Cli {
                    command: Commands::Download(cli_default.cmd),
                })
            }
            _ => Err(err),
        }) {
            Ok(cli) => match cli.command {
                Commands::Download(cli) => {
                    let mut args = DownloadArgs {
                        save_folder: ".".to_string(),
                        threads: 8,
                        proxy: None,
                        headers: HeaderMap::new(),
                        write_buffer_size: 8 * 1024 * 1024,
                        write_channel_size: 1024,
                        retry_gap: Duration::from_millis(500),
                        browser: true,
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
                    if let Ok(value) = config.get_int("General.write_buffer_size") {
                        args.write_buffer_size = value.try_into()?;
                    }
                    if let Ok(value) = config.get_int("General.write_channel_size") {
                        args.write_channel_size = value.try_into()?;
                    }
                    if let Ok(value) = config.get_int("General.retry_gap") {
                        args.retry_gap = Duration::from_millis(value.try_into()?);
                    }
                    if let Ok(value) = config.get_bool("General.browser") {
                        args.browser = value;
                    }
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
                                    eprintln!(
                                        "无法解析请求头名称\n请求头: {}\n错误原因: {:?}",
                                        key, e
                                    );
                                }
                            }
                        }
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
                    if let Some(value) = cli.write_buffer_size {
                        args.write_buffer_size = value;
                    }
                    if let Some(value) = cli.write_channel_size {
                        args.write_channel_size = value;
                    }
                    if let Some(value) = cli.retry_gap {
                        args.retry_gap = Duration::from_millis(value);
                    }
                    if cli.browser {
                        args.browser = true;
                    } else if cli.no_browser {
                        args.browser = false;
                    }
                    for header in cli.headers {
                        let parts: Vec<_> = header.splitn(2, ':').map(|t| t.trim()).collect();
                        if parts.len() != 2 {
                            eprintln!("请求头格式错误: {}", header);
                            continue;
                        }
                        args.headers
                            .insert(HeaderName::from_str(parts[0])?, parts[1].parse()?);
                    }
                    Ok(Args::Download(args))
                }
                Commands::Update => Ok(Args::Update),
                Commands::Clean => Ok(Args::Clean),
            },
            Err(err) => err.exit(),
        }
    }
}
