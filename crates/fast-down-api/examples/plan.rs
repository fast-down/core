//! Minimal example: two-phase `plan()` + `start()`.
//!
//! Run: cargo run -p fast-down-api --example plan

#![allow(clippy::pedantic)]

use fast_down_api::{
    Event, PartialConfig, TerminationReason, create_cancellation_token, create_channel, plan,
};
use url::Url;

const URL: &str = "http://speedtest.tele2.net/10MB.zip";

#[tokio::main]
async fn main() {
    let (tx, rx) = create_channel();
    let token = create_cancellation_token();
    let p = plan(
        Url::parse(URL).unwrap(),
        PartialConfig {
            save_dir: Some(std::env::temp_dir().join("fd_plan")),
            filename: Some("file.bin".into()),
            parse_filename: Some(false),
            threads: Some(8),
            ..Default::default()
        },
        tx,
        token,
    )
    .await
    .unwrap();

    // `plan()` prefetched metadata and computed paths but wrote nothing to disk.
    println!(
        "remote: {} bytes, final_path = {}, can_fastdown = {}",
        p.info().size,
        p.final_path().display(),
        p.info().fast_download
    );

    p.start().await; // blocks until the run ends, emitting Terminated itself
    let mut reason = TerminationReason::Failed;
    while let Ok(ev) = rx.recv().await {
        match ev {
            Event::Progress(pr) => println!("{:5.1}%  {} B/s", pr.percent, pr.bps),
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
