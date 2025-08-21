use crate::base::data::SliceOrBytes;
use core::future;

pub trait WriteStream {
    type Error: Send;
    fn write(&mut self, buf: SliceOrBytes) -> impl Future<Output = Result<(), Self::Error>>;
    fn flush(&mut self) -> impl Future<Output = Result<(), Self::Error>> {
        future::ready(Ok(()))
    }
}

pub trait Pusher: Send {
    type Error: Send;
    type StreamError: Send;
    fn init_write(
        &self,
        start_point: u64,
    ) -> impl Future<Output = Result<impl WriteStream<Error = Self::StreamError>, Self::Error>>;
    fn flush(&mut self) -> impl Future<Output = Result<(), Self::Error>> {
        future::ready(Ok(()))
    }
}
