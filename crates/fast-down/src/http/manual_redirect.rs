//! Manual redirect handling that respects the `Referrer-Policy` header.
//!
//! When `SmartRedirectClient` follows redirects itself (rather
//! than letting `reqwest` do it), it must compute the correct `Referer` header
//! for each hop. [`compute_referer`] implements the W3C Referrer Policy
//! algorithm (with RFC 9110 §7.4 defaults), and [`ReferrerPolicy`] parses the
//! policy tokens sent by servers.

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

/// Returns `true` when following `from` -> `to` would downgrade from HTTPS to HTTP.
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use url::Url;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn parse_all_tokens_case_insensitive() {
        assert_eq!(
            ReferrerPolicy::parse("no-referrer"),
            Some(ReferrerPolicy::NoReferrer)
        );
        assert_eq!(
            ReferrerPolicy::parse("NO-REFERRER"),
            Some(ReferrerPolicy::NoReferrer)
        );
        assert_eq!(
            ReferrerPolicy::parse("No-Referrer-When-Downgrade"),
            Some(ReferrerPolicy::NoReferrerWhenDowngrade)
        );
        assert_eq!(
            ReferrerPolicy::parse("origin"),
            Some(ReferrerPolicy::Origin)
        );
        assert_eq!(
            ReferrerPolicy::parse("origin-when-cross-origin"),
            Some(ReferrerPolicy::OriginWhenCrossOrigin)
        );
        assert_eq!(
            ReferrerPolicy::parse("same-origin"),
            Some(ReferrerPolicy::SameOrigin)
        );
        assert_eq!(
            ReferrerPolicy::parse("strict-origin"),
            Some(ReferrerPolicy::StrictOrigin)
        );
        assert_eq!(
            ReferrerPolicy::parse("strict-origin-when-cross-origin"),
            Some(ReferrerPolicy::StrictOriginWhenCrossOrigin)
        );
        assert_eq!(
            ReferrerPolicy::parse("unsafe-url"),
            Some(ReferrerPolicy::UnsafeUrl)
        );
    }

    #[test]
    fn parse_last_token_wins() {
        assert_eq!(
            ReferrerPolicy::parse("origin, no-referrer"),
            Some(ReferrerPolicy::NoReferrer)
        );
        assert_eq!(
            ReferrerPolicy::parse("no-referrer, origin"),
            Some(ReferrerPolicy::Origin)
        );
    }

    #[test]
    fn parse_invalid_returns_none() {
        assert_eq!(ReferrerPolicy::parse(""), None);
        assert_eq!(ReferrerPolicy::parse("garbage"), None);
        assert_eq!(
            ReferrerPolicy::parse("no-referrer, garbage"),
            Some(ReferrerPolicy::NoReferrer)
        );
    }

    #[test]
    fn compute_referer_matrix() {
        let a = u("https://a.com/p");
        let b = u("https://b.com/q");
        let a_http = u("http://a.com/r");

        // None / NoReferrerWhenDowngrade (default)
        assert_eq!(
            compute_referer(None, &a, &b),
            Some("https://a.com/p".to_string())
        );
        assert_eq!(compute_referer(None, &a, &a_http), None);

        // NoReferrer
        assert_eq!(
            compute_referer(Some(ReferrerPolicy::NoReferrer), &a, &b),
            None
        );

        // Origin
        assert_eq!(
            compute_referer(Some(ReferrerPolicy::Origin), &a, &b),
            Some("https://a.com".to_string())
        );
        assert_eq!(
            compute_referer(Some(ReferrerPolicy::Origin), &a, &a_http),
            Some("https://a.com".to_string())
        );

        // OriginWhenCrossOrigin
        assert_eq!(
            compute_referer(Some(ReferrerPolicy::OriginWhenCrossOrigin), &a, &a),
            Some("https://a.com/p".to_string())
        );
        assert_eq!(
            compute_referer(Some(ReferrerPolicy::OriginWhenCrossOrigin), &a, &b),
            Some("https://a.com".to_string())
        );

        // SameOrigin
        assert_eq!(
            compute_referer(Some(ReferrerPolicy::SameOrigin), &a, &a),
            Some("https://a.com/p".to_string())
        );
        assert_eq!(
            compute_referer(Some(ReferrerPolicy::SameOrigin), &a, &b),
            None
        );

        // StrictOrigin
        assert_eq!(
            compute_referer(Some(ReferrerPolicy::StrictOrigin), &a, &a),
            Some("https://a.com".to_string())
        );
        assert_eq!(
            compute_referer(Some(ReferrerPolicy::StrictOrigin), &a, &a_http),
            None
        );

        // StrictOriginWhenCrossOrigin
        assert_eq!(
            compute_referer(Some(ReferrerPolicy::StrictOriginWhenCrossOrigin), &a, &a),
            Some("https://a.com/p".to_string())
        );
        assert_eq!(
            compute_referer(Some(ReferrerPolicy::StrictOriginWhenCrossOrigin), &a, &b),
            Some("https://a.com".to_string())
        );
        assert_eq!(
            compute_referer(
                Some(ReferrerPolicy::StrictOriginWhenCrossOrigin),
                &a,
                &a_http
            ),
            None
        );

        // UnsafeUrl (keeps full referer, no downgrade suppression)
        assert_eq!(
            compute_referer(Some(ReferrerPolicy::UnsafeUrl), &a, &a_http),
            Some("https://a.com/p".to_string())
        );
    }

    #[test]
    fn compute_referer_strips_userinfo_and_fragment() {
        let with_auth = u("https://user:pass@a.com/p#frag");
        let other = u("https://b.com/q");
        let same = u("https://user:pass@a.com/r#x");
        // cross-origin -> only origin (no userinfo/fragment)
        assert_eq!(
            compute_referer(
                Some(ReferrerPolicy::OriginWhenCrossOrigin),
                &with_auth,
                &other
            ),
            Some("https://a.com".to_string())
        );
        // same-origin -> full referer with userinfo & fragment stripped
        assert_eq!(
            compute_referer(
                Some(ReferrerPolicy::OriginWhenCrossOrigin),
                &with_auth,
                &same
            ),
            Some("https://a.com/p".to_string())
        );
    }

    #[test]
    fn referer_preserves_query_strips_fragment() {
        // `referer_url` keeps the query string but drops the fragment.
        let prev = u("https://a.com/p?x=1&y=2#section");
        let next = u("https://a.com/q");
        assert_eq!(
            compute_referer(Some(ReferrerPolicy::UnsafeUrl), &prev, &next),
            Some("https://a.com/p?x=1&y=2".to_string())
        );
    }

    #[test]
    fn referer_normalizes_default_port() {
        // The `url` crate omits scheme-default ports during serialization, so a
        // referer built from an explicit :443 carries no port.
        let prev = u("https://a.com:443/p");
        let next = u("https://a.com/q");
        assert_eq!(
            compute_referer(Some(ReferrerPolicy::UnsafeUrl), &prev, &next),
            Some("https://a.com/p".to_string())
        );
    }

    #[test]
    fn upgrade_is_not_downgrade() {
        // http -> https is an upgrade, so the default policy keeps the referer.
        let prev = u("http://a.com/p");
        let next = u("https://b.com/q");
        assert_eq!(
            compute_referer(None, &prev, &next),
            Some("http://a.com/p".to_string())
        );
    }

    #[test]
    fn different_ports_are_cross_origin() {
        // Same host but a different (non-default) port is a different origin,
        // so a same-origin policy yields no referer.
        let prev = u("https://a.com:8443/p");
        let next = u("https://a.com/q");
        assert_eq!(
            compute_referer(Some(ReferrerPolicy::SameOrigin), &prev, &next),
            None
        );
    }

    #[test]
    fn parse_trims_whitespace_around_tokens() {
        assert_eq!(
            ReferrerPolicy::parse("  no-referrer  "),
            Some(ReferrerPolicy::NoReferrer)
        );
        assert_eq!(
            ReferrerPolicy::parse("origin ,no-referrer"),
            Some(ReferrerPolicy::NoReferrer)
        );
    }
}
