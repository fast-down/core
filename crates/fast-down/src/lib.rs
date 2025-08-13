mod down;

pub use fast_pull::single;
pub use fast_pull::multi;

#[cfg(feature = "reqwest")]
pub mod reqwest;
mod url_info;
