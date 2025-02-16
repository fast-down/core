import type { Chunk } from "./workerpool.ts";

export interface FetchChunksOptions {
  url: string;
  headers: HeadersInit;
  chunks: Chunk[];
  signal: AbortSignal;
}

export async function* fetchChunks(
  data: FetchChunksOptions
): AsyncGenerator<Uint8Array, void, unknown> {
  for (const chunk of data.chunks) {
    const r = await fetch(data.url, {
      headers: {
        ...data.headers,
        Range: `bytes=${chunk.start}-${chunk.end}`,
      },
      signal: data.signal,
      priority: "high",
    });
    if (r.status !== 206) throw new Error(`不支持 Range 头`);
    yield r.bytes();
  }
}
