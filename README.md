# Fast Down 快下

[![MIT license](https://img.shields.io/badge/license-MIT-brightgreen.svg)](https://opensource.org/licenses/MIT)
![GitHub last commit (branch)](https://img.shields.io/github/last-commit/share121/fast-down/main)

一个超快的下载器

## 功能

- 支持多线程下载
- 支持多平台（Windows、macOS、Linux）

## 使用方法

```powershell
> .\fast-down.exe -h
fast-down v0.1.9
Usage: fast-down [options] <string>

超快的多线程下载器

Arguments:
  string                     要下载的 URL

Options:
  -v, --version              显示当前版本
  -t, --threads <number>     线程数 (default: "32")
  -s, --start <number>       起始块 (default: "0")
  -e, --end <number>         结束块 (default: "Infinity")
  -d, --dir <string>         下载目录 (default: "./")
  -f, --filename <string>    文件名
  --http-proxy <string>      http 代理
  --https-proxy <string>     https 代理
  --headers <string>         请求头 (default: "{}")
  -c, --chunk-size <number>  块大小 (default: "10485760")
  -h, --help                 display help for command
```
