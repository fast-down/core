extern crate alloc;

use bytes::Bytes;
use core::borrow::Borrow;
use core::ops::Deref;

/// [`Bytes`] version of Rust std [`Cow`][`alloc::borrow::Cow`]
#[derive(Clone)]
pub enum SliceOrBytes<'a> {
    Bytes(Bytes),
    Slice(&'a [u8]),
}

impl SliceOrBytes<'_> {
    pub const fn empty() -> Self {
        Self::Bytes(Bytes::new())
    }

    pub fn into_bytes(self) -> Bytes {
        match self {
            SliceOrBytes::Bytes(bytes) => bytes,
            SliceOrBytes::Slice(slice) => Bytes::copy_from_slice(slice),
        }
    }
}

impl<'a> From<&'a [u8]> for SliceOrBytes<'a> {
    fn from(value: &'a [u8]) -> Self {
        Self::Slice(value)
    }
}

impl From<Bytes> for SliceOrBytes<'_> {
    fn from(value: Bytes) -> Self {
        Self::Bytes(value)
    }
}

impl Into<Bytes> for SliceOrBytes<'_> {
    fn into(self) -> Bytes {
        self.into_bytes()
    }
}

impl Deref for SliceOrBytes<'_> {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        match self {
            SliceOrBytes::Bytes(bytes) => &bytes,
            SliceOrBytes::Slice(slice) => slice,
        }
    }
}

impl Borrow<[u8]> for SliceOrBytes<'_> {
    fn borrow(&self) -> &[u8] {
        match self {
            SliceOrBytes::Bytes(bytes) => bytes.borrow(),
            SliceOrBytes::Slice(slice) => slice.borrow(),
        }
    }
}
