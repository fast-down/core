mod url_info;

pub use fast_pull::*;
pub use url_info::UrlInfo;

#[cfg(feature = "reqwest")]
pub mod reqwest;
