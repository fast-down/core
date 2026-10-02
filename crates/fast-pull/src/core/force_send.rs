//! An isolated workaround for a rustc limitation, kept apart from the session
//! logic on purpose.
//!
//! ## What this is
//!
//! [`ForceSend`] is a transparent wrapper that is *always* `Send` and `Sync`,
//! regardless of the wrapped type. It is used in exactly one place — around the
//! `Arc<DownloadResultInner<..>>` inside
//! [`DownloadResult`](crate::DownloadResult) — to work around rustc's
//! `async fn` auto-trait leak check.
//!
//! ## Why it is needed
//!
//! `download_multi` and `download_single` are synchronous, but downstream code
//! naturally constructs their result inside an `async` block and holds it
//! across an `await`:
//!
//! ```ignore
//! tokio::spawn(async move {
//!     let result = download_multi(puller, pusher, options);
//!     while result.recv().await.is_ok() {}
//! });
//! ```
//!
//! rustc's `async fn` auto-trait leak check rejects this with a spurious
//! `higher-ranked lifetime error`, even though the handle really is `Send`.
//! There are two independent triggers, and this file addresses the first:
//!
//! 1. **Constructing the handle.** The leak check gives up when it has to
//!    normalize an associated-type projection (`E::Handle`, named by the
//!    session's task queue) from inside an `async` context. `ForceSend` erases
//!    the projection from the handle's fields, which discharges the check.
//! 2. **Awaiting the event stream.** `crossfire::RecvFuture` is only `Send`
//!    through a hand-written impl the check does not see through; that is why
//!    [`DownloadResult::recv`](crate::DownloadResult::recv) exists — use it
//!    instead of `event_chain().recv()` in a spawned driver.
//!
//! Both are false negatives of the same class, tracked in
//! <https://github.com/rust-lang/rust/issues/110338> (metabug: "incorrect
//! lifetime bound errors in async"); the fix lives behind the upcoming
//! "assumptions on binders" work and is not available yet.
//!
//! ## Why it is sound
//!
//! Every field of `DownloadResultInner` is genuinely `Send + Sync` for **every**
//! instantiation that can exist: the event chain (`crossfire::MAsyncRx`) is
//! unconditionally `Send + Sync`, the task queue + executor require
//! `E: Executor + Send + Sync` by its own definition, and the cancellation
//! token is `Send + Sync`. `ForceSend` therefore asserts a property the type
//! already has; it only silences a compiler false negative. The assertion is
//! deliberately stated once, here, rather than spread across the session code.
//!
//! Once the leak check is fixed upstream, this type can be deleted and the
//! field can go back to a bare `Arc<DownloadResultInner<..>>`.

use std::ops::Deref;

/// A transparent, unconditionally `Send + Sync` holder.
///
/// See the module docs for why this exists and why it is sound. It derefs to
/// the wrapped value, so code that goes through it reads exactly like code that
/// holds the value directly.
#[repr(transparent)]
pub struct ForceSend<T>(pub T);

// SAFETY: `ForceSend` is transparent and is only ever used to wrap a value that
// is already `Send + Sync`. The assertion exists solely to bypass rustc's
// `async fn` auto-trait leak check (rust-lang/rust#110338), which cannot prove
// the wrapped value's auto-traits in an `async` context.
#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl<T> Send for ForceSend<T> {}
unsafe impl<T> Sync for ForceSend<T> {}

impl<T> Deref for ForceSend<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: Clone> Clone for ForceSend<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deref_forwards_to_inner() {
        let wrapped = ForceSend(41u32);
        assert_eq!(*wrapped, 41);
    }

    #[test]
    fn clone_forwards_to_inner() {
        let wrapped = ForceSend(String::from("hi"));
        let cloned: ForceSend<String> = Clone::clone(&wrapped);
        assert_eq!(*cloned, "hi");
        assert_eq!(*wrapped, "hi", "the source must remain usable");
    }
}
