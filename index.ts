import { join } from "path";
import { type Chunk, downloadChunk } from "./workerpool.ts";
import fs from "fs/promises";
import { Command } from "commander";

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
}

export async function download({
  url,
  dirPath,
  threads,
  chunkSize = 10 * 1024 * 1024,
  headers = {},
  startChunk = 0,
  endChunk = Infinity,
}: DownloadOptions) {
  const info = await getURLInfo({
    url,
    headers,
    startChunk,
    endChunk,
  });
  const { contentLength, filename, canUseRange } = info;
  url = info.url;

  dirPath = join(process.cwd(), dirPath);
  try {
    await fs.mkdir(dirPath, { recursive: true });
  } catch {}
  const filePath = join(dirPath, filename);

  console.log(
    `下载 URL：${url}\n文件名：${filename}\n文件大小：${formatFileSize(
      contentLength
    )}`
  );
  if (!contentLength || !canUseRange) {
    console.log("不支持多线程下载，正在单线程下载中...");
    const response = await fetch(url, { headers });
    await Bun.write(filePath, response, { createPath: true });
    console.log("下载完成");
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
        console.log(`分块 ${result.origin.start} - ${result.origin.end} 完成`);
        writeCount--;
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
  const version = "0.1.7";
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
    .option("--headers <string>", "请求头", "{}")
    .option("-c, --chunk-size <number>", "块大小", 10 * 1024 * 1024 + "");
  program.parse();
  const options = program.opts();
  try {
    await download({
      url: program.args[0],
      dirPath: options.dir || "./",
      threads: parseInt(options.threads) || 32,
      startChunk: parseInt(options.start) || 0,
      endChunk: parseInt(options.end) || Infinity,
      headers: JSON.parse(options.headers) || {},
      chunkSize: parseInt(options.chunkSize) || 10 * 1024 * 1024,
    });
  } catch (e) {
    if (e instanceof Error) console.error(e.message);
    else console.error(e);
    exit(1);
  }
}

if (import.meta.main) {
  main();
}
