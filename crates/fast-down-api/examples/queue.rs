//! Concurrency-limited queue: several downloads at once.
//!
//! Run: cargo run -p fast-down-api --example queue

#![allow(clippy::pedantic)]

use fast_down_api::{
    Event, PartialConfig, Queue, QueueEvent, StartMode, State, create_queue_channel,
};
use url::Url;

const URLS: [&str; 3] = [
    "http://speedtest.tele2.net/10MB.zip",
    "http://speedtest.tele2.net/10MB.zip",
    "http://speedtest.tele2.net/10MB.zip",
];

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (tx, rx) = create_queue_channel::<i32>();
    // At most two downloads run at once; the rest wait their turn.
    let queue = Queue::<i32>::new(2, tx);

    for (i, url) in URLS.iter().enumerate() {
        queue.push(
            i as i32,
            State::new(
                Url::parse(url)?,
                PartialConfig {
                    save_dir: Some(std::env::temp_dir().join("fd_queue")),
                    filename: Some(format!("file-{i}.zip")),
                    parse_filename: Some(false),
                    threads: Some(8),
                    ..Default::default()
                },
            ),
            StartMode::Auto,
        )?;
    }

    // One stream: every task's events, tagged by id.
    drop(queue);
    while let Ok(event) = rx.recv().await {
        match event {
            QueueEvent::Task(id, Event::Terminated(reason)) => {
                println!("task {id:?} ended: {reason:?}");
            }
            QueueEvent::Task(_, Event::Renamed(path)) => {
                println!("renamed -> {}", path.display());
            }
            _ => {}
        }
    }

    Ok(())
}
