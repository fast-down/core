use reqwest::header::{HeaderMap, HeaderName};
use std::{collections::HashMap, str::FromStr};

pub fn build_header(headers: &HashMap<String, String>) -> HeaderMap {
    let mut result = HeaderMap::with_capacity(headers.len());
    for (k, v) in headers {
        if let (Ok(k), Ok(v)) = (HeaderName::from_str(k), v.parse()) {
            result.insert(k, v);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_headers_are_inserted() {
        let mut map = HashMap::new();
        map.insert("User-Agent".to_string(), "test".to_string());
        map.insert("Accept".to_string(), "*/*".to_string());
        let h = build_header(&map);
        assert_eq!(h.get("user-agent").unwrap(), "test");
        assert_eq!(h.get("accept").unwrap(), "*/*");
    }

    #[test]
    fn invalid_name_is_skipped() {
        let mut map = HashMap::new();
        // A space inside the header name makes it invalid.
        map.insert("Invalid Name".to_string(), "x".to_string());
        assert!(build_header(&map).is_empty());
    }

    #[test]
    fn invalid_value_is_skipped() {
        let mut map = HashMap::new();
        // A newline in the value makes it invalid (control char rejected).
        map.insert("X-Test".to_string(), "bad\nvalue".to_string());
        assert!(build_header(&map).is_empty());
    }

    #[test]
    fn empty_map_is_empty() {
        let map: HashMap<String, String> = HashMap::new();
        assert!(build_header(&map).is_empty());
    }
}
