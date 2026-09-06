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

/// Total number of bytes across all entries, computed by saturating the sum of
/// each entry's length at [`u64::MAX`].
///
/// # Preconditions
///
/// The entries must be **disjoint** (non-overlapping), as produced by
/// [`Merge::merge_progress`](crate::Merge::merge_progress). Overlapping entries are
/// silently counted twice, inflating the total. If the combined length exceeds
/// [`u64::MAX`], the result is [`u64::MAX`].
impl Total for Vec<ProgressEntry> {
    fn total(&self) -> u64 {
        self.iter()
            .fold(0, |total, entry| total.saturating_add(entry.total()))
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

    #[test]
    fn progress_entry_total_reversed_ranges_are_safe() {
        // A reversed range must saturate to 0 rather than underflow.
        assert_eq!((core::ops::Range { start: 10, end: 5 }).total(), 0);
        assert_eq!(
            (core::ops::Range {
                start: u64::MAX,
                end: 0
            })
            .total(),
            0
        );
        assert_eq!(
            (core::ops::Range {
                start: 100,
                end: 99
            })
            .total(),
            0
        );
        assert_eq!((core::ops::Range { start: 1, end: 0 }).total(), 0);
    }

    #[test]
    fn progress_entry_total_extreme_and_degenerate() {
        // Extreme and empty ranges have well-defined lengths.
        assert_eq!((0..u64::MAX).total(), u64::MAX);
        assert_eq!((u64::MAX..u64::MAX).total(), 0);
        assert_eq!((42..42).total(), 0);
        assert_eq!((0..0).total(), 0);
    }

    #[test]
    fn vec_progress_total_counts_overlap_twice() {
        #![allow(clippy::single_range_in_vec_init)]
        // `total` sums lengths naively, so overlapping entries are counted twice.
        // This pins the documented behaviour: callers must pass disjoint entries.
        let v: Vec<ProgressEntry> = vec![0..5, 3..10];
        // (5 - 0) + (10 - 3) = 12, whereas the union 0..10 is only 10 bytes.
        assert_eq!(v.total(), 12);
    }

    #[test]
    fn vec_progress_total_overflow_saturates() {
        let v: Vec<ProgressEntry> = vec![0..u64::MAX, 0..1];
        assert_eq!(v.total(), u64::MAX);
    }
}
