use crate::ProgressEntry;
use bytes::Bytes;
use core::time::Duration;
use futures::TryStream;

pub trait PullStream<E>:
    TryStream<Ok = Bytes, Error = (E, Option<Duration>)> + Send + Unpin
{
}
impl<E, T: TryStream<Ok = Bytes, Error = (E, Option<Duration>)> + Send + Unpin> PullStream<E>
    for T
{
}
pub type PullResult<T, E> = Result<T, (E, Option<Duration>)>;

pub trait RandPuller: Send + Clone {
    type Error: Send;
    fn pull(
        &mut self,
        range: &ProgressEntry,
    ) -> impl Future<Output = PullResult<impl PullStream<Self::Error>, Self::Error>> + Send;
}

pub trait SeqPuller: Send {
    type Error: Send;
    fn pull(
        &mut self,
    ) -> impl Future<Output = PullResult<impl PullStream<Self::Error>, Self::Error>> + Send;
}
