use crate::{
    FileId, ProgressEntry, PullResult, PullStream,
    http::{HttpError, HttpPuller},
};
use fast_pull::Puller;
use parking_lot::Mutex;
use reqwest::{Client, ClientBuilder, Proxy, Response, header::HeaderMap};
use std::{
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use url::Url;

/// proxy 为 None 意为系统代理
/// 为 Some("") 意为不使用代理
/// 为 Some("proxy") 意为使用 proxy 作为全部代理
pub fn build_client(
    headers: &HeaderMap,
    proxy: Option<&str>,
    accept_invalid_certs: bool,
    accept_invalid_hostnames: bool,
    local_addr: Option<IpAddr>,
) -> Result<reqwest::Client, reqwest::Error> {
    let mut client = ClientBuilder::new()
        .default_headers(headers.clone())
        .danger_accept_invalid_certs(accept_invalid_certs)
        .danger_accept_invalid_hostnames(accept_invalid_hostnames)
        .local_address(local_addr);
    client = match proxy {
        None => client,
        Some("") => client.no_proxy(),
        Some(p) => client.proxy(Proxy::all(p)?),
    };
    client.build()
}

#[derive(Debug)]
pub struct FastDownPuller {
    inner: HttpPuller<Client>,
    headers: Arc<HeaderMap>,
    proxy: Option<Arc<str>>,
    url: Arc<Url>,
    accept_invalid_certs: bool,
    accept_invalid_hostnames: bool,
    file_id: FileId,
    resp: Option<Arc<Mutex<Option<Response>>>>,
    available_ips: Arc<[IpAddr]>,
    turn: Arc<AtomicUsize>,
}

pub struct FastDownPullerOptions<'a> {
    pub url: Url,
    pub headers: Arc<HeaderMap>,
    pub proxy: Option<&'a str>,
    pub accept_invalid_certs: bool,
    pub accept_invalid_hostnames: bool,
    pub file_id: FileId,
    pub resp: Option<Arc<Mutex<Option<Response>>>>,
    pub available_ips: Arc<[IpAddr]>,
}

impl FastDownPuller {
    pub fn new(option: FastDownPullerOptions) -> Result<Self, reqwest::Error> {
        let turn = Arc::new(AtomicUsize::new(1));
        let available_ips = option.available_ips;
        let client = build_client(
            &option.headers,
            option.proxy,
            option.accept_invalid_certs,
            option.accept_invalid_hostnames,
            if available_ips.is_empty() {
                None
            } else {
                available_ips
                    .get(turn.fetch_add(1, Ordering::AcqRel) % available_ips.len())
                    .cloned()
            },
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
            proxy: option.proxy.map(Arc::from),
            url: Arc::new(option.url),
            accept_invalid_certs: option.accept_invalid_certs,
            accept_invalid_hostnames: option.accept_invalid_hostnames,
            file_id: option.file_id,
            available_ips,
            turn,
        })
    }
}

impl Clone for FastDownPuller {
    fn clone(&self) -> Self {
        let available_ips = self.available_ips.clone();
        let turn = self.turn.clone();
        Self {
            inner: if let Ok(client) = build_client(
                &self.headers,
                self.proxy.as_deref(),
                self.accept_invalid_certs,
                self.accept_invalid_hostnames,
                {
                    if available_ips.is_empty() {
                        None
                    } else {
                        available_ips
                            .get(turn.fetch_add(1, Ordering::AcqRel) % available_ips.len())
                            .cloned()
                    }
                },
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
            accept_invalid_certs: self.accept_invalid_certs,
            accept_invalid_hostnames: self.accept_invalid_hostnames,
            file_id: self.file_id.clone(),
            available_ips,
            turn,
        }
    }
}

impl Puller for FastDownPuller {
    type Error = HttpError<Client>;
    async fn pull(
        &mut self,
        range: Option<&ProgressEntry>,
    ) -> PullResult<impl PullStream<Self::Error>, Self::Error> {
        Puller::pull(&mut self.inner, range).await
    }
}
