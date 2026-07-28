//! Progress range type and total-size computation.

use core::ops::Range;

/// A byte-range representing downloaded or to-be-downloaded progress.
///
/// Stored as a `Range<u64>` from `start` (inclusive) to `end` (exclusive).
pub type ProgressEntry = Range<u64>;

/// Trait for computing the total size from one or more [`ProgressEntry`] values.
pub trait Total {
    /// Total number of bytes represented by this progress value.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_entry_total() {
        assert_eq!((0..0).total(), 0);
        assert_eq!((5..10).total(), 5);
        assert_eq!((3..3).total(), 0);
        // A reversed range must not underflow: saturating_sub yields 0.
        let reversed = core::ops::Range { start: 10, end: 5 };
        assert_eq!(reversed.total(), 0);
    }

    #[test]
    fn vec_progress_total() {
        #![allow(clippy::single_range_in_vec_init)]
        let v: Vec<ProgressEntry> = vec![1..5, 8..10, 12..15];
        assert_eq!(v.total(), 9);
        let empty: Vec<ProgressEntry> = vec![];
        assert_eq!(empty.total(), 0);
    }
}
