#![no_std]
extern crate alloc;

mod base;
mod core;
#[cfg(feature = "file")]
pub mod file;
#[cfg(feature = "reqwest")]
pub mod reqwest;
pub use base::*;
pub use core::*;
