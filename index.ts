import { basename, join } from "path";
import { type Chunk, downloadChunk } from "./workerpool.ts";
import fs from "fs/promises";
import { Command } from "commander";
import contentDisposition from "content-disposition";
import sanitize from "sanitize-filename";

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
  async function getURLInfo() {
    while (true) {
      const abortController = new AbortController();
      const r = await fetch(url, {
        headers: headers,
        signal: abortController.signal,
      });
      abortController.abort();
      const contentLength = Math.min(
        +r.headers.get("content-length")!,
        endChunk - startChunk + 1
      );
      let filename: string;
      const disposition = r.headers.get("content-disposition");
      if (disposition)
        filename = contentDisposition.parse(disposition).parameters.filename;
      else filename = decodeURIComponent(basename(new URL(url).pathname));
      filename = sanitize(filename);
      if (!filename) filename = "download";
      if (contentLength) return { filename, contentLength };
      else
        console.log(
          `Content-Length = ${contentLength}, File-Name = ${filename}, retrying...`
        );
    }
  }

  async function createFile(filePath: string) {
    try {
      return await fs.open(filePath, "r+");
    } catch (e) {
      console.error(e);
      return fs.open(filePath, "w");
    }
  }

  dirPath = join(process.cwd(), dirPath);
  const { contentLength, filename } = await getURLInfo();
  try {
    await fs.mkdir(dirPath, { recursive: true });
  } catch (e) {
    console.error(e);
  }
  const filePath = join(dirPath, filename);
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
        console.log(`chunk ${result.origin.start} - ${result.origin.end} done`);
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

async function main() {
  const program = new Command();
  program
    .name("fast-down")
    .description("超快的多线程下载器")
    .version("0.1.3", "-v, --version", "显示当前版本")
    .argument("<string>", "要下载的 URL")
    .option("-t, --threads <number>", "线程数", "32")
    .option("-s, --start <number>", "起始块", "0")
    .option("-e, --end <number>", "结束块", "Infinity")
    .option("-d, --dir <string>", "下载目录", "./")
    .option("--headers <string>", "请求头", "{}");
  program.parse();
  const options = program.opts();
  await download({
    url: program.args[0],
    dirPath: options.dir || "./",
    threads: parseInt(options.threads) || 32,
    startChunk: parseInt(options.start) || 0,
    endChunk: parseInt(options.end) || Infinity,
    headers: JSON.parse(options.headers) || {},
  });
}

if (import.meta.main) {
  main();
}
