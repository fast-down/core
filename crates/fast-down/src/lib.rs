mod down;

pub use fast_pull::multi;
pub use fast_pull::single;

#[cfg(feature = "curl")]
pub mod curl;
#[cfg(feature = "reqwest")]
pub mod reqwest;
mod url_info;
