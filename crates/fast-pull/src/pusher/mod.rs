#[cfg(feature = "file")]
mod cache_std;
#[cfg(feature = "file")]
mod mmap;
#[cfg(feature = "file")]
mod std;

#[cfg(feature = "file")]
pub use cache_std::*;
#[cfg(feature = "file")]
pub use mmap::*;
#[cfg(feature = "file")]
pub use std::*;

mod mem;
pub use mem::*;
