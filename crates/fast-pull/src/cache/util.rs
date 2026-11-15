//! Helpers shared by the cache decorators.

use bytes::Bytes;
use std::collections::BTreeMap;

/// Group the buffered chunks into contiguous runs, longest first.
///
/// Each entry is `(start, len)`; the order decides which run a watermark-driven
/// eviction drains first.
pub(super) fn runs_longest_first(cache: &BTreeMap<u64, Bytes>) -> Vec<(u64, usize)> {
    let mut runs: Vec<(u64, usize)> = Vec::with_capacity(cache.len());
    let mut curr_start = None;
    let mut curr_len = 0;
    let mut expected_next = 0;

    for (&start, bytes) in cache {
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
    runs
}

/// Re-buffer the tail an inner `push` refused, at the offset just past the
/// bytes it did write. `total_len` is the length of the chunk that was pushed.
pub(super) fn rebuffer_tail(
    cache: &mut BTreeMap<u64, Bytes>,
    cache_size: &mut usize,
    start: u64,
    total_len: usize,
    rem: Bytes,
) {
    if rem.is_empty() {
        return;
    }
    let written = total_len.saturating_sub(rem.len());
    *cache_size += rem.len();
    if let Some(old) = cache.insert(start + written as u64, rem) {
        *cache_size -= old.len();
    }
}
