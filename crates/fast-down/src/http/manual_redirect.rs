use url::Url;

/// Serialize a URL for use as a `Referer` header value,
/// stripping `userinfo` and `fragment` per RFC 9110 §7.4.
fn referer_url(url: &Url) -> String {
    let mut cleaned = url.clone();
    // Strip userinfo (username:password@)
    let _ = cleaned.set_username("");
    let _ = cleaned.set_password(None);
    // Strip fragment (#section)
    cleaned.set_fragment(None);
    cleaned.to_string()
}

/// Referrer-Policy values as defined by the W3C Referrer Policy specification.
///
/// Used by [`compute_referer`] to determine the `Referer` header value during
/// redirect following, in accordance with RFC 9110 §7.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferrerPolicy {
    NoReferrer,
    NoReferrerWhenDowngrade,
    Origin,
    OriginWhenCrossOrigin,
    SameOrigin,
    StrictOrigin,
    StrictOriginWhenCrossOrigin,
    UnsafeUrl,
}

impl ReferrerPolicy {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let mut last = None;
        for token in s.split(',') {
            match token.trim().to_lowercase().as_str() {
                "no-referrer" => last = Some(Self::NoReferrer),
                "no-referrer-when-downgrade" => last = Some(Self::NoReferrerWhenDowngrade),
                "origin" => last = Some(Self::Origin),
                "origin-when-cross-origin" => last = Some(Self::OriginWhenCrossOrigin),
                "same-origin" => last = Some(Self::SameOrigin),
                "strict-origin" => last = Some(Self::StrictOrigin),
                "strict-origin-when-cross-origin" => {
                    last = Some(Self::StrictOriginWhenCrossOrigin);
                }
                "unsafe-url" => last = Some(Self::UnsafeUrl),
                _ => {}
            }
        }
        last
    }
}

fn is_downgrade(from: &Url, to: &Url) -> bool {
    from.scheme() == "https" && to.scheme() == "http"
}

/// Compute the `Referer` header value for the next request in a redirect chain.
///
/// Follows RFC 9110 §7.4 and the W3C Referrer Policy specification.
/// When `policy` is `None`, defaults to `no-referrer-when-downgrade` semantics
/// (which is the browser-default behavior per RFC).
#[must_use]
pub fn compute_referer(
    policy: Option<ReferrerPolicy>,
    prev_url: &Url,
    next_url: &Url,
) -> Option<String> {
    let downgrade = is_downgrade(prev_url, next_url);
    let same = prev_url.origin() == next_url.origin();
    let origin = || prev_url.origin().ascii_serialization();

    match policy {
        None | Some(ReferrerPolicy::NoReferrerWhenDowngrade) => {
            if downgrade {
                None
            } else {
                Some(referer_url(prev_url))
            }
        }
        Some(ReferrerPolicy::NoReferrer) => None,
        Some(ReferrerPolicy::Origin) => Some(origin()),
        Some(ReferrerPolicy::OriginWhenCrossOrigin) => {
            if same {
                Some(referer_url(prev_url))
            } else {
                Some(origin())
            }
        }
        Some(ReferrerPolicy::SameOrigin) => {
            if same {
                Some(referer_url(prev_url))
            } else {
                None
            }
        }
        Some(ReferrerPolicy::StrictOrigin) => {
            if downgrade {
                None
            } else {
                Some(origin())
            }
        }
        Some(ReferrerPolicy::StrictOriginWhenCrossOrigin) => {
            if same {
                Some(referer_url(prev_url))
            } else if downgrade {
                None
            } else {
                Some(origin())
            }
        }
        Some(ReferrerPolicy::UnsafeUrl) => Some(referer_url(prev_url)),
    }
}
