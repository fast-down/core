use crate::{
    FileId, ProgressEntry, PullResult, PullStream,
    http::{HttpError, HttpPuller},
};
use fast_pull::Puller;
use parking_lot::Mutex;
use reqwest::{Client, ClientBuilder, Response, header::HeaderMap};
use std::{ops::Deref, sync::Arc};
use url::Url;

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Proxy<T> {
    No,
    #[default]
    System,
    Custom(T),
}

impl<T> Proxy<T> {
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Proxy<U> {
        match self {
            Self::No => Proxy::No,
            Self::System => Proxy::System,
            Self::Custom(t) => Proxy::Custom(f(t)),
        }
    }

    pub fn as_deref(&self) -> Proxy<&T::Target>
    where
        T: Deref,
    {
        match self {
            Self::No => Proxy::No,
            Self::System => Proxy::System,
            Self::Custom(t) => Proxy::Custom(&**t),
        }
    }

    pub const fn as_ref(&self) -> Proxy<&T> {
        match self {
            Self::No => Proxy::No,
            Self::System => Proxy::System,
            Self::Custom(t) => Proxy::Custom(t),
        }
    }
}

/// # Errors
/// 当设置代理报错时返回 Error
pub fn build_client(
    headers: &HeaderMap,
    #[cfg(not(target_family = "wasm"))] proxy: Proxy<&str>,
    #[cfg(not(target_family = "wasm"))]
    #[allow(unused)]
    accept_invalid_certs: bool,
    #[cfg(not(target_family = "wasm"))]
    #[allow(unused)]
    accept_invalid_hostnames: bool,
    #[cfg(not(target_family = "wasm"))] local_addr: Option<std::net::IpAddr>,
) -> Result<Client, reqwest::Error> {
    #[allow(unused_mut)]
    let mut client = ClientBuilder::new().default_headers(headers.clone());
    #[cfg(not(target_family = "wasm"))]
    {
        client = client.local_address(local_addr);
        client = match proxy {
            Proxy::No => client.no_proxy(),
            Proxy::System => client,
            Proxy::Custom(p) => client.proxy(reqwest::Proxy::all(p)?),
        };
    }
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
    #[cfg(not(target_family = "wasm"))]
    proxy: Proxy<Arc<str>>,
    url: Arc<Url>,
    #[cfg(not(target_family = "wasm"))]
    accept_invalid_certs: bool,
    #[cfg(not(target_family = "wasm"))]
    accept_invalid_hostnames: bool,
    file_id: FileId,
    resp: Option<Arc<Mutex<Option<Response>>>>,
    #[cfg(not(target_family = "wasm"))]
    available_ips: Arc<[std::net::IpAddr]>,
    #[cfg(not(target_family = "wasm"))]
    turn: Arc<std::sync::atomic::AtomicUsize>,
}

#[derive(Debug)]
pub struct FastDownPullerOptions<'a> {
    pub url: Url,
    pub headers: Arc<HeaderMap>,
    #[cfg(not(target_family = "wasm"))]
    pub proxy: Proxy<&'a str>,
    #[cfg(not(target_family = "wasm"))]
    pub accept_invalid_certs: bool,
    #[cfg(not(target_family = "wasm"))]
    pub accept_invalid_hostnames: bool,
    pub file_id: FileId,
    pub resp: Option<Arc<Mutex<Option<Response>>>>,
    #[cfg(not(target_family = "wasm"))]
    pub available_ips: Arc<[std::net::IpAddr]>,
}

impl FastDownPuller {
    /// # Errors
    /// 当设置代理报错时返回 Error
    pub fn new(option: FastDownPullerOptions<'_>) -> Result<Self, reqwest::Error> {
        #[cfg(not(target_family = "wasm"))]
        let turn = Arc::new(std::sync::atomic::AtomicUsize::new(1));
        #[cfg(not(target_family = "wasm"))]
        let available_ips = option.available_ips;
        let client = build_client(
            &option.headers,
            #[cfg(not(target_family = "wasm"))]
            option.proxy,
            #[cfg(not(target_family = "wasm"))]
            option.accept_invalid_certs,
            #[cfg(not(target_family = "wasm"))]
            option.accept_invalid_hostnames,
            #[cfg(not(target_family = "wasm"))]
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
            #[cfg(not(target_family = "wasm"))]
            proxy: option.proxy.map(Arc::from),
            url: Arc::new(option.url),
            #[cfg(not(target_family = "wasm"))]
            accept_invalid_certs: option.accept_invalid_certs,
            #[cfg(not(target_family = "wasm"))]
            accept_invalid_hostnames: option.accept_invalid_hostnames,
            file_id: option.file_id,
            #[cfg(not(target_family = "wasm"))]
            available_ips,
            #[cfg(not(target_family = "wasm"))]
            turn,
        })
    }
}

impl Clone for FastDownPuller {
    fn clone(&self) -> Self {
        #[cfg(not(target_family = "wasm"))]
        let available_ips = self.available_ips.clone();
        #[cfg(not(target_family = "wasm"))]
        let turn = self.turn.clone();
        Self {
            inner: build_client(
                &self.headers,
                #[cfg(not(target_family = "wasm"))]
                self.proxy.as_deref(),
                #[cfg(not(target_family = "wasm"))]
                self.accept_invalid_certs,
                #[cfg(not(target_family = "wasm"))]
                self.accept_invalid_hostnames,
                #[cfg(not(target_family = "wasm"))]
                {
                    if available_ips.is_empty() {
                        None
                    } else {
                        available_ips
                            .get(
                                turn.fetch_add(1, std::sync::atomic::Ordering::AcqRel)
                                    % available_ips.len(),
                            )
                            .copied()
                    }
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
            #[cfg(not(target_family = "wasm"))]
            proxy: self.proxy.clone(),
            url: self.url.clone(),
            #[cfg(not(target_family = "wasm"))]
            accept_invalid_certs: self.accept_invalid_certs,
            #[cfg(not(target_family = "wasm"))]
            accept_invalid_hostnames: self.accept_invalid_hostnames,
            file_id: self.file_id.clone(),
            #[cfg(not(target_family = "wasm"))]
            available_ips,
            #[cfg(not(target_family = "wasm"))]
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
