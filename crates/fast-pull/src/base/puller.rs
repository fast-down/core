//! The [`Puller`](crate::Puller) trait: an abstraction over a chunked data source.

use crate::ProgressEntry;
use bytes::Bytes;
use core::time::Duration;
use futures::TryStream;

/// A pull stream that yields [`Bytes`] chunks.
///
/// Each chunk is accompanied by an optional retry delay on error.
pub trait PullStream<E>:
    TryStream<Ok = Bytes, Error = (E, Option<Duration>)> + Send + Unpin
{
}
impl<E, T> PullStream<E> for T where
    T: TryStream<Ok = Bytes, Error = (E, Option<Duration>)> + Send + Unpin
{
}
/// Result type returned by pulling operations.
///
/// On error, returns the error alongside an optional retry-after duration.
pub type PullResult<T, E> = Result<T, (E, Option<Duration>)>;

/// Abstraction over a data source that can be pulled (downloaded) in chunks.
///
/// Implementors produce a [`PullStream`] of bytes, optionally restricted to a
/// specific byte range. Cloning is required for retry and work-stealing scenarios.
pub trait Puller: Send + Sync + Clone + 'static {
    type Error: PullerError;
    /// Pull a (sub)range of the source as a stream of byte chunks.
    ///
    /// Passing `None` for `range` requests the entire source. The returned
    /// [`PullStream`] yields [`Bytes`] chunks; each error carries an optional
    /// retry delay that the engine honors via its retry backoff. Implementors
    /// must be `Clone` so workers can be spawned and work can be stolen/retried.
    fn pull(
        &mut self,
        range: Option<&ProgressEntry>,
    ) -> impl Future<Output = PullResult<impl PullStream<Self::Error>, Self::Error>> + Send;
}

/// Extension trait for pull errors, distinguishing recoverable from irrecoverable failures.
pub trait PullerError: std::error::Error + Send + Sync + Unpin + 'static {
    /// Whether an error is fatal and must **not** be retried.
    ///
    /// The default (`false`) means the error is recoverable and the engine will
    /// retry after the configured backoff. Implementors **must** override this to
    /// return `true` for fatal errors: forgetting to do so lets the engine retry
    /// indefinitely until its backoff gives up.
    fn is_irrecoverable(&self) -> bool {
        false
    }
}

impl PullerError for std::convert::Infallible {
    fn is_irrecoverable(&self) -> bool {
        #[allow(clippy::uninhabited_references)]
        match *self {}
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A `PullerError` that relies on the default `is_irrecoverable` impl.
    #[derive(Debug)]
    struct DefaultErr;
    impl std::fmt::Display for DefaultErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("default error")
        }
    }
    impl std::error::Error for DefaultErr {}
    impl PullerError for DefaultErr {}

    #[test]
    fn default_is_irrecoverable_is_false() {
        // Exercises the default `PullerError::is_irrecoverable` body (lines 49-51) and
        // the `Display` impl for `DefaultErr` (lines 70-72).
        assert!(!DefaultErr.is_irrecoverable());
        assert_eq!(format!("{DefaultErr}"), "default error");
    }

    /// A `PullerError` that overrides `is_irrecoverable` to report a fatal error.
    #[derive(Debug)]
    struct FatalErr;
    impl std::fmt::Display for FatalErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("fatal error")
        }
    }
    impl std::error::Error for FatalErr {}
    impl PullerError for FatalErr {
        fn is_irrecoverable(&self) -> bool {
            true
        }
    }

    #[test]
    fn override_is_irrecoverable_is_true() {
        // An error that overrides the method reports `true`, which is how the
        // engine learns to stop retrying. Pinned alongside the default `false`.
        assert!(FatalErr.is_irrecoverable());
        assert!(!DefaultErr.is_irrecoverable());
    }

    #[test]
    fn irrecoverable_contract_allows_dynamic_decision() {
        // The answer may depend on the error's own state; it is not required to
        // be a compile-time constant per type.
        #[derive(Debug)]
        struct StatefulErr {
            fatal: bool,
        }
        impl std::fmt::Display for StatefulErr {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("stateful")
            }
        }
        impl std::error::Error for StatefulErr {}
        impl PullerError for StatefulErr {
            fn is_irrecoverable(&self) -> bool {
                self.fatal
            }
        }
        assert!(!StatefulErr { fatal: false }.is_irrecoverable());
        assert!(StatefulErr { fatal: true }.is_irrecoverable());
    }
}
