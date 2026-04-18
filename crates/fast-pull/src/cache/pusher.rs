use crate::{ProgressEntry, Pusher};
use bytes::{Bytes, BytesMut};
use std::collections::{BTreeMap, btree_map::Entry};

#[derive(Debug)]
pub struct CachePusher<P> {
    inner: P,
    cache: BTreeMap<u64, Bytes>,
    cache_size: usize,
    buffer_size: usize,
}

impl<P: Pusher> CachePusher<P> {
    pub const fn new(inner: P, buffer_size: usize) -> Self {
        Self {
            inner,
            cache: BTreeMap::new(),
            cache_size: 0,
            buffer_size,
        }
    }

    fn evict_until(&mut self, target_size: usize) -> Result<(), P::Error> {
        if self.cache_size <= target_size {
            return Ok(());
        }

        let mut runs: Vec<(u64, usize)> = Vec::with_capacity(self.cache.len());
        let mut curr_start = None;
        let mut curr_len = 0;
        let mut expected_next = 0;

        for (&start, bytes) in &self.cache {
            let len = bytes.len();
            if let Some(c_start) = curr_start {
                if start == expected_next {
                    curr_len += len;
                    expected_next += len as u64;
                } else {
                    runs.push((c_start, curr_len));
                    curr_start = Some(start);
                    curr_len = len;
                    expected_next = start + len as u64;
                }
            } else {
                curr_start = Some(start);
                curr_len = len;
                expected_next = start + len as u64;
            }
        }
        if let Some(c_start) = curr_start {
            runs.push((c_start, curr_len));
        }
        runs.sort_unstable_by_key(|&(_, len)| std::cmp::Reverse(len));

        let mut curr_buf = BytesMut::with_capacity(self.cache_size);
        let mut err = None;
        for (start, total_len) in runs {
            let need_push = err.is_none() && self.cache_size > target_size;
            let first_bytes = match self.cache.entry(start) {
                Entry::Occupied(entry) => {
                    let is_merged = entry.get().len() == total_len;
                    if !need_push && is_merged {
                        continue;
                    }
                    entry.remove()
                }
                Entry::Vacant(_) => unreachable!(),
            };
            let chunk = if first_bytes.len() == total_len {
                first_bytes
            } else {
                curr_buf.extend_from_slice(&first_bytes);
                let mut curr_key = start + first_bytes.len() as u64;
                let end = start + total_len as u64;
                while curr_key < end {
                    let bytes = self.cache.remove(&curr_key).unwrap();
                    curr_buf.extend_from_slice(&bytes);
                    curr_key += bytes.len() as u64;
                }
                curr_buf.split().freeze()
            };
            if need_push {
                let end = start + total_len as u64;
                let range = start..end;
                self.cache_size -= total_len;
                if let Err((e, ret_bytes)) = self.inner.push(&range, chunk) {
                    err = Some(e);
                    if !ret_bytes.is_empty() {
                        self.cache_size += ret_bytes.len();
                        let retry_start = start + (total_len - ret_bytes.len()) as u64;
                        self.cache.insert(retry_start, ret_bytes);
                    }
                }
            } else {
                self.cache.insert(start, chunk);
            }
        }
        err.map_or(Ok(()), Err)
    }
}

impl<P: Pusher> Pusher for CachePusher<P> {
    type Error = P::Error;

    fn push(&mut self, range: &ProgressEntry, bytes: Bytes) -> Result<(), (Self::Error, Bytes)> {
        if bytes.is_empty() {
            return Ok(());
        }

        self.cache_size += bytes.len();
        if let Some(old_bytes) = self.cache.insert(range.start, bytes) {
            self.cache_size -= old_bytes.len();
        }

        if self.cache_size >= self.buffer_size {
            let low_watermark = self.buffer_size / 2;
            if let Err(e) = self.evict_until(low_watermark) {
                return Err((e, Bytes::new()));
            }
        }

        Ok(())
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.evict_until(0)?;
        self.inner.flush()
    }
}
