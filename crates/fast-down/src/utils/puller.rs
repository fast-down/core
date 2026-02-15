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
        .http1_only()
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
    pullers: Arc<[HttpPuller<Client>]>,
    turn: Arc<AtomicUsize>,
}

pub struct FastDownPullerOptions<'a, 'b> {
    pub url: Url,
    pub headers: Arc<HeaderMap>,
    pub proxy: Option<&'a str>,
    pub accept_invalid_certs: bool,
    pub accept_invalid_hostnames: bool,
    pub file_id: FileId,
    pub resp: Option<Arc<Mutex<Option<Response>>>>,
    pub available_ips: &'b [IpAddr],
}

impl FastDownPuller {
    pub fn new(option: FastDownPullerOptions) -> Result<Self, reqwest::Error> {
        let turn = Arc::new(AtomicUsize::new(1));
        let pullers: Arc<[HttpPuller<Client>]> = option
            .available_ips
            .iter()
            .flat_map(|&ip| {
                build_client(
                    &option.headers,
                    option.proxy,
                    option.accept_invalid_certs,
                    option.accept_invalid_hostnames,
                    Some(ip),
                )
            })
            .map(|client| {
                HttpPuller::new(
                    option.url.clone(),
                    client,
                    option.resp.clone(),
                    option.file_id.clone(),
                )
            })
            .collect();
        let puller = if let Some(puller) = pullers.first().cloned() {
            puller
        } else {
            HttpPuller::new(
                option.url.clone(),
                build_client(
                    &option.headers,
                    option.proxy,
                    option.accept_invalid_certs,
                    option.accept_invalid_hostnames,
                    None,
                )?,
                option.resp.clone(),
                option.file_id.clone(),
            )
        };
        Ok(Self {
            inner: puller,
            pullers,
            turn,
        })
    }
}

impl Clone for FastDownPuller {
    fn clone(&self) -> Self {
        let pullers = self.pullers.clone();
        let turn = self.turn.clone();
        Self {
            inner: if let Some(puller) = {
                if pullers.is_empty() {
                    None
                } else {
                    pullers
                        .get(turn.fetch_add(1, Ordering::AcqRel) % pullers.len())
                        .cloned()
                }
            } {
                puller
            } else {
                self.inner.clone()
            },
            turn,
            pullers,
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
