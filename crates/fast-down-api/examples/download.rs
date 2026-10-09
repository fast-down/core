//! Minimal example: one-shot download via `State::build` + `Ready::start`.
//!
//! Run: cargo run -p fast-down-api --example download

#![allow(clippy::pedantic)]

use fast_down_api::{Ctx, Event, PartialConfig, StartMode, State, create_channel};
use url::Url;

const URL: &str = "http://speedtest.tele2.net/10MB.zip";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (tx, rx) = create_channel();
    let ctx = Ctx::new();

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
    .build(&tx)
    .await?;

    println!("final path: {}", ready.final_path.display());

    // 2. Run it. `Auto` continues from a previous run when possible, otherwise
    //    downloads the whole file. `start` returns immediately; events flow on
    //    `rx`, and the contract guarantees `Terminated` is the last one.
    let handle = ready.start(StartMode::Auto, tx, &ctx);

    // 3. Drain the event stream until the terminal event.
    while let Ok(ev) = rx.recv().await {
        match ev {
            Event::Progress(p) => println!("{:5.1}%  {} B/s", p.push.percent, p.push.bps),
            Event::Renamed(path) => println!("renamed -> {}", path.display()),
            Event::Terminated(_) => break,
            other => println!("{other:?}"),
        }
    }

    // 4. The outcome comes from the handle (it is a `Future<Output =
    //    TerminationReason>`); `Terminated` above only marks the end of the
    //    stream.
    println!("ended: {:?}", handle.await);

    Ok(())
}
