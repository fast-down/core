use fast_down_rust::{download, DownloadOptions};

#[tokio::main]
async fn main() {
    let r = download(DownloadOptions {
        url: "http://127.0.0.1:8080/19044.1288.211006-0501.21h2_release_svc_refresh_CLIENT_LTSC_EVAL_x64FRE_zh-cn.iso",
        threads: 32,
        save_folder: "./test/",
        file_name: None,
        headers: None,
        proxy: None,
        on_info: Some(Box::new(|e| {
            println!("{:#?}", e);
        })),
        on_progress: Some(Box::new(|e| {
            println!("{:#?}", e);
        })),
    })
    .await
    .unwrap();
    println!("{:#?}", r);
}
