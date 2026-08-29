//! Minimal example: resume an interrupted download with `plan_resume()`.
//!
//! Run: cargo run -p fast-down-api --example plan_resume

#![allow(clippy::pedantic)]

use fast_down_api::{
    Event, PartialConfig, TerminationReason, create_cancellation_token, create_channel,
    peek_resume, plan, plan_resume,
};
use std::time::Duration;
use url::Url;

const URL: &str = "http://speedtest.tele2.net/10MB.zip";

#[tokio::main]
async fn main() {
    let out = std::env::temp_dir().join("fd_plan_resume");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();
    let pc = PartialConfig {
        save_dir: Some(out.clone()),
        filename: Some("file.bin".into()),
        parse_filename: Some(false),
        threads: Some(8),
        ..Default::default()
    };

    // Interrupt a download to leave a partial `.part`/`.fd` behind: drive a
    // plan and cancel it once 30% has landed.
    let (tx, rx) = create_channel();
    let token = create_cancellation_token();
    let p = plan(Url::parse(URL).unwrap(), pc.clone(), tx, token.clone())
        .await
        .unwrap();
    let mut cancelled = false;
    tokio::select! {
        () = p.start() => {}
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
    println!("interrupted at 30%: {cancelled}");

    // The final `.fd` flush races the cancellation; wait (bounded) for it.
    let tmp = out.join("file.bin.part");
    let fd = out.join("file.bin.fd");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !fd.exists() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // `peek_resume`: pure inspection of the on-disk state, no download.
    if let Ok(state) = peek_resume(&tmp).await {
        let done: u64 = state.get_progress().iter().map(|r| r.end - r.start).sum();
        println!(
            "peek_resume: {} byte-ranges, {done} bytes fetched",
            state.get_progress().len()
        );
    }

    // `plan_resume`: plan that continues from the partial `.part`, then start.
    let (tx, rx) = create_channel();
    let token = create_cancellation_token();
    let p2 = plan_resume(&tmp, Some(Url::parse(URL).unwrap()), pc, tx, token)
        .await
        .unwrap();
    p2.start().await;
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
    println!("ended: {reason:?}");
}
