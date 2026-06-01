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
    fn pull(
        &mut self,
        range: Option<&ProgressEntry>,
    ) -> impl Future<Output = PullResult<impl PullStream<Self::Error>, Self::Error>> + Send;
}

/// Extension trait for pull errors, distinguishing recoverable from irrecoverable failures.
pub trait PullerError: Send + Unpin + 'static {
    fn is_irrecoverable(&self) -> bool {
        false
    }
}
