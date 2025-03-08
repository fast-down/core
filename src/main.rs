use fast_down::{
    download::{self, DownloadOptions},
    download_progress::DownloadProgress,
    merge_progress,
};
use reqwest::header::{HeaderMap, HeaderValue};

#[tokio::main]
async fn main() {
    let mut headers = HeaderMap::new();
    headers.insert("User-Agent", HeaderValue::from_static("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36"));

    let mut progress: Vec<DownloadProgress> = Vec::new();

    let mut r = download::download(DownloadOptions {
        url: "https://mirrors.tuna.tsinghua.edu.cn/debian-cd/12.9.0-live/amd64/iso-hybrid/debian-live-12.9.0-amd64-kde.iso",
        threads: 32,
        save_folder: "./test/",
        file_name: None,
        headers: Some(headers),
        proxy: None,
    })
    .await
    .unwrap();
    // println!("{:#?}", r);

    while let Some(e) = r.rx.recv().await {
        merge_progress::merge_progress(&mut progress, e);
        draw_progress(r.file_size, &progress);
    }
}

fn draw_progress(total: usize, downloaded: &[DownloadProgress]) {
    print!("\r{}: {:?}", total, downloaded);
}
