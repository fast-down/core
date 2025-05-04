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
