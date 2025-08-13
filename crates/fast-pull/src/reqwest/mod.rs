#![deprecated(
    since = "3.1.0",
    note = "`reqwest` feature is deprecated, and will be removed in 3.4.0, please use `reqwest` from `fast-down` crate instead"
)]

mod prefetch;
mod puller;

pub use prefetch::*;
pub use puller::*;
