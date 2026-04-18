use crate::{ProgressEntry, Pusher};
use bytes::Bytes;
use std::collections::BTreeMap;

#[derive(Debug)]
pub struct CacheSeqPusher<P> {
    inner: P,
    cache: BTreeMap<u64, Bytes>,
    cache_size: usize,
    high_watermark: usize,
    low_watermark: usize,
}

impl<P: Pusher> CacheSeqPusher<P> {
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
        let mut expected = None;
        while let Some(entry) = self.cache.first_entry() {
            let start = *entry.key();
            if self.cache_size <= target_size && Some(start) != expected {
                break;
            }
            let chunk = entry.remove();
            let chunk_len = chunk.len();
            let next_pos = start + chunk_len as u64;
            self.cache_size -= chunk_len;
            if let Err((e, ret)) = self.inner.push(&(start..next_pos), chunk) {
                if !ret.is_empty() {
                    self.cache_size += ret.len();
                    self.cache.insert(next_pos - ret.len() as u64, ret);
                }
                return Err(e);
            }
            expected = Some(next_pos);
        }
        Ok(())
    }
}

impl<P: Pusher> Pusher for CacheSeqPusher<P> {
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
