use alloc::vec::Vec;
use crate::{Pusher, ReadStream, SliceOrBytes, WriteStream};

impl ReadStream for &'_ [u8] {
    type Error = core::convert::Infallible;

    async fn read_with<'a, F, Fut, Ret>(&'a mut self, read_fn: F) -> Result<Ret, Self::Error>
    where
        F: FnOnce(SliceOrBytes<'a>) -> Fut,
        Fut: Future<Output=Ret>
    {
        Ok(read_fn(SliceOrBytes::from(*self)).await)
    }
}

impl WriteStream for &mut Vec<u8> {
    type Error = core::convert::Infallible;

    async fn write(&mut self, buf: SliceOrBytes<'_>) -> Result<(), Self::Error> {
        self.extend_from_slice(&buf);
        Ok(())
    }
}

#[cfg(feature = "std")]
pub mod std {
    extern crate std;

    use alloc::vec::Vec;
    use bytes::BytesMut;
    use tokio::io::SeekFrom;
    use crate::{ReadStream, SliceOrBytes, WriteStream};

    pub struct CursorWriter<T>(pub u64, pub T);

    impl<T> CursorWriter<T> {
        pub fn new(writer: T) -> Self {
            Self(0, writer)
        }

        pub fn with_start_point(writer: T, start_point: u64) -> Self {
            Self(start_point, writer)
        }
    }

    impl<T: std::io::Write + std::io::Seek> WriteStream for CursorWriter<T> {
        type Error = std::io::Error;

        async fn write(&mut self, buf: SliceOrBytes<'_>) -> Result<(), Self::Error> {
            self.1.seek(SeekFrom::Start(self.0))?;
            self.1.write_all(&buf)?;
            self.0 += buf.len() as u64;
            Ok(())
        }
    }

    pub struct StdWriter<T>(pub T);

    impl<T: std::io::Write> WriteStream for StdWriter<T> {
        type Error = std::io::Error;

        async fn write(&mut self, buf: SliceOrBytes<'_>) -> Result<(), Self::Error> {
            self.0.write_all(&buf)?;
            Ok(())
        }
    }

    pub struct StdReader<T, const N: usize>(pub T, [u8; N]);

    impl<T: std::io::Read, const N: usize> ReadStream for StdReader<T, N> {
        type Error = std::io::Error;

        async fn read_with<'a, F, Fut, Ret>(&'a mut self, read_fn: F) -> Result<Ret, Self::Error>
        where
            F: FnOnce(SliceOrBytes<'a>) -> Fut,
            Fut: Future<Output=Ret>
        {
            let len = self.0.read(&mut self.1)?;
            Ok(read_fn(SliceOrBytes::from(&self.1[0..len])).await)
        }
    }

    pub struct StdReaderStackLocal<T, const N: usize>(pub T);

    impl<T: std::io::Read, const N: usize> ReadStream for StdReaderStackLocal<T, N> {
        type Error = std::io::Error;

        async fn read_with<'a, F, Fut, Ret>(&'a mut self, read_fn: F) -> Result<Ret, Self::Error>
        where
            F: FnOnce(SliceOrBytes<'a>) -> Fut,
            Fut: Future<Output=Ret>
        {
            let mut buffer = [0u8; N];
            let len = self.0.read(&mut buffer)?;
            Ok(read_fn(unsafe { SliceOrBytes::from_slice_any_lifetime(&buffer[0..len]) }).await)
        }
    }

    pub struct StdReaderBytes<T>(pub usize, pub T);

    impl<T> StdReaderBytes<T> {
        pub fn new(reader: T) -> Self {
            Self::with_capacity(reader, 2048)
        }

        pub fn with_capacity(reader: T, cap: usize) -> Self {
            Self(cap, reader)
        }
    }

    impl<T: std::io::Read> ReadStream for StdReaderBytes<T> {
        type Error = std::io::Error;

        async fn read_with<'a, F, Fut, Ret>(&'a mut self, read_fn: F) -> Result<Ret, Self::Error>
        where
            F: FnOnce(SliceOrBytes<'a>) -> Fut,
            Fut: Future<Output=Ret>
        {
            let mut bytes = BytesMut::with_capacity(self.0);
            bytes.resize(self.0, 0);
            let len = self.1.read(&mut bytes)?;
            Ok(read_fn(SliceOrBytes::Bytes(bytes.freeze().split_to(len))).await)
        }
    }
}
