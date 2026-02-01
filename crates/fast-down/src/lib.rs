#![doc = include_str!("../README.md")]

mod url_info;

pub use fast_pull::*;
pub use url_info::*;

#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "reqwest")]
pub mod reqwest;
#[cfg(feature = "utils")]
pub mod utils;
