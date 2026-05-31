use crate::Proxy;
use crate::{
    FileId, ProgressEntry, PullResult, PullStream,
    http::{HttpError, HttpPuller, ReferrerPolicy},
    reqwest::SmartRedirectClient,
};
use fast_pull::Puller;
use parking_lot::Mutex;
use reqwest::{ClientBuilder, Response, header::HeaderMap, redirect::Policy};
use std::sync::Arc;
use url::Url;

/// # Errors
/// Returns an error if proxy configuration fails
pub fn build_client(
    mut headers: HeaderMap,
    proxy: Proxy<&str>,
    #[allow(unused)] accept_invalid_certs: bool,
    #[allow(unused)] accept_invalid_hostnames: bool,
    local_addr: Option<std::net::IpAddr>,
    max_redirects: usize,
) -> Result<SmartRedirectClient, reqwest::Error> {
    let referer = headers.remove("referer");
    let referrer_policy = headers
        .remove("referrer-policy")
        .and_then(|v| v.to_str().ok().and_then(ReferrerPolicy::parse));
    let mut client = ClientBuilder::new()
        .default_headers(headers)
        .local_address(local_addr)
        .redirect(Policy::none());
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
    Ok(SmartRedirectClient::new(
        client.build()?,
        referer,
        referrer_policy,
        max_redirects,
    ))
}

#[derive(Debug)]
pub struct FastDownPuller {
    inner: HttpPuller<SmartRedirectClient>,
    headers: Arc<HeaderMap>,
    proxy: Proxy<Arc<str>>,
    url: Arc<Url>,
    accept_invalid_certs: bool,
    accept_invalid_hostnames: bool,
    file_id: FileId,
    resp: Option<Arc<Mutex<Option<Response>>>>,
    available_ips: Arc<[std::net::IpAddr]>,
    turn: Arc<std::sync::atomic::AtomicUsize>,
    max_redirects: usize,
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
    pub max_redirects: usize,
}

impl FastDownPuller {
    /// # Errors
    /// Returns an error if proxy configuration fails
    pub fn new(option: FastDownPullerOptions<'_>) -> Result<Self, reqwest::Error> {
        let turn = Arc::new(std::sync::atomic::AtomicUsize::new(1));
        let available_ips = option.available_ips;
        let client = build_client(
            option.headers.as_ref().clone(),
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
            option.max_redirects,
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
            max_redirects: option.max_redirects,
        })
    }
}

impl Clone for FastDownPuller {
    fn clone(&self) -> Self {
        let available_ips = self.available_ips.clone();
        let turn = self.turn.clone();
        Self {
            inner: build_client(
                self.headers.as_ref().clone(),
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
                self.max_redirects,
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
            max_redirects: self.max_redirects,
        }
    }
}

impl Puller for FastDownPuller {
    type Error = HttpError<SmartRedirectClient>;
    async fn pull(
        &mut self,
        range: Option<&ProgressEntry>,
    ) -> PullResult<impl PullStream<Self::Error>, Self::Error> {
        Puller::pull(&mut self.inner, range).await
    }
}
