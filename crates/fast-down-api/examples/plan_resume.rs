//! Minimal example: resume an interrupted download.
//!
//! Runs a download, cancels it part-way, inspects the on-disk `.fd` state, then
//! resumes it from where it stopped.
//!
//! Run: cargo run -p fast-down-api --example plan_resume

#![allow(clippy::pedantic)]

use fast_down_api::{Ctx, Event, PartialConfig, StartMode, State, create_channel};
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
    let ctx = Ctx::new();
    let ready = State::new(url.clone(), config.clone()).build(&tx).await?;
    let fd_path = ready
        .fd_path
        .clone()
        .expect("a fresh build decides the .fd path when overwrite is set");

    let handle = ready.start(StartMode::Auto, tx, &ctx);
    let mut cancelled = false;
    // Drain events; cancel once enough has landed. `start` already runs the
    // download in the background, so this is the foreground work.
    while let Ok(ev) = rx.recv().await {
        if let Event::Progress(pr) = ev
            && pr.push.percent > 30.0
        {
            handle.abort();
            cancelled = true;
            break;
        }
    }
    // Let the run finish unwinding after cancellation.
    handle.await;
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
    let ctx = Ctx::new();
    let ready = State::load(&fd_path).await?.build(&tx).await?;
    println!("resuming: {:?}", ready.outcome);

    let handle = ready.start(StartMode::Resume, tx, &ctx);

    // Drain until the terminal event, then take the outcome from the handle.
    while let Ok(ev) = rx.recv().await {
        match ev {
            Event::Resumed { progress, .. } => {
                let got: u64 = progress.iter().map(|r| r.end - r.start).sum();
                println!("resuming from {got} bytes");
            }
            Event::Progress(pr) => println!("{:5.1}%  {} B/s", pr.push.percent, pr.push.bps),
            Event::Renamed(path) => println!("renamed -> {}", path.display()),
            Event::Terminated(_) => break,
            other => println!("{other:?}"),
        }
    }
    println!("ended: {:?}", handle.await);

    Ok(())
}
