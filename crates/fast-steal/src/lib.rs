#![no_std]
//! `fast-steal` provides a `no_std`-compatible building block for work-stealing
//! schedulers.
//!
//! The crate is built around three public types:
//! - [`Task`] — a cancellable unit of work tracking a `start..end` range, advanced
//!   without locks on targets with native 128-bit atomics.
//! - [`TaskQueue`] — a concurrent queue that hands out work and steals sub-ranges
//!   from busy workers.
//! - [`Executor`] / [`Handle`] — traits you implement to plug the queue into any
//!   async runtime.
//!
//! Only [`alloc`](https://doc.rust-lang.org/alloc/) is required by the `no_std`
//! build: with default features off (`--no-default-features --features spin`) the
//! non-`core` crates are `alloc`, `spin`, which supplies the queue's lock, and
//! `portable-atomic`, which supplies the task's 128-bit atomics and is compiled
//! in under every feature combination. The default `std` feature instead backs
//! that lock with `parking_lot`, and it is what the tests and doctests are
//! written against. For a complete runnable example, see the crate-level
//! documentation (the included README) below.
#![doc = include_str!("../README.md")]

mod executor;
mod task;
mod task_queue;

pub use executor::*;
pub use task::*;
pub use task_queue::*;
