import { fetchChunks } from "./worker.ts";

export interface Chunk {
  start: number;
  end: number;
}

export interface DownloadChunkOptions {
  threads: number;
  url: string;
  headers: HeadersInit;
  chunks: Chunk[];
  onProgress(data: { origin: Chunk; data: Uint8Array; index: number }): void;
  maxRetries: number;
}

export function downloadChunk({
  threads,
  url,
  headers,
  chunks: data,
  onProgress,
  maxRetries = 3,
}: DownloadChunkOptions) {
  if (data.length < 1) return Promise.resolve([]);
  if (threads < 1) throw new Error("threadCount must be greater than 0");
  return new Promise<void>((resolve, reject) => {
    const baseChunkCount = Math.floor(data.length / threads);
    const remainingChunks = data.length % threads;
    const workerPool: WorkerData[] = [];
    let activeWorkers = 0;
    for (let i = 0; i < threads; i++, activeWorkers++) {
      const startChunk = i * baseChunkCount + Math.min(i, remainingChunks);
      if (startChunk >= data.length) break;
      const endChunk =
        startChunk + baseChunkCount + (i < remainingChunks ? 1 : 0);
      const abortController = new AbortController();
      workerPool[i] = {
        worker: fetchChunks({
          url,
          headers,
          chunks: data.slice(startChunk, endChunk),
          signal: abortController.signal,
        }),
        abortController,
        startChunk,
        endChunk,
        currentChunk: startChunk,
        retryCount: 0,
        stolen: false,
      } as WorkerData;
      const messageHandle = (result: Uint8Array) => {
        onProgress({
          index: workerPool[i].currentChunk,
          origin: data[workerPool[i].currentChunk],
          data: result,
        });
        workerPool[i].retryCount = 0;
        workerPool[i].currentChunk++;
        if (workerPool[i].currentChunk < workerPool[i].endChunk) return;
        if (workerPool[i].stolen) {
          workerPool[i].abortController.abort();
          workerPool[i].abortController = new AbortController();
          workerPool[i].stolen = false;
        }
        let maxRemain = 0;
        let targetWorkerIndex = -1;
        for (let j = 0; j < workerPool.length; j++) {
          if (!(j in workerPool) || j === i) continue;
          const w = workerPool[j];
          const remaining = w.endChunk - w.currentChunk - 1;
          if (remaining > maxRemain) {
            maxRemain = remaining;
            targetWorkerIndex = j;
          }
        }
        if (maxRemain < 1) {
          delete workerPool[i];
          activeWorkers--;
          if (activeWorkers === 0) resolve();
          return;
        }
        const targetWorker = workerPool[targetWorkerIndex];
        targetWorker.stolen = true;
        const splitPoint = Math.ceil(
          (targetWorker.currentChunk + targetWorker.endChunk) / 2
        );
        workerPool[i].endChunk = targetWorker.endChunk;
        workerPool[i].currentChunk =
          workerPool[i].startChunk =
          targetWorker.endChunk =
            splitPoint;
        workerPool[i].worker = fetchChunks({
          url,
          headers,
          chunks: data.slice(splitPoint, workerPool[i].endChunk),
          signal: workerPool[i].abortController.signal,
        });
        addEventListener(workerPool[i].worker, messageHandle, errorHandel);
      };
      const errorHandel = (err: unknown) => {
        if (workerPool[i].retryCount >= maxRetries) {
          for (let i = 0; i < workerPool.length; i++)
            if (i in workerPool) workerPool[i].abortController.abort();
          return reject(err);
        }
        workerPool[i].retryCount++;
        workerPool[i].stolen = false;
        workerPool[i].worker = fetchChunks({
          url,
          headers,
          chunks: data.slice(
            workerPool[i].currentChunk,
            workerPool[i].endChunk
          ),
          signal: workerPool[i].abortController.signal,
        });
        addEventListener(workerPool[i].worker, messageHandle, errorHandel);
      };
      addEventListener(workerPool[i].worker, messageHandle, errorHandel);
    }
  });
}

async function addEventListener(
  stream: AsyncGenerator<Uint8Array, void, unknown>,
  messageHandle: (e: Uint8Array) => void,
  errorHandle: (e: unknown) => void
) {
  try {
    for await (const data of stream) messageHandle(data);
  } catch (e) {
    if (e instanceof DOMException && e.name === "AbortError") return;
    errorHandle(e);
  }
}

interface WorkerData {
  worker: AsyncGenerator<Uint8Array, void, unknown>;
  abortController: AbortController;
  startChunk: number;
  endChunk: number;
  currentChunk: number;
  retryCount: number;
  stolen: boolean;
}
