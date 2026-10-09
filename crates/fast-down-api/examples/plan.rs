//! Minimal example: two-phase resolve (`State::build`) + run (`Ready::start`).
//!
//! Shows inspecting the resolved plan before committing to it.
//!
//! Run: cargo run -p fast-down-api --example plan

#![allow(clippy::pedantic)]

use fast_down_api::{
    Ctx, Event, PartialConfig, ResumeOutcome, StartMode, State, create_channel,
};
use url::Url;

const URL: &str = "http://speedtest.tele2.net/10MB.zip";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (tx, rx) = create_channel();
    let ctx = Ctx::new();

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
    .build(&tx)
    .await?;

    // `build` prefetched metadata and computed paths but wrote nothing to disk.
    println!(
        "remote: {} bytes, final_path = {}, fast_download = {}",
        ready.info.size,
        ready.final_path.display(),
        ready.info.fast_download
    );
    match &ready.outcome {
        ResumeOutcome::Fresh => println!("nothing to continue; will download the whole file"),
        ResumeOutcome::Resumable => println!("will continue a previous run"),
        ResumeOutcome::Mismatch(e) => println!("stale state; cannot resume: {e}"),
    }

    // Start with `Auto`: resume when possible, otherwise download fresh.
    let handle = ready.start(StartMode::Auto, tx, &ctx);

    // Drain until the terminal event, then take the outcome from the handle.
    while let Ok(ev) = rx.recv().await {
        match ev {
            Event::Progress(pr) => println!("{:5.1}%  {} B/s", pr.push.percent, pr.push.bps),
            Event::Renamed(path) => println!("renamed -> {}", path.display()),
            Event::Terminated(_) => break,
            other => println!("{other:?}"),
        }
    }
    println!("ended: {:?}", handle.await);

    Ok(())
}
