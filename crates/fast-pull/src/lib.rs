#![no_std]
#![doc = include_str!("../README.md")]

mod base;
mod core;
#[cfg(feature = "file")]
pub mod file;
#[cfg(feature = "mem")]
pub mod mem;
pub use base::*;
pub use core::*;
