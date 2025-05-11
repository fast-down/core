use color_eyre::eyre::{eyre, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::str::FromStr;

pub fn build_headers(headers: &[String]) -> Result<HeaderMap> {
    let mut header_map = HeaderMap::with_capacity(headers.len());
    for header in headers {
        let parts: Vec<&str> = header.splitn(2, ':').map(|t| t.trim()).collect();
        if parts.len() != 2 {
            return Err(eyre!("header should have a format of 'Header: Value'"));
        }
        let key = parts[0];
        let value = parts[1];
        header_map.insert(HeaderName::from_str(key)?, HeaderValue::from_str(value)?);
    }
    Ok(header_map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header;

    #[test]
    fn test_build_headers_success() {
        let headers = vec![
            "Content-Type: application/json".to_string(),
            "Authorization: Bearer token123".to_string(),
        ];
        let result = build_headers(&headers).unwrap();

        assert_eq!(
            result.get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        assert_eq!(
            result.get(header::AUTHORIZATION).unwrap(),
            "Bearer token123"
        );
    }

    #[test]
    fn test_build_headers_empty_input() {
        let headers = vec![];
        let result = build_headers(&headers).unwrap();

        assert!(result.is_empty());
    }

    #[test]
    #[should_panic = "header should have a format of 'Header: Value'"]
    fn test_build_headers_invalid_format() {
        let headers = vec![
            "Content-Type application/json".to_string(), // Missing colon
        ];
        build_headers(&headers).unwrap();
    }

    #[test]
    fn test_build_headers_invalid_header_name() {
        let headers = vec!["Invalid-Header-Name: value".to_string()];
        let result = build_headers(&headers).unwrap();

        assert_eq!(result.get("Invalid-Header-Name").unwrap(), "value");
    }
}
