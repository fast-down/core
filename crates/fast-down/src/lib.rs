#![doc = include_str!("../README.md")]

pub use fast_pull::*;

mod proxy;
pub use proxy::*;

mod url_info;
pub use url_info::*;

mod utils;
#[allow(unused_imports)]
pub use utils::*;

#[cfg(feature = "http")]
pub mod http;

#[cfg(feature = "reqwest")]
pub mod reqwest;
