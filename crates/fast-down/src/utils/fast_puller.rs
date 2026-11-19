//! The default HTTP [`fast_pull::Puller`] for this crate.
//!
//! [`FastDownPuller`] ties together the [`crate::http::HttpPuller`] engine and a
//! `SmartRedirectClient`, adding proxy support, optional
//! multi-interface IP rotation, and file-identity-based resumability. Construct
//! one from [`FastDownPullerOptions`] (typically via [`build_client`] to wire up
//! the underlying reqwest client), then pass it to `fast_pull::download_multi`
//! or `fast_pull::download_single` alongside a `Pusher` such as
//! `fast_pull::StdFilePusher` (requires the `file` feature of `fast-pull`).

use crate::Proxy;
use crate::{
    FileId, ProgressEntry, PullResult, PullStream,
    http::{HttpError, HttpPuller, ReferrerPolicy},
    reqwest::{BuildClientError, SmartRedirectClient},
};
use fast_pull::Puller;
use parking_lot::Mutex;
use reqwest::{
    ClientBuilder, Response,
    header::{ACCEPT_ENCODING, HeaderMap, HeaderValue},
    redirect::Policy,
};
use std::sync::Arc;
use std::time::Duration;
use url::Url;

/// The cookies a client built by [`build_client`] keeps.
///
/// A [`FastDownPuller`] shares one store across every client it builds, so a
/// clone — which rebuilds the client to rotate the local source address — still
/// inherits the session earlier responses established. Cloning the store shares
/// the jar; it never copies the cookies.
///
/// Cookie support is compiled in by the `cookie-store` feature (it enables
/// reqwest's `cookies` feature). Without it no client can retain a cookie, and
/// `enabled` has no effect — the same behaviour the plain `cookie_store` flag
/// always had.
#[derive(Debug, Clone, Default)]
pub struct CookieStore {
    #[cfg(feature = "cookie-store")]
    jar: Option<Arc<reqwest::cookie::Jar>>,
}

impl CookieStore {
    /// A store that keeps cookies only when `enabled`.
    #[cfg(feature = "cookie-store")]
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            jar: if enabled {
                Some(Arc::new(reqwest::cookie::Jar::default()))
            } else {
                None
            },
        }
    }

    /// A store that keeps cookies only when `enabled`.
    ///
    /// Without the `cookie-store` feature no client can retain a cookie, so the
    /// store is always empty and `enabled` has no effect.
    #[cfg(not(feature = "cookie-store"))]
    #[must_use]
    pub const fn new(_enabled: bool) -> Self {
        Self {}
    }

    /// Whether this store keeps cookies at all.
    ///
    /// Always `false` without the `cookie-store` feature.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        #[cfg(feature = "cookie-store")]
        {
            self.jar.is_some()
        }
        #[cfg(not(feature = "cookie-store"))]
        {
            false
        }
    }
}

/// # Errors
/// Returns [`BuildClientError`] if the HTTP client cannot be built (invalid
/// proxy URL, TLS backend failure, or other `reqwest::ClientBuilder` errors), or
/// if `accept_invalid_certs` / `accept_invalid_hostnames` was requested without
/// the `reqwest-tls` feature.
#[allow(clippy::too_many_arguments)]
pub fn build_client(
    mut headers: HeaderMap,
    proxy: Proxy<&str>,
    accept_invalid_certs: bool,
    accept_invalid_hostnames: bool,
    #[allow(unused)] cookie_store: &CookieStore,
    local_addr: Option<std::net::IpAddr>,
    max_redirects: usize,
    connect_timeout: Duration,
) -> Result<SmartRedirectClient, BuildClientError> {
    // Without the TLS backend `reqwest` has no `danger_accept_invalid_certs` /
    // `danger_accept_invalid_hostnames` at all, so the request cannot be honoured.
    // Refusing beats silently validating the certificates the caller asked to
    // skip: the mismatch would only surface as a failed handshake much later.
    #[cfg(not(feature = "reqwest-tls"))]
    if accept_invalid_certs || accept_invalid_hostnames {
        return Err(BuildClientError::MissingFeature(
            "accept_invalid_certs/accept_invalid_hostnames require the `reqwest-tls` feature",
        ));
    }
    let referer = headers.remove("referer");
    let referrer_policy = headers
        .remove("referrer-policy")
        .and_then(|v| v.to_str().ok().and_then(ReferrerPolicy::parse));
    // Per RFC 9110 §15.4 item 2.5, resource-specific headers MUST be stripped
    // on redirect. Extract them so they can be injected only on the first hop.
    let origin = headers.remove("origin");
    let authorization = headers.remove("authorization");
    let cookie = headers.remove("cookie");
    // A content coding makes the body bytes something other than the resource's
    // bytes at the offsets `Content-Range` names, and the engine writes those
    // offsets verbatim. This is an invariant rather than a default: a
    // caller-supplied `Accept-Encoding` is overwritten instead of honoured.
    // `reqwest`'s decompression features are unified across the dependency
    // graph, so another crate can switch them on and have the client negotiate
    // a coding even when this one did not ask for it.
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    let mut client = ClientBuilder::new()
        .default_headers(headers)
        .local_address(local_addr)
        .connect_timeout(connect_timeout)
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
    if let Some(jar) = &cookie_store.jar {
        // Hand over the puller's jar instead of letting the builder allocate a
        // private one: a rebuild must not lose the cookies the previous client
        // collected.
        client = client.cookie_provider(jar.clone());
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
/// rotated local address for multi-interface setups; the cookie jar (with the
/// `cookie-store` feature) is shared with every client the puller builds, so a
/// clone keeps the session earlier responses established.
#[derive(Debug)]
pub struct FastDownPuller {
    inner: HttpPuller<SmartRedirectClient>,
    headers: Arc<HeaderMap>,
    proxy: Proxy<Arc<str>>,
    url: Arc<Url>,
    accept_invalid_certs: bool,
    accept_invalid_hostnames: bool,
    cookie_store: CookieStore,
    file_id: FileId,
    resp: Option<Arc<Mutex<Option<Response>>>>,
    available_ips: Arc<[std::net::IpAddr]>,
    turn: Arc<std::sync::atomic::AtomicUsize>,
    max_redirects: usize,
    connect_timeout: Duration,
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
    ///
    /// Without that feature the client cannot honour the flag:
    /// [`FastDownPuller::new`] fails with [`BuildClientError`] instead of
    /// silently validating the certificates anyway.
    pub accept_invalid_certs: bool,
    /// Accept invalid TLS hostnames (requires the `reqwest-tls` feature).
    ///
    /// Like [`accept_invalid_certs`](Self::accept_invalid_certs), a build without
    /// `reqwest-tls` refuses the option rather than ignoring it.
    pub accept_invalid_hostnames: bool,
    /// The cookie store every client this puller builds shares (requires the
    /// `cookie-store` feature).
    ///
    /// Pass the same value to the prefetch client and to the puller so the
    /// session a probe established survives into the download. `CookieStore`'s
    /// `Clone` shares the jar; it never copies the cookies.
    pub cookie_store: CookieStore,
    /// The expected [`FileId`], used to detect a changed resource and resume safely.
    pub file_id: FileId,
    /// An already-open response to reuse for the first request (e.g. from a prefetch).
    pub resp: Option<Arc<Mutex<Option<Response>>>>,
    /// Candidate local source IPs for outbound connections; rotated across clones.
    pub available_ips: Arc<[std::net::IpAddr]>,
    /// Maximum number of redirects to follow before failing.
    pub max_redirects: usize,
    /// Bounds one TCP connect (DNS + proxy tunnel + TLS handshake).
    pub connect_timeout: Duration,
}

impl FastDownPuller {
    /// # Errors
    /// Returns [`BuildClientError`] if the underlying HTTP client cannot be
    /// built (invalid proxy, TLS setup failure, etc.), or if certificate
    /// validation was disabled while the `reqwest-tls` feature is off.
    pub fn new(option: FastDownPullerOptions<'_>) -> Result<Self, BuildClientError> {
        let turn = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let available_ips = option.available_ips;
        let local_addr = next_local_addr(&available_ips, &turn);
        let client = build_client(
            option.headers.as_ref().clone(),
            option.proxy,
            option.accept_invalid_certs,
            option.accept_invalid_hostnames,
            &option.cookie_store,
            local_addr,
            option.max_redirects,
            option.connect_timeout,
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
            connect_timeout: option.connect_timeout,
        })
    }
}

/// The next local source address in the rotation, or `None` when the caller
/// configured no addresses.
fn next_local_addr(
    available: &[std::net::IpAddr],
    turn: &std::sync::atomic::AtomicUsize,
) -> Option<std::net::IpAddr> {
    if available.is_empty() {
        return None;
    }
    let index = turn.fetch_add(1, std::sync::atomic::Ordering::AcqRel) % available.len();
    available.get(index).copied()
}

impl Clone for FastDownPuller {
    /// Rebuilds the HTTP client so a retry can leave the previous client and
    /// local source address behind. The cookie store (with the `cookie-store`
    /// feature) is shared with the previous client, so the rebuild keeps the
    /// session earlier responses established. If rebuilding fails, the previous
    /// client is kept: `Clone` cannot report the failure and a download must not
    /// panic.
    fn clone(&self) -> Self {
        let available_ips = self.available_ips.clone();
        let turn = self.turn.clone();
        let local_addr = next_local_addr(&available_ips, &turn);
        Self {
            inner: build_client(
                self.headers.as_ref().clone(),
                self.proxy.as_deref(),
                self.accept_invalid_certs,
                self.accept_invalid_hostnames,
                &self.cookie_store,
                local_addr,
                self.max_redirects,
                self.connect_timeout,
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
            cookie_store: self.cookie_store.clone(),
            file_id: self.file_id.clone(),
            available_ips,
            turn,
            max_redirects: self.max_redirects,
            connect_timeout: self.connect_timeout,
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
            cookie_store: CookieStore::default(),
            file_id: FileId::default(),
            resp: None,
            available_ips: Arc::from(Vec::<std::net::IpAddr>::new()),
            max_redirects: 10,
            connect_timeout: Duration::from_secs(10),
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

    /// `clone()` rebuilds the client, but the rebuilt client must be handed the
    /// very same jar: a jar per client is what made `cookie-store` useless on the
    /// download path.
    #[cfg(feature = "cookie-store")]
    #[test]
    fn clone_shares_the_cookie_jar() {
        let mut opts = make_options(Url::parse("http://example.com/a.bin").unwrap());
        opts.cookie_store = CookieStore::new(true);
        let puller = FastDownPuller::new(opts).expect("new must succeed");
        assert!(
            puller.cookie_store.is_enabled(),
            "cookie_store = true must create a jar"
        );
        let cloned = puller.clone();
        let jar = puller
            .cookie_store
            .jar
            .as_ref()
            .expect("the puller must hold a jar");
        let cloned_jar = cloned
            .cookie_store
            .jar
            .as_ref()
            .expect("the clone must hold a jar");
        assert!(
            Arc::ptr_eq(jar, cloned_jar),
            "clone() must share the cookie jar instead of starting a new one"
        );
    }

    /// The first response sets a cookie; a clone (the retry) must send it.
    /// `/b` only answers when the `cookie` header the jar collected is present,
    /// so a clone with a fresh jar cannot pass.
    #[cfg(feature = "cookie-store")]
    #[tokio::test]
    #[allow(clippy::significant_drop_tightening)]
    async fn clone_keeps_the_cookies_the_first_response_set() {
        use futures::TryStreamExt;

        let mut server = mockito::Server::new_async().await;
        let sets_cookie = server
            .mock("GET", "/a")
            .with_status(200)
            .with_header("set-cookie", "k=v")
            .with_body("a")
            .expect(1)
            .create_async()
            .await;
        let needs_cookie = server
            .mock("GET", "/b")
            .match_header("cookie", "k=v")
            .with_status(200)
            .with_body("b")
            .expect(1)
            .create_async()
            .await;

        let mut opts = make_options(Url::parse(&format!("{}/a", server.url())).unwrap());
        opts.cookie_store = CookieStore::new(true);
        let mut puller = FastDownPuller::new(opts).expect("new must succeed");
        let mut first = FastDownPuller::pull(&mut puller, None)
            .await
            .expect("the request that sets the cookie must succeed");
        while first
            .try_next()
            .await
            .expect("the first body must arrive")
            .is_some()
        {}
        // The stream borrows the puller for as long as it lives.
        drop(first);

        // A retry is a clone: same jar, rebuilt client, next resource.
        puller.url = Arc::new(Url::parse(&format!("{}/b", server.url())).unwrap());
        let mut retry = puller.clone();
        let mut second = FastDownPuller::pull(&mut retry, None)
            .await
            .expect("the clone must request /b with the cookie the first response set");
        while second
            .try_next()
            .await
            .expect("the second body must arrive")
            .is_some()
        {}

        needs_cookie.assert_async().await;
        sets_cookie.assert_async().await;
    }

    #[test]
    fn build_client_custom_proxy_succeeds() {
        let headers = HeaderMap::new();
        let client = build_client(
            headers,
            Proxy::Custom("http://127.0.0.1:1080"),
            false,
            false,
            &CookieStore::new(false),
            None,
            10,
            Duration::from_secs(10),
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
        let client = build_client(
            headers,
            Proxy::System,
            false,
            false,
            &CookieStore::new(false),
            None,
            10,
            Duration::from_secs(10),
        )
        .expect("build_client with the system proxy must succeed");
        let _ = client;
    }

    /// Without the TLS backend the builder has no way to disable certificate
    /// validation at all, so a silent success would validate exactly what the
    /// caller asked to skip. The refusal must name the missing feature.
    #[cfg(not(feature = "reqwest-tls"))]
    #[test]
    fn build_client_refuses_invalid_cert_settings_without_tls() {
        let err = build_client(
            HeaderMap::new(),
            Proxy::No,
            true,
            false,
            &CookieStore::new(false),
            None,
            10,
            Duration::from_secs(10),
        )
        .expect_err("accept_invalid_certs needs the reqwest-tls feature");
        assert!(matches!(err, BuildClientError::MissingFeature(_)));
        assert!(
            err.to_string().contains("reqwest-tls"),
            "the error must name the feature that is missing, got: {err}"
        );
    }

    /// The same flag is part of the build with the TLS backend, so the guard
    /// above cannot be mistaken for a general refusal.
    #[cfg(feature = "reqwest-tls")]
    #[test]
    fn build_client_honours_invalid_cert_settings_with_tls() {
        let client = build_client(
            HeaderMap::new(),
            Proxy::No,
            true,
            true,
            &CookieStore::new(false),
            None,
            10,
            Duration::from_secs(10),
        )
        .expect("the TLS backend can accept invalid certificates and hostnames");
        let _ = client;
    }

    #[tokio::test]
    async fn fast_down_puller_pull_forwards_to_inner() {
        // `pull` now issues the request before returning, so it needs a server that
        // actually answers. Bind an ephemeral port and serve one ranged response.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 206 Partial Content\r\n\
                          Content-Length: 4\r\n\
                          Content-Range: bytes 0-3/4\r\n\
                          Accept-Ranges: bytes\r\n\r\nbody",
                    )
                    .await;
            }
        });
        let opts = make_options(Url::parse(&format!("http://{addr}/a.bin")).unwrap());
        let mut puller = FastDownPuller::new(opts).expect("FastDownPuller::new must succeed");
        // `pull` forwards to the inner `HttpPuller`; a reachable server makes the
        // forwarding observable as an `Ok`.
        let stream = FastDownPuller::pull(&mut puller, None).await;
        assert!(stream.is_ok());
    }

    #[test]
    fn clone_falls_back_to_the_existing_client_when_the_rebuild_fails() {
        // `Clone` cannot return an error. A rebuild that fails (here: a proxy
        // edited into an unparseable value) keeps the previous client rather than
        // panicking; the proxy field is still reproduced for the next clone.
        let mut puller = FastDownPuller::new(make_options(
            Url::parse("http://example.com/a.bin").unwrap(),
        ))
        .expect("new must succeed");
        // Inject an invalid custom proxy that makes `build_client` fail on clone.
        puller.proxy = Proxy::Custom(Arc::from("not a valid proxy url"));
        let cloned = puller.clone();
        assert_eq!(
            cloned.proxy, puller.proxy,
            "clone must reproduce the proxy and not panic on a bad one"
        );
    }

    #[test]
    fn next_local_addr_starts_at_the_first_configured_ip() {
        let ips: Vec<std::net::IpAddr> = vec![
            "127.0.0.1".parse().unwrap(),
            "127.0.0.2".parse().unwrap(),
            "127.0.0.3".parse().unwrap(),
        ];
        let turn = std::sync::atomic::AtomicUsize::new(0);
        assert_eq!(next_local_addr(&ips, &turn), Some(ips[0]));
        assert_eq!(next_local_addr(&ips, &turn), Some(ips[1]));
        assert_eq!(next_local_addr(&ips, &turn), Some(ips[2]));
        assert_eq!(next_local_addr(&ips, &turn), Some(ips[0]));
        assert_eq!(next_local_addr(&[], &turn), None);
    }

    #[test]
    fn turn_counter_advances_once_per_client() {
        // `turn` starts at 0, so the first instance uses IP index 0 and every
        // clone advances to the next address. The counter is shared through the
        // `Arc`, so both handles observe the same value.
        let ips: Vec<std::net::IpAddr> = vec![
            "127.0.0.1".parse().unwrap(),
            "127.0.0.2".parse().unwrap(),
            "127.0.0.3".parse().unwrap(),
        ];
        let mut opts = make_options(Url::parse("http://example.com/a.bin").unwrap());
        opts.available_ips = Arc::from(ips);
        let puller = FastDownPuller::new(opts).expect("new with ips must succeed");
        // new() consumed 0 and advanced the counter to 1.
        assert_eq!(puller.turn.load(std::sync::atomic::Ordering::Acquire), 1);
        let cloned = puller.clone();
        // clone() consumed 1 and advanced the counter to 2.
        assert_eq!(cloned.turn.load(std::sync::atomic::Ordering::Acquire), 2);
        assert_eq!(puller.turn.load(std::sync::atomic::Ordering::Acquire), 2);
    }

    /// The engine writes the body at the offsets `Content-Range` names, so a
    /// content coding would corrupt the output. Identity is not a default a
    /// caller may override: a `gzip` request header is replaced.
    #[tokio::test]
    #[allow(clippy::significant_drop_tightening)]
    async fn build_client_forces_the_identity_encoding() {
        use futures::TryStreamExt;

        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/identity")
            .match_header("accept-encoding", "identity")
            .with_status(200)
            .with_body("x")
            .create_async()
            .await;

        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
        let mut opts = make_options(Url::parse(&format!("{}/identity", server.url())).unwrap());
        opts.headers = Arc::new(headers);
        let mut puller = FastDownPuller::new(opts).expect("new must succeed");
        let mut stream = FastDownPuller::pull(&mut puller, None)
            .await
            .expect("the request must succeed");
        while stream
            .try_next()
            .await
            .expect("the body must arrive")
            .is_some()
        {}
        drop(stream);

        mock.assert_async().await;
    }

    /// A ranged request lands in the same byte space as the whole-file one, so
    /// it asks for identity as well.
    #[tokio::test]
    #[allow(clippy::significant_drop_tightening)]
    async fn ranged_requests_also_ask_for_identity() {
        use futures::TryStreamExt;

        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/range")
            .match_header("accept-encoding", "identity")
            .match_header("range", "bytes=0-0")
            .with_status(206)
            .with_header("content-range", "bytes 0-0/1")
            .with_header("content-length", "1")
            .with_body("x")
            .create_async()
            .await;

        let opts = make_options(Url::parse(&format!("{}/range", server.url())).unwrap());
        let mut puller = FastDownPuller::new(opts).expect("new must succeed");
        let range = 0..1u64;
        let mut stream = FastDownPuller::pull(&mut puller, Some(&range))
            .await
            .expect("the request must succeed");
        while stream
            .try_next()
            .await
            .expect("the body must arrive")
            .is_some()
        {}
        drop(stream);

        mock.assert_async().await;
    }
}
