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
pub type PullResult<E, S> = Result<S, (E, Option<Duration>)>;

pub trait RandPuller: Send + Clone {
    type Error: Send;
    fn pull(
        &mut self,
        range: &ProgressEntry,
    ) -> impl Future<Output = PullResult<Self::Error, impl PullStream<Self::Error>>> + Send;
}

pub trait SeqPuller: Send {
    type Error: Send;
    fn pull(
        &mut self,
    ) -> impl Future<Output = PullResult<Self::Error, impl PullStream<Self::Error>>> + Send;
}
