import { join } from "path";
import { type Chunk, downloadChunk } from "./workerpool.ts";
import fs from "fs/promises";
import { getURLInfo } from "./getUrlInfo.ts";

export interface DownloadOptions {
  url: string;
  dirPath: string;
  threads: number;
  chunkSize?: number;
  headers?: HeadersInit;
  startChunk?: number;
  endChunk?: number;
  filename?: string;
  proxy?: string;
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
  chunkSize = 1 * 1024 * 1024,
  headers = {},
  startChunk = 0,
  endChunk = Infinity,
  filename,
  proxy,
  onProgress,
  onInfo,
}: DownloadOptions) {
  process.env.NODE_TLS_REJECT_UNAUTHORIZED = "0";
  const info = await getURLInfo({
    url,
    headers,
    startChunk,
    endChunk,
    filename,
    proxy,
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
      proxy,
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
      proxy,
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
            if (e instanceof Error) console.error(e.message);
            else console.error(e);
            await Bun.sleep(1000);
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
