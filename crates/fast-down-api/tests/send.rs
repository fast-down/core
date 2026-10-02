//! Regression tests locking in that a downstream consumer can construct a
//! `fast_down::DownloadResult` inside an `async` block and `tokio::spawn` the
//! future that owns it, using this crate's concrete types.
//!
//! Unlike `fast-pull`'s own tests, these exercise `FastDownPuller` and
//! `BoxPusher` (whose push error is `Box<dyn AnyError>`), which is the shape
//! that additionally tripped the auto-trait leak check. The checks are
//! compile-time: the functions are referenced but never called, so no HTTP
//! client is built.
//!
//! Two separate leaks are covered, both handled by `fast-pull`:
//!
//! * constructing the handle inside an `async` block — fixed by the isolated
//!   `ForceSend` wrapper around the session state;
//! * awaiting the event stream inline — fixed by `DownloadResult::recv`, whose
//!   return type is spelled `impl Future + Send`.
//!
//! If either the `ForceSend` wrapper or `recv` regresses, these stop compiling.

#![allow(clippy::let_underscore_future)]

use fast_down::multi::{DownloadOptions, download_multi};
use std::time::Duration;

type Puller = fast_down::fast_puller::FastDownPuller;
type Pusher = fast_down::BoxPusher;

fn options(len: u64) -> DownloadOptions<std::iter::Once<std::ops::Range<u64>>> {
    DownloadOptions {
        download_chunks: std::iter::once(0..len),
        concurrent: 2,
        retry_gap: Duration::from_secs(1),
        pull_timeout: Duration::from_secs(5),
        push_queue_cap: 16,
        min_chunk_size: 1,
        max_speculative: 2,
    }
}

/// Construct inside the `async` block, hold across an `await`, drive with
/// `recv`, and hand the whole future to `tokio::spawn`.
#[test]
fn downstream_construct_and_spawn_is_send() {
    fn check(puller: Puller, pusher: Pusher) {
        let _ = tokio::spawn(async move {
            let result = download_multi(puller, pusher, options(1024));
            tokio::time::sleep(Duration::from_millis(1)).await;
            result.set_threads(2, 1);
            result.abort();
            while result.recv().await.is_ok() {}
        });
    }
    let _ = check;
}

/// The same, but construction happens outside and only the owned handle moves
/// into the spawned task — the shape `runner.rs` uses.
#[test]
fn downstream_move_handle_into_spawn_is_send() {
    fn check(puller: Puller, pusher: Pusher) {
        let result = download_multi(puller, pusher, options(1024));
        let _ = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1)).await;
            result.abort();
            while result.recv().await.is_ok() {}
        });
    }
    let _ = check;
}
