import type { Chunk } from "./workerpool.ts";

export interface FetchChunksOptions {
  url: string;
  headers: HeadersInit;
  chunks: Chunk[];
  signal: AbortSignal;
  proxy?: string;
}

export async function* fetchChunks(
  data: FetchChunksOptions
): AsyncGenerator<Uint8Array, void, unknown> {
  const r = await fetch(data.url, {
    headers: {
      ...data.headers,
      Range: `bytes=${data.chunks[0].start}-${data.chunks.at(-1)!.end}`,
    },
    signal: data.signal,
    priority: "high",
    proxy: data.proxy,
  });
  if (r.status !== 206) throw new Error(`不支持 Range 头`);
  if (!r.body) throw new Error(`响应体为空`);
  let buffer: Uint8Array = new Uint8Array();
  let i = 0;
  for await (const respondChunk of r.body) {
    buffer = mergeUint8Array(buffer, respondChunk);
    while (i < data.chunks.length) {
      const chunk = data.chunks[i];
      const chunkSize = chunk.end - chunk.start + 1;
      if (buffer.length < chunkSize) break;
      yield buffer.slice(0, chunkSize);
      buffer = buffer.slice(chunkSize);
      i++;
    }
  }
}

function mergeUint8Array(a: Uint8Array, b: Uint8Array) {
  const t = new Uint8Array(a.length + b.length);
  t.set(a);
  t.set(b, a.length);
  return t;
}
