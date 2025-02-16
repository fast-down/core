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
  canUseRange: boolean;
}
export async function getURLInfo({
  url,
  headers,
  startChunk,
  endChunk,
}: getURLInfoOptions): Promise<getURLInfoResult> {
  let filename = "";
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
      redirect: "follow",
    });
    if (r.status === 206) canUseRange = true;
    abortController.abort();
    const contentLength = +r.headers.get("content-length")!;
    const disposition = r.headers.get("content-disposition");
    if (disposition)
      filename = contentDisposition.parse(disposition).parameters.filename;
    else filename = decodeURIComponent(basename(new URL(url).pathname));
    filename = sanitize(filename);
    if (!filename) filename = "download";
    if (contentLength) return { filename, contentLength, url, canUseRange };
  }
  return { filename, contentLength: 0, url, canUseRange };
}
