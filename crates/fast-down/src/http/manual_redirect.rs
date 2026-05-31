use url::Url;

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
        for token in s.split(',') {
            match token.trim().to_lowercase().as_str() {
                "no-referrer" => return Some(Self::NoReferrer),
                "no-referrer-when-downgrade" => return Some(Self::NoReferrerWhenDowngrade),
                "origin" => return Some(Self::Origin),
                "origin-when-cross-origin" => return Some(Self::OriginWhenCrossOrigin),
                "same-origin" => return Some(Self::SameOrigin),
                "strict-origin" => return Some(Self::StrictOrigin),
                "strict-origin-when-cross-origin" => {
                    return Some(Self::StrictOriginWhenCrossOrigin);
                }
                "unsafe-url" => return Some(Self::UnsafeUrl),
                _ => {}
            }
        }
        None
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
                Some(prev_url.to_string())
            }
        }
        Some(ReferrerPolicy::NoReferrer) => None,
        Some(ReferrerPolicy::Origin) => Some(origin()),
        Some(ReferrerPolicy::OriginWhenCrossOrigin) => {
            if same {
                Some(prev_url.to_string())
            } else {
                Some(origin())
            }
        }
        Some(ReferrerPolicy::SameOrigin) => {
            if same {
                Some(prev_url.to_string())
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
            if downgrade {
                None
            } else if same {
                Some(prev_url.to_string())
            } else {
                Some(origin())
            }
        }
        Some(ReferrerPolicy::UnsafeUrl) => Some(prev_url.to_string()),
    }
}
