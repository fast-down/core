use fast_down::{
    download::{self, DownloadOptions},
    download_progress::DownloadProgress,
    format_file_size, merge_progress,
};
use reqwest::header::{HeaderMap, HeaderValue};

#[tokio::main]
async fn main() {
    let mut headers = HeaderMap::new();
    headers.insert("User-Agent", HeaderValue::from_static("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36"));

    let mut progress: Vec<DownloadProgress> = Vec::new();

    let mut r = download::download(DownloadOptions {
        url: "https://software.download.prss.microsoft.com/dbazure/Win11_24H2_Pro_Chinese_Simplified_x64.iso?t=3514b994-009a-4529-a6bb-3a070a437d0e&P1=1741505777&P2=601&P3=2&P4=HLBLBRGDVsInamMxArey2xTV48WZfRzmLg2%2fZKOaPGMf8wa%2bPsUA6iT4XonZ1cUG9GH009fsZCO3EEzHZp8bA0%2fRxrgFn9qtu%2bldLK24Nj%2bwlZ8rsEKeq6BTIDWGBncEOpGNMBo45WYPGRNX9q2BqJN4JuKLAJjuPxVO24ywxyZxNm7NC3ClA3SRdKtZnAyxBCvLjyR%2fdx839Vfcptya6%2fXZUHYZJjv8D4gMh5O%2bew%2boLtkxlcDCp%2fP8grauRVgMBUKxUCMCkvfkk6Pf1VXu8oX%2fQzUfoY3eQQqps8MWrVMwDpvcQz2jnUsnoLj0o8%2fMT9EFEPHqOvSMyCgogrxAJg%3d%3d",
        threads: 32,
        save_folder: "./test/",
        file_name: None,
        headers: Some(headers),
        proxy: None,
    })
    .await
    .unwrap();
    println!(
        "文件名: {}\n文件大小: {} ({} 字节) \n文件路径: {}",
        r.file_name,
        format_file_size::format_file_size(r.file_size as f64),
        r.file_size,
        r.file_path.to_str().unwrap()
    );

    while let Some(e) = r.rx.recv().await {
        merge_progress::merge_progress(&mut progress, e);
        draw_progress(r.file_size, &progress);
    }
}

fn draw_progress(total: u64, downloaded: &[DownloadProgress]) {
    print!("\r{}: {:?}", total, downloaded);
}
