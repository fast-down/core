//! Core abstractions shared by every download engine in `fast-pull`.
//!
//! This module defines the two central traits — [`Puller`](crate::Puller) and
//! [`Pusher`](crate::Pusher) — plus the supporting types used to describe
//! progress and events: [`ProgressEntry`](crate::ProgressEntry),
//! [`Event`](crate::Event), [`WorkerId`](crate::WorkerId), and helpers for
//! merging and inverting progress ranges.

mod event;
mod invert;
mod merge;
mod progress;
mod puller;
mod pusher;

pub use event::*;
pub use invert::*;
pub use merge::*;
pub use progress::*;
pub use puller::*;
pub use pusher::*;
