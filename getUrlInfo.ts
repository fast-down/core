import contentDisposition from "content-disposition";
import sanitize from "sanitize-filename";
import { basename } from "path";

export interface getURLInfoOptions {
  url: string;
  headers?: HeadersInit;
  startChunk: number;
  endChunk: number;
  filename?: string;
  proxy?: string;
}
export interface getURLInfoResult {
  filename: string;
  contentLength: number;
  url: string;
  canUseRange: boolean;
}
export async function getURLInfo({
  url,
  headers,
  startChunk,
  endChunk,
  filename,
  proxy,
}: getURLInfoOptions): Promise<getURLInfoResult> {
  let canUseRange = false;
  for (let i = 3; i; i--) {
    const abortController = new AbortController();
    const r = await fetch(url, {
      headers: {
        ...headers,
        Range: `bytes=${startChunk}-${
          Number.isFinite(endChunk) ? endChunk : ""
        }`,
      },
      signal: abortController.signal,
      priority: "high",
      proxy,
    });
    if (r.status === 206) canUseRange = true;
    abortController.abort();
    const contentLength = +r.headers.get("content-length")!;
    if (!filename) {
      const disposition = r.headers.get("content-disposition");
      if (disposition)
        filename = contentDisposition.parse(disposition).parameters.filename;
      else filename = decodeURIComponent(basename(new URL(url).pathname));
    }
    filename = sanitize(filename);
    if (!filename) filename = "download";
    if (contentLength) return { filename, contentLength, url, canUseRange };
  }
  return { filename: filename!, contentLength: 0, url, canUseRange };
}
