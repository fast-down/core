//! A deterministic in-memory [`Puller`](crate::Puller) for tests, plus a helper
//! to build mock payloads.

use crate::{ProgressEntry, PullResult, PullStream, Puller};
use futures::stream;
use std::{sync::Arc, vec::Vec};

/// Build a deterministic byte array for mock testing.
///
/// The payload is a xorshift64 keystream, which has no short period: distinct
/// equal-length windows of the result differ. This matters because the usual
/// end-to-end check is `assert_eq!(downloaded, build_mock_data(size))`, and a
/// periodic payload makes that assertion blind to whole blocks written at the wrong
/// offset or swapped between workers.
///
/// The result is prefix-stable: `build_mock_data(n)` is the first `n` bytes of
/// `build_mock_data(m)` for every `m >= n`, so a range of the full array can be used
/// as the expected value for a partial download.
#[must_use]
pub fn build_mock_data(size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size.next_multiple_of(8));
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    while out.len() < size {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(size);
    out
}

/// A [`Puller`] implementation backed by an in-memory byte slice, used for testing.
#[derive(Debug, Clone)]
pub struct MockPuller(pub Arc<[u8]>);
impl MockPuller {
    #[must_use]
    pub fn new(data: &[u8]) -> Self {
        Self(Arc::from(data))
    }
}
impl Puller for MockPuller {
    type Error = std::convert::Infallible;
    fn pull(
        &mut self,
        range: Option<&ProgressEntry>,
    ) -> impl Future<Output = PullResult<impl PullStream<Self::Error>, Self::Error>> {
        let data = match range {
            #[allow(clippy::cast_possible_truncation)]
            Some(r) => &self.0[r.start as usize..r.end as usize],
            None => &self.0,
        };
        std::future::ready(Ok(stream::iter(
            data.chunks(2).map(|c| Ok(c.iter().copied().collect())),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::TryStreamExt;

    async fn pull_all(puller: &mut MockPuller, range: Option<&ProgressEntry>) -> Vec<u8> {
        let mut stream = puller.pull(range).await.unwrap();
        let mut got = Vec::new();
        while let Some(chunk) = stream.try_next().await.unwrap() {
            got.extend_from_slice(&chunk[..]);
        }
        got
    }

    // The mock must yield exactly the requested byte range, reassembled in order,
    // regardless of its internal 2-byte chunking.
    #[tokio::test]
    async fn mock_puller_yields_exact_range_in_order() {
        let data = build_mock_data(30);
        let mut puller = MockPuller::new(&data);
        let got = pull_all(&mut puller, Some(&(10..20))).await;
        assert_eq!(got, data[10..20]);
    }

    // `None` requests the entire source.
    #[tokio::test]
    async fn mock_puller_none_yields_full_source() {
        let data = build_mock_data(17);
        let mut puller = MockPuller::new(&data);
        let got = pull_all(&mut puller, None).await;
        assert_eq!(got, data);
    }

    // Callers slice the full array to build the expected value for a partial range,
    // so a shorter build must be a prefix of a longer one, and repeated calls must
    // agree.
    #[test]
    fn build_mock_data_is_deterministic_and_prefix_stable() {
        let long = build_mock_data(1024);
        for size in [0, 1, 7, 8, 9, 300, 1024] {
            let short = build_mock_data(size);
            assert_eq!(short.len(), size);
            assert_eq!(short, build_mock_data(size));
            assert_eq!(short[..], long[..size]);
        }
    }

    // End-to-end assertions compare the whole payload, so they can only detect a
    // block written at the wrong offset if no two windows of the payload are equal.
    // A periodic pattern such as `i % 256` fails this and silently accepts any swap
    // of two 256-aligned blocks.
    #[test]
    fn build_mock_data_has_no_repeating_window() {
        let data = build_mock_data(64 * 1024);
        let mut seen = std::collections::HashSet::with_capacity(data.len());
        for window in data.windows(8) {
            assert!(seen.insert(window), "window {window:?} occurs twice");
        }
    }
}
