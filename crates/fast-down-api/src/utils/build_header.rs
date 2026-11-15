use reqwest::header::{HeaderMap, HeaderName};
use std::{collections::HashMap, str::FromStr};

/// Build a header map from the configured headers.
///
/// Returns the accepted headers and the names that were rejected (an invalid
/// name or value), so the caller can report them instead of dropping them
/// silently.
pub fn build_header(headers: &HashMap<String, String>) -> (HeaderMap, Vec<String>) {
    let mut result = HeaderMap::with_capacity(headers.len());
    let mut invalid = Vec::new();
    for (k, v) in headers {
        if let (Ok(k), Ok(v)) = (HeaderName::from_str(k), v.parse()) {
            result.insert(k, v);
        } else {
            invalid.push(k.clone());
        }
    }
    (result, invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_headers_are_inserted() {
        let mut map = HashMap::new();
        map.insert("User-Agent".to_string(), "test".to_string());
        map.insert("Accept".to_string(), "*/*".to_string());
        let (h, invalid) = build_header(&map);
        assert_eq!(invalid.len(), 0);
        assert_eq!(h.get("user-agent").unwrap(), "test");
        assert_eq!(h.get("accept").unwrap(), "*/*");
    }

    #[test]
    fn invalid_name_is_reported_and_skipped() {
        let mut map = HashMap::new();
        // A space inside the header name makes it invalid.
        map.insert("Invalid Name".to_string(), "x".to_string());
        let (h, invalid) = build_header(&map);
        assert!(h.is_empty());
        assert_eq!(invalid, ["Invalid Name"]);
    }

    #[test]
    fn invalid_value_is_reported_and_skipped() {
        let mut map = HashMap::new();
        // A newline in the value makes it invalid (control char rejected).
        map.insert("X-Test".to_string(), "bad\nvalue".to_string());
        let (h, invalid) = build_header(&map);
        assert!(h.is_empty());
        assert_eq!(invalid, ["X-Test"]);
    }

    #[test]
    fn empty_map_is_empty() {
        let map: HashMap<String, String> = HashMap::new();
        let (h, invalid) = build_header(&map);
        assert!(h.is_empty());
        assert_eq!(invalid.len(), 0);
    }
}
