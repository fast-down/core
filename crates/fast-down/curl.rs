use std::convert::Infallible;
use std::io::Error;
use fast_down::curl::puller::{Options, WorkerPuller};
use fast_down::curl::worker::multi;
use fast_pull::single::TransferOptions;
use std::thread;
use futures::StreamExt;
use fast_pull::{Event, Puller};

#[actix_rt::main]
async fn main() {
    let (tx_ops, rx_ops) = kanal::unbounded();
    let handle = thread::spawn(move || {
        multi(rx_ops).unwrap();
    });

    let puller = WorkerPuller::create(tx_ops.to_async(), Options {
        url: "http://localhost".to_string(),
        data_channel_cap: 16,
        headers: vec!["user-agent: curl/1.41".to_string()],
    }).await.unwrap();
    let mut data: Vec<u8> = Vec::new();
    let mut result = fast_pull::single::transfer(puller.init_read(None).await.unwrap(), &mut data, TransferOptions {
        retry_gap: Default::default(),
    }).await;
    let mut stream = result.event_chain.stream();
    while let Some(event) = stream.next().await {
        match event {
            Event::Pulling(_) => {}
            Event::PullError(_, _) => {}
            Event::PullStreamError(id, err) => {
                dbg!(id, err);
            }
            Event::PullProgress(_, _) => {}
            Event::PushError(_, _) => {}
            Event::PushStreamError(_, _) => {}
            Event::PushProgress(_, _) => {}
            Event::FlushError(_) => {}
            Event::Finished(_) => {}
        }
    }
    result.join().await.unwrap();
    drop(stream);
    drop(result);

    dbg!(data);
}
