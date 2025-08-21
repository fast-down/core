extern crate alloc;
use fast_pull::{ProgressEntry, Puller, ReadStream, SliceOrBytes};
use alloc::format;
use bytes::Bytes;
use reqwest::{Client, Response, header};
use url::Url;

#[derive(Clone)]
pub struct ReqwestPuller {
    pub(crate) client: Client,
    url: Url,
}

impl ReqwestPuller {
    pub fn new(url: Url, client: Client) -> Self {
        Self { client, url }
    }
}

impl Puller for ReqwestPuller {
    type StreamError = reqwest::Error;
    type Error = reqwest::Error;
    async fn init_read(
        &self,
        maybe_entry: Option<&ProgressEntry>,
    ) -> Result<impl ReadStream<Error = Self::Error> + Send + Unpin, Self::Error> {
        let builder = self.client.get(self.url.clone());
        let response = if let Some(entry) = maybe_entry {
            builder.header(
                header::RANGE,
                format!("bytes={}-{}", entry.start, entry.end - 1),
            )
        } else {
            builder
        }
            .send()
            .await?;
        Ok(ReqwestPullStream(response))
    }
}
struct ReqwestPullStream(Response);
impl ReadStream for ReqwestPullStream {
    type Error = reqwest::Error;

    async fn read_with<'a, F, Fut, Ret>(&mut self, read_fn: F) -> Result<Ret, Self::Error>
    where
        F: FnOnce(SliceOrBytes<'a>) -> Fut,
        Fut: Future<Output = Ret>,
    {
        Ok(if let Some(chunk) = self.0.chunk().await? {
            read_fn(chunk.into()).await
        } else {
            read_fn(Bytes::new().into()).await
        })
    }
}
