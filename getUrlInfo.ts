import contentDisposition from "content-disposition";
import sanitize from "sanitize-filename";
import { basename } from "path";

export interface getURLInfoOptions {
  url: string;
  headers?: HeadersInit;
  startChunk: number;
  endChunk: number;
}
export interface getURLInfoResult {
  filename: string;
  contentLength: number;
  url: string;
}
export async function getURLInfo({
  url,
  headers,
  startChunk,
  endChunk,
}: getURLInfoOptions): Promise<getURLInfoResult> {
  let filename = "";
  for (let i = 3; i; i--) {
    const abortController = new AbortController();
    const r = await fetch(url, {
      headers: headers,
      signal: abortController.signal,
      redirect: "follow",
    });
    url = r.url;
    abortController.abort();
    const contentLength = Math.min(
      +r.headers.get("content-length")!,
      endChunk - startChunk + 1
    );
    const disposition = r.headers.get("content-disposition");
    if (disposition)
      filename = contentDisposition.parse(disposition).parameters.filename;
    else filename = decodeURIComponent(basename(new URL(url).pathname));
    filename = sanitize(filename);
    if (!filename) filename = "download";
    if (contentLength) return { filename, contentLength, url };
    else
      console.log(
        `文件长度：${contentLength}，文件名：${filename}，正在重试……`
      );
  }
  return { filename, contentLength: 0, url };
}
