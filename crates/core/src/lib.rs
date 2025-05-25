mod base;
mod core;
mod writer;
mod utils;

pub use base::event::*;
pub use base::merge_progress::*;
pub use base::progress::*;
pub use base::total::*;

pub use core::auto;
pub use core::multi;
pub use core::prefetch::*;
pub use core::single;
pub use writer::{SeqWriter, RandWriter};
pub use core::*;

#[cfg(feature = "file")]
pub mod file {
    pub use crate::utils::file::*;
    pub use crate::writer::file::*;
}
