use crate::curl::worker::{DataSignal, Op, TaskHandle, options};
use bytes::Bytes;
use fast_pull::{ProgressEntry, Puller, ReadStream, SliceOrBytes};
use futures::TryStream;
use kanal::{AsyncReceiver, AsyncSender};
use std::io;
use std::sync::Arc;
use thiserror::Error;

#[derive(Clone)]
pub struct Options {
    pub url: String,
    pub data_channel_cap: usize,
    pub headers: Vec<String>,
}

#[derive(Clone)]
pub struct WorkerFetcher {
    tx_ops: AsyncSender<Op>,
    options: Arc<Options>,
}

impl WorkerFetcher {
    pub async fn create(tx_ops: AsyncSender<Op>, options: Options) -> Result<Self, anyhow::Error> {
        Ok(Self {
            tx_ops,
            options: Arc::new(options),
        })
    }
}

#[derive(Error, Debug)]
pub enum CreateTaskError {
    #[error("failed to sending new task request to worker")]
    Send,
    #[error("failed to acquire TaskHandle")]
    Recv,
}

struct WorkerPuller {
    tx_ops: AsyncSender<Op>,
    rx_data: AsyncReceiver<Bytes>,
    signal: Arc<DataSignal>,
    th: TaskHandle,
}

impl ReadStream for WorkerPuller {
    type Error = io::Error;

    async fn read_with<'a, F, Fut, Ret>(&'a mut self, read_fn: F) -> Result<Ret, Self::Error>
    where
        F: FnOnce(SliceOrBytes<'a>) -> Fut,
        Fut: Future<Output=Ret>
    {
        let data = self.rx_data.recv().await.map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "data channel closed"))?;
        Ok(read_fn(data.into()).await)
    }
}

impl Puller for WorkerFetcher {
    type StreamError = io::Error;
    type Error = CreateTaskError;

    async fn init_read(
        &self,
        maybe_entry: Option<&ProgressEntry>,
    ) -> Result<impl ReadStream<Error = Self::StreamError> + Send + Unpin, CreateTaskError>
    {
        let (tx_data, rx_data) = kanal::bounded(self.options.data_channel_cap);
        let signal: Arc<DataSignal> = Default::default();
        let (tx_ret, ret) = oneshot::channel();
        let mut headers = curl::easy::List::new();
        for header in &self.options.headers {
            headers.append(header).unwrap();
        }
        if let Some(entry) = maybe_entry {
            headers
                .append(&format!(
                    "{}: bytes={}-{}",
                    http::header::RANGE,
                    entry.start,
                    entry.end
                ))
                .unwrap();
        }
        self.tx_ops
            .send(Op::New(
                tx_data,
                options::New {
                    headers,
                    signal: signal.clone(),
                    url: self.options.url.clone(),
                    extra: None,
                },
                tx_ret,
            ))
            .await
            .map_err(|e| CreateTaskError::Send)?;
        let th = ret.await.map_err(|_| CreateTaskError::Recv)?;
        Ok(WorkerPuller {
            tx_ops: self.tx_ops.clone(),
            rx_data: rx_data.to_async(),
            signal,
            th,
        })
    }
}
