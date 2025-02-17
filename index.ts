import { Command } from "commander";
import cliProgress from "cli-progress";
import { formatFileSize } from "./formatFileSize.ts";
import { exit } from "process";
import { download } from "./download.ts";

async function main() {
  const program = new Command();
  const version = "0.1.11";
  console.log(`fast-down v${version}`);
  program
    .name("fast-down")
    .description("超快的多线程下载器")
    .version(version, "-v, --version", "显示当前版本")
    .argument("<string>", "要下载的 URL")
    .option("-t, --threads <number>", "线程数", "32")
    .option("-s, --start <number>", "起始块", "0")
    .option("-e, --end <number>", "结束块", "Infinity")
    .option("-d, --dir <string>", "下载目录", "./")
    .option("-f, --filename <string>", "文件名")
    .option("-p, --proxy <string>", "代理")
    .option("--headers <string>", "请求头", "{}")
    .option("-c, --chunk-size <number>", "块大小", 1 * 1024 * 1024 + "");
  program.parse();
  const options = program.opts();
  const bar = new cliProgress.SingleBar(
    {
      format:
        "|{bar}| {percentage}% | {value} / {total} | 速度：{speed} | 剩余时间：{eta_formatted} | 用时：{duration_formatted}",
      hideCursor: true,
      formatValue(v, _, type) {
        switch (type) {
          case "value":
          case "total":
            return formatFileSize(v);
          default:
            return v + "";
        }
      },
    },
    cliProgress.Presets.shades_classic
  );
  let oldTime = Date.now();
  let oldSize = 0;
  await download({
    url: program.args[0],
    dirPath: options.dir || "./",
    threads: parseInt(options.threads) || 32,
    startChunk: parseInt(options.start) || 0,
    endChunk: parseInt(options.end) || Infinity,
    headers: (options.headers && JSON.parse(options.headers)) || {},
    chunkSize: parseInt(options.chunkSize) || 1 * 1024 * 1024,
    filename: options.filename,
    proxy: options.proxy,
    onInfo(info) {
      console.log(
        `下载 URL：${info.url}
文件名：${info.filename}
文件路径：${info.filePath}
文件大小：${formatFileSize(info.contentLength)}
线程数：${info.threads}`
      );
      bar.start(info.contentLength, 0, { speed: "N/A" });
      oldTime = Date.now();
    },
    onProgress(current) {
      const dTime = Date.now() - oldTime;
      if (dTime >= 1000) {
        bar.update(current, {
          speed: formatFileSize((current - oldSize) / (dTime / 1000)) + "/s",
        });
        oldTime = Date.now();
        oldSize = current;
      } else bar.update(current);
    },
  });
  bar.stop();
}

if (import.meta.main) {
  main().catch((e) => {
    if (e instanceof Error) console.error(e.message);
    else console.error(e);
    exit(1);
  });
}
