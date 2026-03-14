use crate::Proxy;
use crate::{
    FileId, ProgressEntry, PullResult, PullStream,
    http::{HttpError, HttpPuller},
};
use fast_pull::Puller;
use parking_lot::Mutex;
use reqwest::{Client, ClientBuilder, Response, header::HeaderMap};
use std::sync::Arc;
use url::Url;

/// # Errors
/// 当设置代理报错时返回 Error
pub fn build_client(
    headers: &HeaderMap,
    proxy: Proxy<&str>,
    #[allow(unused)] accept_invalid_certs: bool,
    #[allow(unused)] accept_invalid_hostnames: bool,
    local_addr: Option<std::net::IpAddr>,
) -> Result<Client, reqwest::Error> {
    let mut client = ClientBuilder::new()
        .default_headers(headers.clone())
        .local_address(local_addr);
    client = match proxy {
        Proxy::No => client.no_proxy(),
        Proxy::System => client,
        Proxy::Custom(p) => client.proxy(reqwest::Proxy::all(p)?),
    };
    #[cfg(feature = "reqwest-tls")]
    {
        client = client
            .danger_accept_invalid_certs(accept_invalid_certs)
            .danger_accept_invalid_hostnames(accept_invalid_hostnames);
    }
    client.build()
}

#[derive(Debug)]
pub struct FastDownPuller {
    inner: HttpPuller<Client>,
    headers: Arc<HeaderMap>,
    proxy: Proxy<Arc<str>>,
    url: Arc<Url>,
    accept_invalid_certs: bool,
    accept_invalid_hostnames: bool,
    file_id: FileId,
    resp: Option<Arc<Mutex<Option<Response>>>>,
    available_ips: Arc<[std::net::IpAddr]>,
    turn: Arc<std::sync::atomic::AtomicUsize>,
}

#[derive(Debug)]
pub struct FastDownPullerOptions<'a> {
    pub url: Url,
    pub headers: Arc<HeaderMap>,
    pub proxy: Proxy<&'a str>,
    pub accept_invalid_certs: bool,
    pub accept_invalid_hostnames: bool,
    pub file_id: FileId,
    pub resp: Option<Arc<Mutex<Option<Response>>>>,
    pub available_ips: Arc<[std::net::IpAddr]>,
}

impl FastDownPuller {
    /// # Errors
    /// 当设置代理报错时返回 Error
    pub fn new(option: FastDownPullerOptions<'_>) -> Result<Self, reqwest::Error> {
        let turn = Arc::new(std::sync::atomic::AtomicUsize::new(1));
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
                    .get(
                        turn.fetch_add(1, std::sync::atomic::Ordering::AcqRel)
                            % available_ips.len(),
                    )
                    .copied()
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
            inner: build_client(
                &self.headers,
                self.proxy.as_deref(),
                self.accept_invalid_certs,
                self.accept_invalid_hostnames,
                if available_ips.is_empty() {
                    None
                } else {
                    available_ips
                        .get(
                            turn.fetch_add(1, std::sync::atomic::Ordering::AcqRel)
                                % available_ips.len(),
                        )
                        .copied()
                },
            )
            .map_or_else(
                |_| self.inner.clone(),
                |client| {
                    HttpPuller::new(
                        self.url.as_ref().clone(),
                        client,
                        self.resp.clone(),
                        self.file_id.clone(),
                    )
                },
            ),
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
