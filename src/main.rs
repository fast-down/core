use fast_down_rust::{download, DownloadOptions};

fn main() {
    download(DownloadOptions {
        url: "https://www.google.com",
        threads: 32,
        save_folder: "./test/",
        file_name: None,
        headers: None,
        proxy: None,
    });
}
