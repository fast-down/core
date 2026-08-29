//! Minimal example: one-shot `download()`.
//!
//! Run: cargo run -p fast-down-api --example download

#![allow(clippy::pedantic)]

use fast_down_api::{
    Event, PartialConfig, TerminationReason, create_cancellation_token, create_channel, download,
};
use url::Url;

const URL: &str = "http://speedtest.tele2.net/10MB.zip";

#[tokio::main]
async fn main() {
    let (tx, rx) = create_channel();
    let token = create_cancellation_token();
    download(
        Url::parse(URL).unwrap(),
        PartialConfig {
            save_dir: Some(std::env::temp_dir().join("fd_download")),
            filename: Some("file.bin".into()),
            parse_filename: Some(false),
            ..Default::default()
        },
        tx,
        token,
    );

    let mut reason = TerminationReason::Failed;
    while let Ok(ev) = rx.recv().await {
        match ev {
            Event::Progress(p) => println!("{:5.1}%  {} B/s", p.percent, p.bps),
            Event::Renamed(path) => println!("renamed -> {}", path.display()),
            Event::Terminated(r) => {
                reason = r;
                break;
            }
            _ => {}
        }
    }
    println!("ended: {reason:?}");
}
