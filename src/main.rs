use fast_down::{
    download::{self, DownloadOptions},
    download_progress::DownloadProgress,
    format_file_size, merge_progress,
};
use reqwest::header::{HeaderMap, HeaderValue};

fn main() {
    let mut headers = HeaderMap::new();
    headers.insert("User-Agent", HeaderValue::from_static("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36"));

    let mut progress: Vec<DownloadProgress> = Vec::new();

    let  r = download::download(DownloadOptions {
        url: "https://mirrors.tuna.tsinghua.edu.cn/debian-cd/12.9.0-live/amd64/iso-hybrid/debian-live-12.9.0-amd64-kde.iso",
        threads: 32,
        // save_folder: r"C:\Users\Administrator\Desktop\新建文件夹 (3)",
        save_folder: r".\downloads",
        file_name: None,
        headers: Some(headers), 
        proxy: None,
    })    .unwrap();
    println!(
        "文件名: {}\n文件大小: {} ({} 字节) \n文件路径: {}",
        r.file_name,
        format_file_size::format_file_size(r.file_size as f64),
        r.file_size,
        r.file_path.to_str().unwrap()
    );

    while let Ok(e) = r.rx.recv() {
        merge_progress::merge_progress(&mut progress, e);
        draw_progress(r.file_size, &progress);
    }
}

fn draw_progress(total: usize, progress: &[DownloadProgress]) {
    let downloaded: usize = progress.iter().map(|x| x.size()).sum();
    print!("\r{:.2}", downloaded as f64 / total as f64 * 100.0);
}
