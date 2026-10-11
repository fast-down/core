//! Crate-owned wrapper errors that keep third-party error types out of the
//! public API.
//!
//! The engine is built on `reqwest` (and the pulled-in worker errors), but
//! `reqwest` is not re-exported, so exposing its types would force every
//! consumer to depend on the exact same version. [`TaskError`] holds the
//! foreign error as a private boxed trait object and forwards
//! [`Display`](std::fmt::Display) / [`Error::source`](std::error::Error::source),
//! so callers can still render and inspect the cause without naming the crate.
//! Client-construction failures use the engine's own
//! [`BuildClientError`](fast_down::reqwest::BuildClientError), which is already
//! opaque.

use std::error::Error;

/// A failure from a download worker (network, decode, or write).
///
/// Wraps an opaque boxed error so no foreign error type appears in the public
/// API. Use [`downcast_ref`](Self::downcast_ref) to recover a concrete cause.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct TaskError(Box<dyn Error + Send + Sync + 'static>);

impl TaskError {
    /// Wrap a worker error, erasing its concrete type.
    pub fn new(error: impl Error + Send + Sync + 'static) -> Self {
        Self(Box::new(error))
    }

    /// Attempt to recover the concrete error this wraps.
    #[must_use]
    pub fn downcast_ref<T: Error + 'static>(&self) -> Option<&T> {
        self.0.downcast_ref()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::fmt;

    #[derive(Debug)]
    struct Boom;
    impl fmt::Display for Boom {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("boom")
        }
    }
    impl Error for Boom {}

    #[test]
    fn task_error_forwards_display_and_downcasts() {
        let err = TaskError::new(Boom);
        assert_eq!(err.to_string(), "boom");
        assert!(err.downcast_ref::<Boom>().is_some());
        assert!(err.downcast_ref::<std::io::Error>().is_none());
    }

    #[test]
    fn task_error_source_is_forwarded() {
        let err = TaskError::new(Boom);
        // `Boom` has no source of its own.
        assert!(Error::source(&err).is_none());
    }
}
