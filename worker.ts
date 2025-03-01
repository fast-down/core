import type { Chunk } from "./workerpool.ts";

export interface FetchChunksOptions {
  url: string;
  headers: HeadersInit;
  chunks: Chunk[];
  signal: AbortSignal;
  proxy?: string;
}

export async function* fetchChunks(
  options: FetchChunksOptions
): AsyncGenerator<Uint8Array, void, unknown> {
  const { url, headers, chunks, signal, proxy } = options;
  const response = await fetch(url, {
    headers: {
      ...headers,
      Range: `bytes=${chunks[0].start}-${chunks.at(-1)!.end}`,
    },
    signal,
    priority: "high",
    proxy,
  });
  if (response.status !== 206) throw new Error(`服务器不支持 Range 请求`);
  if (!response.body) throw new Error(`响应体为空`);
  let buffer = new Uint8Array();
  let chunkIndex = 0;
  for await (const responseChunk of response.body) {
    buffer = mergeUint8Arrays(buffer, responseChunk);
    while (chunkIndex < chunks.length) {
      const { start, end } = chunks[chunkIndex];
      const chunkSize = end - start + 1;
      if (buffer.length < chunkSize) break;
      yield buffer.subarray(0, chunkSize);
      buffer = buffer.subarray(chunkSize);
      chunkIndex++;
    }
  }
}
function mergeUint8Arrays(a: Uint8Array, b: Uint8Array) {
  const merged = new Uint8Array(a.length + b.length);
  merged.set(a);
  merged.set(b, a.length);
  return merged;
}
