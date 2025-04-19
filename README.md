# fast-down 快下

![GitHub last commit (branch)](https://img.shields.io/github/last-commit/share121/fast-down/main)
[![Rust](https://github.com/share121/fast-down/workflows/Test/badge.svg)](https://github.com/share121/fast-down/actions)
[![Latest version](https://img.shields.io/crates/v/fast-down.svg)](https://crates.io/crates/fast-down)
[![Documentation](https://docs.rs/fast-down/badge.svg)](https://docs.rs/fast-down)
![License](https://img.shields.io/crates/l/fast-down.svg)

`fast-down` 全网最快多线程下载库

![cli界面](cli.png)

## 优势

1. 全网最快多线程下载库
2. 无锁
3. 安全的 Rust 代码
4. 超强任务调度算法：自研 [fast-steal](https://github.com/share121/fast-steal) 任务窃取算法
5. 跨平台，Windows、Linux、Mac OS 都支持
6. 错误自动重试
7. 进度跟踪
8. 性能优化：高效的内存使用，可配置缓冲区大小
9. 完整性验证：支持多种哈希算法的文件完整性校验
10. 高度可配置：支持自定义线程数、缓冲区大小等

```powershell
> ./fast-down.exe -h
超级快的下载器命令行界面

Usage: fast-down.exe [OPTIONS] <URL>

Arguments:
  <URL>  要下载的URL

Options:
  -f, --allow-overwrite                      强制覆盖已有文件
  -d, --dir <SAVE_FOLDER>                    保存目录 [default: .]
  -t, --threads <THREADS>                    下载线程数 [default: 32]
  -o, --out <FILE_NAME>                      自定义文件名
  -p, --all-proxy <PROXY>                    代理地址 (格式: http://proxy:port 或 socks5://proxy:port)
  -H, --header <Key: Value>                  自定义请求头 (可多次使用)
      --get-chunk-size <GET_CHUNK_SIZE>      下载分块大小 (单位: B) [default: 8192]
      --write-chunk-size <WRITE_CHUNK_SIZE>  写入分块大小 (单位: B) [default: 8388608]
      --progress-width <PROGRESS_WIDTH>      进度条显示宽度 [default: 50]
  -h, --help                                 Print help
  -V, --version                              Print version
```
