use crate::{ProgressEntry, Pusher};
use bytes::Bytes;
use std::collections::BTreeMap;

/// 优先选择大块调用 push 但是不含合并过程
#[derive(Debug)]
pub struct CacheDirectPusher<P> {
    inner: P,
    cache: BTreeMap<u64, Bytes>,
    cache_size: usize,
    high_watermark: usize,
    low_watermark: usize,
}

impl<P: Pusher> CacheDirectPusher<P> {
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

        for (mut start, mut total_len) in runs {
            while total_len > 0 {
                let chunk = self.cache.remove(&start).unwrap();
                let len = chunk.len();
                self.cache_size -= len;
                total_len -= len;
                let range = start..start + len as u64;
                if let Err((e, ret_bytes)) = self.inner.push(&range, chunk) {
                    if !ret_bytes.is_empty() {
                        self.cache_size += ret_bytes.len();
                        let retry_start = start + (len - ret_bytes.len()) as u64;
                        self.cache.insert(retry_start, ret_bytes);
                    }
                    return Err(e);
                }
                start += len as u64;
            }
            if self.cache_size <= target_size {
                break;
            }
        }
        Ok(())
    }
}

impl<P: Pusher> Pusher for CacheDirectPusher<P> {
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
