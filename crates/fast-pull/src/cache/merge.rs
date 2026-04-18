use crate::{ProgressEntry, Pusher};
use bytes::{Bytes, BytesMut};
use std::collections::{BTreeMap, btree_map::Entry};

/// 优先选择大块调用 push，并且会把大块合并成一个 Bytes
#[derive(Debug)]
pub struct CacheMergePusher<P> {
    inner: P,
    cache: BTreeMap<u64, Bytes>,
    cache_size: usize,
    high_watermark: usize,
    low_watermark: usize,
}

impl<P: Pusher> CacheMergePusher<P> {
    pub const fn new(inner: P, high_watermark: usize, low_watermark: usize) -> Self {
        Self {
            inner,
            cache: BTreeMap::new(),
            cache_size: 0,
            high_watermark,
            low_watermark,
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

impl<P: Pusher> Pusher for CacheMergePusher<P> {
    type Error = P::Error;

    fn push(&mut self, range: &ProgressEntry, bytes: Bytes) -> Result<(), (Self::Error, Bytes)> {
        if bytes.is_empty() {
            return Ok(());
        }

        self.cache_size += bytes.len();
        if let Some(old_bytes) = self.cache.insert(range.start, bytes) {
            self.cache_size -= old_bytes.len();
        }

        if self.cache_size >= self.high_watermark
            && let Err(e) = self.evict_until(self.low_watermark)
        {
            return Err((e, Bytes::new()));
        }

        Ok(())
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.evict_until(0)?;
        self.inner.flush()
    }
}
