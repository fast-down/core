//! Minimal example: two-phase resolve (`State::build`) + run (`Ready::start`).
//!
//! Shows inspecting the resolved plan before committing to it.
//!
//! Run: cargo run -p fast-down-api --example plan

#![allow(clippy::pedantic)]

use fast_down_api::{
    Event, PartialConfig, ResumeOutcome, StartMode, State, TerminationReason,
    create_cancellation_token, create_channel,
};
use url::Url;

const URL: &str = "http://speedtest.tele2.net/10MB.zip";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (tx, rx) = create_channel();
    let token = create_cancellation_token();

    let ready = State::new(
        Url::parse(URL)?,
        PartialConfig {
            save_dir: Some(std::env::temp_dir().join("fd_plan")),
            filename: Some("file.bin".into()),
            parse_filename: Some(false),
            threads: Some(8),
            ..Default::default()
        },
    )
    .build(tx.clone())
    .await?;

    // `build` prefetched metadata and computed paths but wrote nothing to disk.
    println!(
        "remote: {} bytes, final_path = {}, fast_download = {}",
        ready.info().size,
        ready.final_path().display(),
        ready.info().fast_download
    );
    match ready.resume_outcome() {
        ResumeOutcome::Fresh => println!("nothing to continue; will download the whole file"),
        ResumeOutcome::Resumable => println!("will continue a previous run"),
        ResumeOutcome::Mismatch(e) => println!("stale state; cannot resume: {e}"),
    }

    let drain = tokio::spawn(async move {
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
        reason
    });

    // Start with `Auto`: resume when possible, otherwise download fresh.
    ready.start(StartMode::Auto, tx, token).await;
    println!("ended: {:?}", drain.await?);

    Ok(())
}
