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

pub trait RandPuller: Send + Sync + Clone + 'static {
    type Error: Send + Unpin + 'static;
    fn pull(
        &mut self,
        range: &ProgressEntry,
    ) -> impl Future<Output = PullResult<impl PullStream<Self::Error>, Self::Error>> + Send;
}

pub trait SeqPuller: Send + 'static {
    type Error: Send + Unpin + 'static;
    fn pull(
        &mut self,
    ) -> impl Future<Output = PullResult<impl PullStream<Self::Error>, Self::Error>> + Send;
}
