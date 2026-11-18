#![no_std]
//! `fast-steal` provides a `no_std`-compatible building block for work-stealing
//! schedulers.
//!
//! The crate is built around three public types:
//! - [`Task`] — a lock-free, cancellable unit of work tracking a `start..end` range.
//! - [`TaskQueue`] — a concurrent queue that hands out work and steals sub-ranges
//!   from busy workers.
//! - [`Executor`] / [`Handle`] — traits you implement to plug the queue into any
//!   async runtime.
//!
//! Only [`alloc`](https://doc.rust-lang.org/alloc/) is required by the `no_std`
//! build: with default features off (`--no-default-features --features spin`) the
//! only non-`core` crates are `alloc` and `spin`, the latter supplying the queue's
//! lock. The default `std` feature instead backs that lock with `parking_lot`, and
//! it is what the tests and doctests are written against. For a complete runnable
//! example, see the crate-level documentation (the included README) below.
#![doc = include_str!("../README.md")]

mod executor;
mod task;
mod task_queue;

pub use executor::*;
pub use task::*;
pub use task_queue::*;
