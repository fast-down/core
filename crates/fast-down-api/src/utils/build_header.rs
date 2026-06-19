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
