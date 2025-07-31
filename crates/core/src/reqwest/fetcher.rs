use crate::WorkerId;
use crate::base::source::{Fetcher, Puller};
use bytes::Bytes;
use http::header::IntoHeaderName;
use reqwest::header::HeaderValue;
use std::ops::Range;

#[derive(Clone)]
pub struct ClientFetcher {
    pub client: reqwest::Client,
    pub method: reqwest::Method,
    pub url: reqwest::Url,
    pub headers: Option<reqwest::header::HeaderMap>,
}

#[derive(Clone)]
pub struct ClientFetcherBuilder {
    client: reqwest::Client,
    url: reqwest::Url,
    method: Option<reqwest::Method>,
    headers: Option<reqwest::header::HeaderMap>,
}

impl ClientFetcherBuilder {
    pub fn method(mut self, method: reqwest::Method) -> Self {
        self.method = Some(method);
        self
    }

    pub fn headers(mut self, headers: reqwest::header::HeaderMap) -> Self {
        self.headers = Some(headers);
        self
    }

    pub fn header<K>(mut self, key: K, value: impl Into<HeaderValue>) -> Self
    where
        K: IntoHeaderName,
    {
        let map = self.headers.get_or_insert_default();
        map.insert(key, value.into());
        self
    }

    pub fn build(self) -> ClientFetcher {
        ClientFetcher {
            client: self.client,
            url: self.url,
            method: self.method.unwrap_or(reqwest::Method::GET),
            headers: self.headers,
        }
    }
}

impl ClientFetcher {
    pub fn builder(client: reqwest::Client, url: reqwest::Url) -> ClientFetcherBuilder {
        ClientFetcherBuilder {
            client,
            url,
            method: Default::default(),
            headers: Default::default(),
        }
    }
}

impl Fetcher for ClientFetcher {
    type Error = reqwest::Error;
    type Puller = reqwest::Response;

    #[inline(always)]
    async fn fetch(
        &self,
        _: WorkerId,
        range: Option<&Range<u64>>,
    ) -> Result<Self::Puller, Self::Error> {
        let mut builder = self.client.request(self.method.clone(), self.url.clone());
        if let Some(headers) = &self.headers {
            builder = builder.headers(headers.clone());
        }
        if let Some(Range { start, end }) = range {
            builder = builder.header(reqwest::header::RANGE, format!("bytes={start}-{end}"));
        }
        builder.send().await
    }

    fn clone(&self) -> Self {
        Clone::clone(self)
    }
}

impl Puller for reqwest::Response {
    type Error = reqwest::Error;

    #[inline(always)]
    async fn pull(&mut self) -> Result<Option<Bytes>, reqwest::Error> {
        self.chunk().await
    }
}

#[cfg(test)]
mod tests {
    use crate::base::source::{Fetcher, Puller};
    use crate::reqwest::fetcher::ClientFetcher;

    #[tokio::test]
    async fn test_reqwest_fetcher() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/endpoint")
            .with_status(200)
            .with_body(b"Hello World")
            .create_async()
            .await;
        let client = reqwest::Client::new();
        let fetcher = ClientFetcher::builder(
            client,
            format!("{}/endpoint", server.url()).parse().unwrap(),
        )
        .build();
        let mut puller = fetcher.fetch(0, None).await.unwrap();
        let mut content = Vec::new();
        while let Some(ref bytes) = puller.pull().await.unwrap() {
            content.extend_from_slice(bytes);
        }
        assert_eq!(content, b"Hello World");
    }
}
