mod url_info;

pub use fast_pull::*;
pub use url_info::*;

#[cfg(feature = "downloader")]
pub mod downloader;
#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "reqwest")]
pub mod reqwest;
