use bytes::BytesMut;
use reqwest::blocking::Response;
use std::io::Read;
use std::time::Duration;
use std::{io, thread};

#[inline]
pub fn read_response(
    response: &mut Response,
    mut buffer: &mut BytesMut,
    retry_gap: Duration,
    mut on_error: impl FnMut(io::Error),
) -> usize {
    loop {
        unsafe { buffer.set_len(buffer.capacity()) };
        match response.read(&mut buffer) {
            Ok(len) => {
                unsafe { buffer.set_len(len) };
                break len;
            }
            Err(e) => on_error(e),
        }
        thread::sleep(retry_gap);
    }
}
