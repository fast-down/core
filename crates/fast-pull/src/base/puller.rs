use crate::ProgressEntry;
use bytes::Bytes;
use core::time::Duration;
use futures::TryStream;

pub trait PullStream<E>:
    TryStream<Ok = Bytes, Error = (E, Option<Duration>)> + Send + Unpin
{
}
impl<E, T> PullStream<E> for T where
    T: TryStream<Ok = Bytes, Error = (E, Option<Duration>)> + Send + Unpin
{
}
pub type PullResult<T, E> = Result<T, (E, Option<Duration>)>;

pub trait Puller: Send + Sync + Clone + 'static {
    type Error: PullerError;
    fn pull(
        &mut self,
        range: Option<&ProgressEntry>,
    ) -> impl Future<Output = PullResult<impl PullStream<Self::Error>, Self::Error>> + Send;
}

pub trait PullerError: Send + Unpin + 'static {
    fn is_irrecoverable(&self) -> bool {
        false
    }
}
