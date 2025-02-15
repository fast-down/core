import { expect, test } from "bun:test";
import { download } from "./index.ts";
import { fileSha256 } from "./fileSha256.ts";

test(
  "Download ISO file test",
  async () => {
    const url =
      "https://mirrors.tuna.tsinghua.edu.cn/debian-cd/12.9.0-live/amd64/iso-hybrid/debian-live-12.9.0-amd64-kde.iso";
    const dirPath = ".";
    const threads = 32;

    console.time("Download ISO file test");
    const outputPath = await download({ url, dirPath, threads });
    console.timeEnd("Download ISO file test");

    console.time("Calculate file hash test");
    const fileHash = await fileSha256(outputPath);
    console.log(fileHash);
    console.timeEnd("Calculate file hash test");
    expect(fileHash).toBe(
      "6f6a087a4a8326fdb55971d95855f1185f5b547dfe92cf770da4ec555b763d3f"
    );
  },
  { timeout: 5 * 60 * 1000 }
);

test(
  "Breakpoint continuation test",
  async () => {
    const url =
      "https://mirrors.tuna.tsinghua.edu.cn/debian-cd/12.9.0-live/amd64/iso-hybrid/debian-live-12.9.0-amd64-kde.iso";
    const dirPath = ".";
    const threads = 32;

    console.time("Download ISO file test");

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
    console.timeEnd("Download ISO file test");

    console.time("Calculate file hash test");
    const fileHash = await fileSha256(outputPath);
    console.timeEnd("Calculate file hash test");
    console.log(fileHash);
    expect(fileHash).toBe(
      "6f6a087a4a8326fdb55971d95855f1185f5b547dfe92cf770da4ec555b763d3f"
    );
  },
  { timeout: 5 * 60 * 1000 }
);
