mod down;

pub use fast_pull::single;
pub use fast_pull::multi;

#[cfg(feature = "reqwest")]
pub mod reqwest;
#[cfg(feature = "curl")]
pub mod curl;
mod url_info;
