use color_eyre::eyre::{eyre, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::str::FromStr;

pub fn build_headers(headers: &[String]) -> Result<HeaderMap> {
    let mut header_map = HeaderMap::with_capacity(headers.len());
    for header in headers {
        let parts: Vec<&str> = header.splitn(2, ':').map(|t| t.trim()).collect();
        if parts.len() != 2 {
            return Err(eyre!("Header格式应为 'Key: Value'"));
        }
        let key = parts[0];
        let value = parts[1];
        header_map.insert(HeaderName::from_str(key)?, HeaderValue::from_str(value)?);
    }
    Ok(header_map)
}

pub fn format_time(time: u64) -> String {
    let seconds = time % 60;
    let minutes = (time / 60) % 60;
    let hours = time / 3600;
    format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
}
