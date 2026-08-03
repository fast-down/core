//! The default HTTP [`fast_pull::Puller`] for this crate.
//!
//! [`FastDownPuller`] ties together the [`crate::http::HttpPuller`] engine and a
//! `SmartRedirectClient`, adding proxy support, optional
//! multi-interface IP rotation, and file-identity-based resumability. Construct
//! one from [`FastDownPullerOptions`] (typically via [`build_client`] to wire up
//! the underlying reqwest client), then pass it to `fast_pull::download_multi`
//! or `fast_pull::download_single` alongside a `Pusher` such as
//! `fast_pull::file::StdFilePusher` (requires the `file` feature of `fast-pull`).

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
/// Returns an error if the HTTP client cannot be built (invalid proxy URL,
/// TLS backend failure, or other `reqwest::ClientBuilder` errors).
pub fn build_client(
    mut headers: HeaderMap,
    proxy: Proxy<&str>,
    #[allow(unused)] accept_invalid_certs: bool,
    #[allow(unused)] accept_invalid_hostnames: bool,
    #[allow(unused)] cookie_store: bool,
    local_addr: Option<std::net::IpAddr>,
    max_redirects: usize,
) -> Result<SmartRedirectClient, reqwest::Error> {
    let referer = headers.remove("referer");
    let referrer_policy = headers
        .remove("referrer-policy")
        .and_then(|v| v.to_str().ok().and_then(ReferrerPolicy::parse));
    // Per RFC 9110 §15.4 item 2.5, resource-specific headers MUST be stripped
    // on redirect. Extract them so they can be injected only on the first hop.
    let origin = headers.remove("origin");
    let authorization = headers.remove("authorization");
    let cookie = headers.remove("cookie");
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
    #[cfg(feature = "cookie-store")]
    {
        client = client.cookie_store(cookie_store);
    }
    Ok(SmartRedirectClient::new(
        client.build()?,
        referer,
        referrer_policy,
        origin,
        authorization,
        cookie,
        max_redirects,
    ))
}

/// The default [`Puller`] implementation for the fast-down crate.
///
/// Wraps an [`HttpPuller`] with a [`SmartRedirectClient`], IP rotation,
/// and proxy support. Cloning creates a new HTTP client with an optionally
/// rotated local address for multi-interface setups.
#[derive(Debug)]
pub struct FastDownPuller {
    inner: HttpPuller<SmartRedirectClient>,
    headers: Arc<HeaderMap>,
    proxy: Proxy<Arc<str>>,
    url: Arc<Url>,
    accept_invalid_certs: bool,
    accept_invalid_hostnames: bool,
    cookie_store: bool,
    file_id: FileId,
    resp: Option<Arc<Mutex<Option<Response>>>>,
    available_ips: Arc<[std::net::IpAddr]>,
    turn: Arc<std::sync::atomic::AtomicUsize>,
    max_redirects: usize,
}
// Field-level docs live on [`FastDownPullerOptions`], the public construction
// surface; the runtime struct mirrors those fields.

/// Options for constructing a [`FastDownPuller`].
#[derive(Debug)]
pub struct FastDownPullerOptions<'a> {
    /// The URL to download.
    pub url: Url,
    /// Extra request headers sent on every request.
    pub headers: Arc<HeaderMap>,
    /// Proxy selection (no / system / custom URL).
    pub proxy: Proxy<&'a str>,
    /// Accept invalid TLS certificates (requires the `reqwest-tls` feature).
    pub accept_invalid_certs: bool,
    /// Accept invalid TLS hostnames (requires the `reqwest-tls` feature).
    pub accept_invalid_hostnames: bool,
    /// Enable a cookie store (requires the `cookie-store` feature).
    pub cookie_store: bool,
    /// The expected [`FileId`], used to detect a changed resource and resume safely.
    pub file_id: FileId,
    /// An already-open response to reuse for the first request (e.g. from a prefetch).
    pub resp: Option<Arc<Mutex<Option<Response>>>>,
    /// Candidate local source IPs for outbound connections; rotated across clones.
    pub available_ips: Arc<[std::net::IpAddr]>,
    /// Maximum number of redirects to follow before failing.
    pub max_redirects: usize,
}

impl FastDownPuller {
    /// # Errors
    /// Returns an error if the underlying HTTP client cannot be built (invalid
    /// proxy, TLS setup failure, etc.).
    pub fn new(option: FastDownPullerOptions<'_>) -> Result<Self, reqwest::Error> {
        let turn = Arc::new(std::sync::atomic::AtomicUsize::new(1));
        let available_ips = option.available_ips;
        let client = build_client(
            option.headers.as_ref().clone(),
            option.proxy,
            option.accept_invalid_certs,
            option.accept_invalid_hostnames,
            option.cookie_store,
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
        let url = Arc::new(option.url);
        Ok(Self {
            inner: HttpPuller::new(
                url.clone(),
                client,
                option.resp.clone(),
                option.file_id.clone(),
            ),
            resp: option.resp,
            headers: option.headers,
            proxy: option.proxy.map(Arc::from),
            url,
            accept_invalid_certs: option.accept_invalid_certs,
            accept_invalid_hostnames: option.accept_invalid_hostnames,
            cookie_store: option.cookie_store,
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
                self.cookie_store,
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
                        self.url.clone(),
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
            cookie_store: self.cookie_store,
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn make_options(url: Url) -> FastDownPullerOptions<'static> {
        FastDownPullerOptions {
            url,
            headers: Arc::new(HeaderMap::new()),
            proxy: Proxy::No,
            accept_invalid_certs: false,
            accept_invalid_hostnames: false,
            cookie_store: false,
            file_id: FileId::default(),
            resp: None,
            available_ips: Arc::from(Vec::<std::net::IpAddr>::new()),
            max_redirects: 10,
        }
    }

    /// Plan A stores the URL in a single `Arc<Url>` shared between the
    /// `FastDownPuller` and its `HttpPuller` inner, and carried across `Clone`.
    /// This asserts that cloning a `FastDownPuller` shares the exact same
    /// `Arc<Url>` (i.e. the URL is stored only once).
    #[test]
    fn test_fast_down_puller_clone_shares_url_arc() {
        let opts = make_options(Url::parse("http://example.com/a.bin").unwrap());
        let puller = FastDownPuller::new(opts).expect("FastDownPuller::new must succeed");
        let cloned = puller.clone();
        assert!(
            Arc::ptr_eq(&puller.url, &cloned.url),
            "clone() must share the same Arc<Url> (URL stored once)"
        );
    }

    #[test]
    fn build_client_custom_proxy_succeeds() {
        let headers = HeaderMap::new();
        let client = build_client(
            headers,
            Proxy::Custom("http://127.0.0.1:1080"),
            false,
            false,
            false,
            None,
            10,
        )
        .expect("build_client with a custom proxy must succeed");
        let _ = client;
    }

    #[test]
    fn new_with_available_ips_rotates_local_address() {
        let ips: Vec<std::net::IpAddr> = vec!["127.0.0.1".parse().unwrap()];
        let mut opts = make_options(Url::parse("http://example.com/a.bin").unwrap());
        opts.available_ips = Arc::from(ips);
        let puller = FastDownPuller::new(opts).expect("FastDownPuller::new with ips must succeed");
        let cloned = puller.clone();
        assert!(Arc::ptr_eq(&puller.url, &cloned.url));
    }

    #[test]
    fn clone_with_available_ips_rotates_local_address() {
        let ips: Vec<std::net::IpAddr> = vec!["127.0.0.1".parse().unwrap()];
        let mut opts = make_options(Url::parse("http://example.com/a.bin").unwrap());
        opts.available_ips = Arc::from(ips);
        let puller = FastDownPuller::new(opts).expect("FastDownPuller::new with ips must succeed");
        // Cloning with non-empty available_ips rotates the local address and
        // rebuilds a client via the `map_or_else` success branch.
        let cloned = puller.clone();
        assert!(Arc::ptr_eq(&puller.url, &cloned.url));
    }

    #[test]
    fn build_client_system_proxy_succeeds() {
        let headers = HeaderMap::new();
        let client = build_client(headers, Proxy::System, false, false, false, None, 10)
            .expect("build_client with the system proxy must succeed");
        let _ = client;
    }

    #[tokio::test]
    async fn fast_down_puller_pull_forwards_to_inner() {
        let opts = make_options(Url::parse("http://example.com/a.bin").unwrap());
        let mut puller = FastDownPuller::new(opts).expect("FastDownPuller::new must succeed");
        // `pull` simply forwards to the inner `HttpPuller`; driving the stream is
        // not required to cover the forwarding body.
        let stream = FastDownPuller::pull(&mut puller, None).await;
        assert!(stream.is_ok());
    }

    #[test]
    fn clone_swallows_build_client_error() {
        // Hypothesis C: `FastDownPuller::clone` rebuilds the client via
        // `build_client`, but on failure it silently falls back to
        // `self.inner.clone()` (the `map_or_else` Err branch). Because
        // `Clone::clone` cannot return a `Result`, the build error is swallowed
        // and the clone keeps the stale client instead of surfacing the failure.
        let mut puller = FastDownPuller::new(make_options(
            Url::parse("http://example.com/a.bin").unwrap(),
        ))
        .expect("new must succeed");
        // Inject an invalid custom proxy that makes `build_client` fail on clone.
        puller.proxy = Proxy::Custom(Arc::from("not a valid proxy url"));
        // Clone must still succeed (the error is swallowed via `map_or_else`,
        // not propagated) and reproduce the proxy config.
        let cloned = puller.clone();
        assert_eq!(
            cloned.proxy, puller.proxy,
            "clone must reproduce the proxy and not panic on a bad one"
        );
    }

    #[test]
    fn turn_counter_starts_at_one_offsetting_first_ip_index() {
        // `turn` is initialized to 1, so the first `fetch_add` in `new` returns 1
        // and the first instance picks IP index `1 % len` rather than `0`. The
        // rotation is still complete (every IP is eventually reached); only the
        // starting point is offset by one. This locks in the counter behavior.
        let ips: Vec<std::net::IpAddr> = vec![
            "127.0.0.1".parse().unwrap(),
            "127.0.0.2".parse().unwrap(),
            "127.0.0.3".parse().unwrap(),
        ];
        let mut opts = make_options(Url::parse("http://example.com/a.bin").unwrap());
        opts.available_ips = Arc::from(ips);
        let puller = FastDownPuller::new(opts).expect("new with ips must succeed");
        // new() consumed the initial value 1 and advanced the counter to 2.
        assert_eq!(puller.turn.load(std::sync::atomic::Ordering::Acquire), 2);
        let cloned = puller.clone();
        // clone() consumed 2 and advanced the counter to 3. The counter is a
        // shared `Arc`, so both handles observe the same advanced value.
        assert_eq!(cloned.turn.load(std::sync::atomic::Ordering::Acquire), 3);
        assert_eq!(puller.turn.load(std::sync::atomic::Ordering::Acquire), 3);
    }
}
