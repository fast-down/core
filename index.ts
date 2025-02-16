import { join } from "path";
import { type Chunk, downloadChunk } from "./workerpool.ts";
import fs from "fs/promises";
import { Command } from "commander";
import cliProgress from "cli-progress";
import { formatFileSize } from "./formatFileSize.ts";
import { getURLInfo } from "./getUrlInfo.ts";
import { exit } from "process";

export interface DownloadOptions {
  url: string;
  dirPath: string;
  threads: number;
  chunkSize?: number;
  headers?: HeadersInit;
  startChunk?: number;
  endChunk?: number;
  filename?: string;
  httpProxy?: string;
  httpsProxy?: string;
  onProgress?(current: number, total: number): void;
  onInfo?(info: {
    contentLength: number;
    filename: string;
    filePath: string;
    canUseRange: boolean;
    canFastDownload: boolean;
    url: string;
    threads: number;
  }): void;
}

export async function download({
  url,
  dirPath,
  threads,
  chunkSize = 10 * 1024 * 1024,
  headers = {},
  startChunk = 0,
  endChunk = Infinity,
  filename,
  httpProxy,
  httpsProxy,
  onProgress,
  onInfo,
}: DownloadOptions) {
  process.env.NODE_TLS_REJECT_UNAUTHORIZED = "0";
  if (httpProxy) process.env.HTTP_PROXY = httpProxy;
  if (httpsProxy) process.env.HTTPS_PROXY = httpsProxy;

  const info = await getURLInfo({
    url,
    headers,
    startChunk,
    endChunk,
    filename,
  });
  const { contentLength, canUseRange } = info;
  url = info.url;
  filename = info.filename;

  dirPath = join(process.cwd(), dirPath);
  try {
    await fs.mkdir(dirPath, { recursive: true });
  } catch {}
  const filePath = join(dirPath, filename);

  const canFastDownload = !!contentLength && canUseRange;
  onInfo?.({
    contentLength,
    filename,
    filePath,
    canUseRange,
    canFastDownload,
    url,
    threads: canFastDownload ? threads : 1,
  });
  let current = 0;
  onProgress?.(0, contentLength);
  if (!canFastDownload) {
    const response = await fetch(url, {
      headers,
      priority: "high",
    });
    if (!response.body) throw new Error("Response body is not readable");
    const file = Bun.file(filePath);
    const writer = file.writer();
    for await (const chunk of response.body) {
      writer.write(chunk);
      current += chunk.byteLength;
      onProgress?.(current, contentLength);
    }
    await writer.end();
    return filePath;
  }

  const file = await createFile(filePath);
  try {
    const chunkCount = Math.ceil(contentLength / chunkSize);
    const chunks: Chunk[] = Array.from({ length: chunkCount }, (_, i) => ({
      start: startChunk + i * chunkSize,
      end: startChunk + Math.min((i + 1) * chunkSize, contentLength) - 1,
    }));
    let writeReslove: (value: void | PromiseLike<void>) => void;
    let writeCount = 0;
    const writePromise = new Promise<void>(
      (resolve) => (writeReslove = resolve)
    );
    const downloadPromise = downloadChunk({
      threads,
      url,
      headers,
      chunks,
      async onProgress(result) {
        writeCount++;
        while (true) {
          try {
            await file.write(
              result.data,
              0,
              result.data.byteLength,
              result.origin.start
            );
            break;
          } catch (e) {
            console.error(e);
          }
        }
        writeCount--;
        current += result.data.byteLength;
        onProgress?.(current, contentLength);
        if (writeCount === 0) writeReslove();
      },
      maxRetries: Infinity,
    });
    await Promise.all([writePromise, downloadPromise]);
  } finally {
    await file.close();
  }
  return filePath;
}

async function createFile(filePath: string) {
  try {
    return await fs.open(filePath, "r+");
  } catch (e) {
    return fs.open(filePath, "w");
  }
}

async function main() {
  const program = new Command();
  const version = "0.1.8";
  console.log(`fast-down v${version}`);
  program
    .name("fast-down")
    .description("超快的多线程下载器")
    .version(version, "-v, --version", "显示当前版本")
    .argument("<string>", "要下载的 URL")
    .option("-t, --threads <number>", "线程数", "32")
    .option("-s, --start <number>", "起始块", "0")
    .option("-e, --end <number>", "结束块", "Infinity")
    .option("-d, --dir <string>", "下载目录", "./")
    .option("-f, --filename <string>", "文件名")
    .option("--http-proxy <string>", "http 代理")
    .option("--https-proxy <string>", "https 代理")
    .option("--headers <string>", "请求头", "{}")
    .option("-c, --chunk-size <number>", "块大小", 10 * 1024 * 1024 + "");
  program.parse();
  const options = program.opts();
  const bar = new cliProgress.SingleBar(
    {
      format:
        "|{bar}| {percentage}% | {value} / {total} | 速度：{speed} | 剩余时间：{eta_formatted} | 用时：{duration_formatted}",
      hideCursor: true,
      formatValue(v, _, type) {
        switch (type) {
          case "value":
          case "total":
            return formatFileSize(v);
          default:
            return v + "";
        }
      },
    },
    cliProgress.Presets.shades_classic
  );
  let oldTime = Date.now();
  let oldSize = 0;
  await download({
    url: program.args[0],
    dirPath: options.dir || "./",
    threads: parseInt(options.threads) || 32,
    startChunk: parseInt(options.start) || 0,
    endChunk: parseInt(options.end) || Infinity,
    headers: JSON.parse(options.headers) || {},
    chunkSize: parseInt(options.chunkSize) || 10 * 1024 * 1024,
    filename: options.filename,
    httpProxy: options.httpProxy,
    httpsProxy: options.httpsProxy,
    onInfo(info) {
      console.log(
        `下载 URL：${info.url}
文件名：${info.filename}
文件路径：${info.filePath}
文件大小：${formatFileSize(info.contentLength)}
线程数：${info.threads}`
      );
      bar.start(info.contentLength, 0, { speed: "N/A" });
      oldTime = Date.now();
    },
    onProgress(current) {
      const dTime = Date.now() - oldTime;
      if (dTime >= 1000) {
        bar.update(current, {
          speed: formatFileSize((current - oldSize) / (dTime / 1000)) + "/s",
        });
        oldTime = Date.now();
        oldSize = current;
      } else bar.update(current);
    },
  });
  bar.stop();
}

if (import.meta.main) {
  main().catch((e) => {
    if (e instanceof Error) console.error(e.message);
    else console.error(e);
    exit(1);
  });
}
