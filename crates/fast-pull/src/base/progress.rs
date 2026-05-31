use core::ops::Range;

/// A byte-range representing downloaded or to-be-downloaded progress.
///
/// Stored as a `Range<u64>` from `start` (inclusive) to `end` (exclusive).
pub type ProgressEntry = Range<u64>;

/// Trait for computing the total size from one or more [`ProgressEntry`] values.
pub trait Total {
    fn total(&self) -> u64;
}

impl Total for ProgressEntry {
    #[allow(clippy::inline_always)]
    #[inline(always)]
    fn total(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }
}

impl Total for Vec<ProgressEntry> {
    fn total(&self) -> u64 {
        self.iter().map(Total::total).sum()
    }
}
