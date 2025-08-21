use crate::ProgressEntry;
use crate::base::data::SliceOrBytes;

pub trait ReadStream {
    type Error: Send;
    fn read_with<'a, F, Fut, Ret>(
        &'a mut self,
        read_fn: F,
    ) -> impl Future<Output = Result<Ret, Self::Error>>
    where
        F: FnOnce(SliceOrBytes<'a>) -> Fut,
        Fut: Future<Output = Ret>;
}

pub trait Puller: Send + Clone {
    type StreamError: Send;
    type Error: Send;
    fn init_read(
        &self,
        maybe_entry: Option<&ProgressEntry>,
    ) -> impl Future<
        Output = Result<impl ReadStream<Error = Self::StreamError> + Send + Unpin, Self::Error>,
    > + Send;
}
