use color_eyre::eyre::{eyre, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufReader, Read};
use std::str::FromStr;

pub fn sha256_file(file_path: &str) -> std::io::Result<String> {
    let file = File::open(file_path)?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let bytes_read = reader.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

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
