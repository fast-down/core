use crate::utils::build_client;
use inherit_config_derive::Config;
use reqwest::{Client, header::HeaderMap};
use std::{path::Path, sync::Arc, time::Duration};

#[derive(Debug, Clone, Config)]
pub struct DownloadConfig {
    #[config(default = Some(32))]
    pub threads: Option<usize>,

    #[config(default = Some(Arc::from("")))]
    pub proxy: Option<Arc<str>>,

    #[config(default = Some(Arc::new(HeaderMap::new())))]
    pub headers: Option<Arc<HeaderMap>>,

    #[config(default = Some(false))]
    pub accept_invalid_certs: Option<bool>,

    #[config(default = Some(false))]
    pub accept_invalid_hostnames: Option<bool>,

    #[config(default = Some(false))]
    pub multiplexing: Option<bool>,

    #[config(default = Some(Path::new(".").into()))]
    pub save_dir: Option<Arc<Path>>,

    #[config(default = Some(1024))]
    pub write_queue_cap: Option<usize>,

    #[config(default = Some(8 * 1024 * 1024))]
    pub write_buffer_size: Option<usize>,

    #[config(default = Some(Duration::from_millis(500)))]
    pub retry_gap: Option<Duration>,

    #[config(default = Some(1024 * 1024))]
    pub min_chunk_size: Option<u64>,
}

pub fn get_client(config: &DownloadConfig) -> Result<Client, reqwest::Error> {
    build_client(
        config.headers.as_ref().unwrap(),
        config.proxy.as_ref().unwrap(),
        config.accept_invalid_certs.unwrap(),
        config.accept_invalid_hostnames.unwrap(),
    )
}
