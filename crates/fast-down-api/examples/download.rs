//! Minimal example: one-shot download via `State::build` + `Ready::start`.
//!
//! Run: cargo run -p fast-down-api --example download

#![allow(clippy::pedantic)]

use fast_down_api::{
    Event, PartialConfig, StartMode, State, TerminationReason, create_cancellation_token,
    create_channel,
};
use url::Url;

const URL: &str = "http://speedtest.tele2.net/10MB.zip";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (tx, rx) = create_channel();
    let token = create_cancellation_token();

    // 1. Resolve against the remote. This prefetches metadata and probes the
    //    disk, but writes nothing.
    let ready = State::new(
        Url::parse(URL)?,
        PartialConfig {
            save_dir: Some(std::env::temp_dir().join("fd_download")),
            filename: Some("file.bin".into()),
            parse_filename: Some(false),
            ..Default::default()
        },
    )
    .build(tx.clone())
    .await?;

    println!("final path: {}", ready.final_path().display());

    // 2. Drain the event stream on another task.
    let drain = tokio::spawn(async move {
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
        reason
    });

    // 3. Run it. `Auto` continues from a previous run when possible, otherwise
    //    downloads the whole file.
    ready.start(StartMode::Auto, tx, token).await;
    println!("ended: {:?}", drain.await?);

    Ok(())
}
