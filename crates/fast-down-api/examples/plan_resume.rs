//! Minimal example: resume an interrupted download.
//!
//! Runs a download, cancels it part-way, inspects the on-disk `.fd` state, then
//! resumes it from where it stopped.
//!
//! Run: cargo run -p fast-down-api --example plan_resume

#![allow(clippy::pedantic)]

use fast_down_api::{
    Event, PartialConfig, StartMode, State, TerminationReason, create_cancellation_token,
    create_channel,
};
use std::time::Duration;
use url::Url;

const URL: &str = "http://speedtest.tele2.net/10MB.zip";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = std::env::temp_dir().join("fd_plan_resume");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out)?;

    let config = PartialConfig {
        save_dir: Some(out.clone()),
        filename: Some("file.bin".into()),
        parse_filename: Some(false),
        threads: Some(8),
        ..Default::default()
    };
    let url = Url::parse(URL)?;

    // 1. Start a download and cancel it once 30% has landed, leaving a
    //    `.part`/`.fd` pair behind.
    let (tx, rx) = create_channel();
    let token = create_cancellation_token();
    let ready = State::new(url.clone(), config.clone())
        .build(tx.clone())
        .await?;
    let fd_path = ready
        .config_path()
        .map(std::path::Path::to_path_buf)
        .expect("a fresh build decides the .fd path when overwrite is set");

    let run = ready.start(StartMode::Auto, tx, token.clone());
    let mut cancelled = false;
    tokio::pin!(run);
    tokio::select! {
        () = &mut run => {}
        () = async {
            while let Ok(ev) = rx.recv().await {
                if let Event::Progress(pr) = ev
                    && pr.percent > 30.0
                {
                    token.cancel();
                    cancelled = true;
                    break;
                }
            }
        } => {}
    }
    // Let the run finish unwinding after cancellation.
    run.await;
    println!("interrupted at 30%: {cancelled}");

    // 2. Inspect the persisted state without touching the network. `State::load`
    //    reads the `.fd`; `Record` is the file's in-memory form.
    if let Ok(state) = State::load(&fd_path).await {
        let record = &state.record;
        println!(
            "on disk: {} byte-ranges, {} bytes fetched",
            record.progress().len(),
            record.downloaded()
        );
    }

    // 3. Resume: load the `.fd`, then start in `Resume` mode. `build` re-checks
    //    the remote identity and the `.part`, so a changed remote is caught here
    //    rather than corrupting the output.
    let (tx, rx) = create_channel();
    let token = create_cancellation_token();
    let ready = State::load(&fd_path).await?.build(tx.clone()).await?;
    println!("resuming: {:?}", ready.resume_outcome());

    let drain = tokio::spawn(async move {
        let mut reason = TerminationReason::Failed;
        while let Ok(ev) = rx.recv().await {
            match ev {
                Event::Resumed { progress, .. } => {
                    let got: u64 = progress.iter().map(|r| r.end - r.start).sum();
                    println!("resuming from {got} bytes");
                }
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

    // A short settle so the final `.part`/`.fd` flush from step 1 is complete.
    tokio::time::sleep(Duration::from_millis(100)).await;

    ready.start(StartMode::Resume, tx, token).await;
    println!("ended: {:?}", drain.await?);

    Ok(())
}
