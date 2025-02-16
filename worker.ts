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
  if (!r.body) throw new Error(`body 为空`);
  let buffer: Uint8Array = new Uint8Array();
  let i = 0;
  for await (const respondChunk of r.body) {
    const temp = new Uint8Array(buffer.length + respondChunk.length);
    temp.set(buffer);
    temp.set(respondChunk, buffer.length);
    buffer = temp;
    let offset = 0;
    while (true) {
      const chunk = data.chunks[i];
      const chunkSize = chunk.end - chunk.start + 1;
      if (buffer.byteLength < chunkSize) break;
      yield buffer.slice(offset, offset + chunkSize);
      offset += chunkSize;
      i++;
    }
    if (offset) buffer = buffer.slice(offset);
  }
}
