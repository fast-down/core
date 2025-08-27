mod url_info;

pub use fast_pull::*;
pub use url_info::UrlInfo;

pub mod http;
#[cfg(feature = "reqwest")]
pub mod reqwest;
#[cfg(feature = "reqwest-middleware")]
pub mod reqwest_middleware;
