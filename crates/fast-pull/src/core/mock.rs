use crate::{ProgressEntry, PullResult, PullStream, Puller, PullerError};
use futures::stream;
use std::{sync::Arc, vec::Vec};

#[must_use]
pub fn build_mock_data(size: usize) -> Vec<u8> {
    #[allow(clippy::cast_possible_truncation)]
    (0..size).map(|i| (i % 256) as u8).collect()
}

#[derive(Debug, Clone)]
pub struct MockPuller(pub Arc<[u8]>);
impl MockPuller {
    #[must_use]
    pub fn new(data: &[u8]) -> Self {
        Self(Arc::from(data))
    }
}
impl Puller for MockPuller {
    type Error = ();
    async fn pull(
        &mut self,
        range: Option<&ProgressEntry>,
    ) -> PullResult<impl PullStream<Self::Error>, Self::Error> {
        let data = match range {
            #[allow(clippy::cast_possible_truncation)]
            Some(r) => &self.0[r.start as usize..r.end as usize],
            None => &self.0,
        };
        Ok(stream::iter(
            data.chunks(2).map(|c| Ok(c.iter().copied().collect())),
        ))
    }
}
impl PullerError for () {}
