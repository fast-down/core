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
    /// retry after the configured backoff. Return `true` to stop retrying and
    /// abort the affected worker.
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
