use crate::{
    FileId, ProgressEntry, PullResult, PullStream, RandPuller, SeqPuller,
    http::{HttpError, HttpPuller},
};
use reqwest::{Client, ClientBuilder, Proxy, Response, header::HeaderMap};
use spin::mutex::SpinMutex;
use std::sync::Arc;
use url::Url;

pub fn build_client(
    headers: &HeaderMap,
    proxy: &str,
    accept_invalid_certs: bool,
    accept_invalid_hostnames: bool,
) -> Result<reqwest::Client, reqwest::Error> {
    let mut client = ClientBuilder::new()
        .default_headers(headers.clone())
        .danger_accept_invalid_certs(accept_invalid_certs)
        .danger_accept_invalid_hostnames(accept_invalid_hostnames)
        .http2_adaptive_window(true)
        .brotli(true)
        .gzip(true)
        .deflate(true)
        .zstd(true);
    if !proxy.is_empty() {
        client = client.proxy(Proxy::all(proxy)?);
    }
    let client = client.build()?;
    Ok(client)
}

#[derive(Debug)]
pub struct FastDownPuller {
    inner: HttpPuller<Client>,
    headers: Arc<HeaderMap>,
    proxy: Arc<str>,
    url: Arc<Url>,
    multiplexing: bool,
    accept_invalid_certs: bool,
    accept_invalid_hostnames: bool,
    file_id: FileId,
    resp: Option<Arc<SpinMutex<Option<Response>>>>,
}

pub struct FastDownPullerOptions<'a> {
    pub url: Url,
    pub headers: Arc<HeaderMap>,
    pub proxy: &'a str,
    pub multiplexing: bool,
    pub accept_invalid_certs: bool,
    pub accept_invalid_hostnames: bool,
    pub file_id: FileId,
    pub resp: Option<Arc<SpinMutex<Option<Response>>>>,
}

impl FastDownPuller {
    pub fn new(option: FastDownPullerOptions) -> Result<Self, reqwest::Error> {
        let client = build_client(
            &option.headers,
            option.proxy,
            option.accept_invalid_certs,
            option.accept_invalid_hostnames,
        )?;
        Ok(Self {
            inner: HttpPuller::new(
                option.url.clone(),
                client,
                option.resp.clone(),
                option.file_id.clone(),
            ),
            resp: option.resp,
            headers: option.headers,
            proxy: Arc::from(option.proxy),
            url: Arc::new(option.url),
            multiplexing: option.multiplexing,
            accept_invalid_certs: option.accept_invalid_certs,
            accept_invalid_hostnames: option.accept_invalid_hostnames,
            file_id: option.file_id,
        })
    }
}

impl Clone for FastDownPuller {
    fn clone(&self) -> Self {
        Self {
            inner: if !self.multiplexing
                && let Ok(client) = build_client(
                    &self.headers,
                    &self.proxy,
                    self.accept_invalid_certs,
                    self.accept_invalid_hostnames,
                ) {
                HttpPuller::new(
                    self.url.as_ref().clone(),
                    client,
                    self.resp.clone(),
                    self.file_id.clone(),
                )
            } else {
                self.inner.clone()
            },
            resp: self.resp.clone(),
            headers: self.headers.clone(),
            proxy: self.proxy.clone(),
            url: self.url.clone(),
            multiplexing: self.multiplexing,
            accept_invalid_certs: self.accept_invalid_certs,
            accept_invalid_hostnames: self.accept_invalid_hostnames,
            file_id: self.file_id.clone(),
        }
    }
}

impl RandPuller for FastDownPuller {
    type Error = HttpError<Client>;
    async fn pull(
        &mut self,
        range: &ProgressEntry,
    ) -> PullResult<impl PullStream<Self::Error>, Self::Error> {
        RandPuller::pull(&mut self.inner, range).await
    }
}

impl SeqPuller for FastDownPuller {
    type Error = HttpError<Client>;
    async fn pull(&mut self) -> PullResult<impl PullStream<Self::Error>, Self::Error> {
        SeqPuller::pull(&mut self.inner).await
    }
}
