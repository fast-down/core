import { expect, test } from "bun:test";
import { download } from "./index.ts";
import { fileSha256 } from "./fileSha256.ts";

test(
  "下载 ISO 文件测试",
  async () => {
    const url =
      "https://mirrors.tuna.tsinghua.edu.cn/debian-cd/12.9.0-live/amd64/iso-hybrid/debian-live-12.9.0-amd64-kde.iso";
    const dirPath = ".";
    const threads = 32;

    console.time("下载 ISO 文件测试");
    const outputPath = await download({ url, dirPath, threads });
    console.timeEnd("下载 ISO 文件测试");

    console.time("计算文件 sha256");
    const fileHash = await fileSha256(outputPath);
    console.log(fileHash);
    console.timeEnd("计算文件 sha256");
    expect(fileHash).toBe(
      "6f6a087a4a8326fdb55971d95855f1185f5b547dfe92cf770da4ec555b763d3f"
    );
  },
  { timeout: 5 * 60 * 1000 }
);

test(
  "断点续传测试",
  async () => {
    const url =
      "https://mirrors.tuna.tsinghua.edu.cn/debian-cd/12.9.0-live/amd64/iso-hybrid/debian-live-12.9.0-amd64-kde.iso";
    const dirPath = ".";
    const threads = 32;

    console.time("下载 ISO 文件测试");
    const outputPath = await download({
      url,
      dirPath,
      threads,
      endChunk: 9,
    });
    await download({
      url,
      dirPath,
      threads,
      startChunk: 10,
      endChunk: 19,
    });
    await download({
      url,
      dirPath,
      threads,
      startChunk: 20,
    });
    console.timeEnd("下载 ISO 文件测试");

    console.time("计算文件 sha256");
    const fileHash = await fileSha256(outputPath);
    console.log(fileHash);
    console.timeEnd("计算文件 sha256");
    expect(fileHash).toBe(
      "6f6a087a4a8326fdb55971d95855f1185f5b547dfe92cf770da4ec555b763d3f"
    );
  },
  { timeout: 5 * 60 * 1000 }
);
