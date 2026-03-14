#![doc = include_str!("../README.md")]

mod proxy;
mod url_info;

pub use fast_pull::*;
pub use proxy::*;
pub use url_info::*;

#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "reqwest")]
pub mod reqwest;
mod utils;
#[allow(unused_imports)]
pub use utils::*;
