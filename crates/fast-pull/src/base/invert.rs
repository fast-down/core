//! Iterator and helper for computing the *gaps* (not-yet-downloaded ranges)
//! from a set of [`ProgressEntry`](crate::ProgressEntry)s.

use crate::ProgressEntry;

/// Iterator that yields the *gaps* (non-downloaded ranges) from a list of [`ProgressEntry`]s.
///
/// Entries shorter than `window` are merged into adjacent gaps to reduce fragmentation.
///
/// # Preconditions
///
/// The input entries must be **sorted by `start` and non-overlapping**, as produced by
/// [`Merge::merge_progress`](crate::Merge::merge_progress). Unsorted or overlapping input
/// makes the iterator emit nonsensical reversed gaps; a `debug_assert` catches it in
/// debug builds. Reversed entries (`start > end`) are tolerated and ignored.
#[derive(Debug)]
pub struct InvertIter<I: Iterator<Item = ProgressEntry>> {
    /// Iterator over the already-downloaded (sorted) ranges.
    iter: I,
    /// End offset of the last range consumed from `iter`.
    prev_end: u64,
    /// Total size of the source.
    total_size: u64,
    /// Merge entries shorter than this into the surrounding gap.
    window: u64,
}

impl<I> Iterator for InvertIter<I>
where
    I: Iterator<Item = ProgressEntry>,
{
    type Item = ProgressEntry;
    fn next(&mut self) -> Option<Self::Item> {
        let mut gap_start = self.prev_end;
        let mut last_end = gap_start;
        for range in self.iter.by_ref() {
            // Only the ordering precondition is checked here. A reversed range
            // (`start > end`) is a tolerated no-op -- the `saturating_sub` below
            // gives it length 0 so it is absorbed into the surrounding gap -- and
            // must therefore not trip this assertion.
            debug_assert!(
                range.start >= last_end,
                "InvertIter requires sorted, non-overlapping ranges, but got {range:?} \
                 after a range ending at {last_end}; merge the input first"
            );
            last_end = range.end;
            if range.start == gap_start {
                gap_start = range.end;
                continue;
            }
            let len = range.end.saturating_sub(range.start);
            if len >= self.window {
                self.prev_end = range.end;
                return Some(gap_start..range.start);
            }
        }
        if gap_start < self.total_size {
            self.prev_end = self.total_size;
            Some(gap_start..self.total_size)
        } else {
            None
        }
    }
}

/// `window`: when a [`ProgressEntry`] length is less than `window`, it is merged into the gap to reduce progress fragmentation.
pub const fn invert<I>(progress: I, total_size: u64, window: u64) -> InvertIter<I>
where
    I: Iterator<Item = ProgressEntry>,
{
    InvertIter {
        iter: progress,
        prev_end: 0,
        total_size,
        window,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::single_range_in_vec_init)]
    use super::*;

    fn invert_vec(progress: &[ProgressEntry], total_size: u64, window: u64) -> Vec<ProgressEntry> {
        invert(progress.iter().cloned(), total_size, window).collect()
    }

    #[test]
    fn test_windowed_invert() {
        assert_eq!(invert_vec(&[10..20], 30, 1), [0..10, 20..30]);
        assert_eq!(invert_vec(&[10..12], 30, 5), [0..30]);
        assert_eq!(invert_vec(&[10..20, 25..27], 30, 5), [0..10, 20..30]);
        assert_eq!(invert_vec(&[10..14, 25..27, 30..32], 50, 5), [0..50]);
        assert_eq!(invert_vec(&[10..14, 25..49], 50, 5), [0..25, 49..50]);
        assert_eq!(invert_vec(&[2..4, 6..8, 10..12], 15, 5), [0..15]);
        assert_eq!(invert_vec(&[0..2, 10..20], 30, 5), [2..10, 20..30]);
    }

    #[test]
    fn test_invert_empty_progress() {
        // Nothing downloaded of a 50-byte file -> one gap spanning everything.
        assert_eq!(invert_vec(&[], 50, 1), [0..50]);
    }

    #[test]
    fn test_invert_zero_total_size() {
        // total_size 0 -> no gaps, even if progress is present.
        assert_eq!(invert_vec(&[0..5], 0, 1), []);
    }

    #[test]
    fn test_invert_full_cover_no_gaps() {
        assert_eq!(invert_vec(&[0..30], 30, 1), []);
    }

    #[test]
    fn test_invert_window_zero_keeps_small_entries() {
        #![allow(clippy::single_range_in_vec_init)]
        // window=0 means every entry (even tiny) is kept, so small entries are
        // not merged into the surrounding gap.
        assert_eq!(invert_vec(&[10..12], 30, 0), [0..10, 12..30]);
    }

    #[test]
    fn test_invert_trailing_gap_only() {
        assert_eq!(invert_vec(&[0..20], 30, 1), [20..30]);
    }

    #[test]
    fn test_invert_leading_gap_only() {
        assert_eq!(invert_vec(&[10..30], 30, 1), [0..10]);
    }

    #[test]
    fn test_invert_contiguous_then_gap() {
        #![allow(clippy::single_range_in_vec_init)]
        assert_eq!(invert_vec(&[0..10, 10..20], 30, 1), [20..30]);
    }

    #[test]
    fn test_invert_reversed_range_is_safe_and_ignored() {
        // A reversed entry (start > end) must not underflow the length
        // computation. It gets length 0 and is absorbed into the surrounding
        // gap, so it is harmlessly ignored instead of corrupting the output.
        let reversed = core::ops::Range { start: 30, end: 5 };
        assert_eq!(invert_vec(&[reversed], 50, 1), [0..50]);
    }

    #[test]
    fn invert_window_boundary_len_equals_window_is_emitted() {
        // The window test is `len >= window`, so a length exactly equal to the
        // window keeps the entry and the surrounding gaps stay separate.
        assert_eq!(invert_vec(&[0..5, 10..15], 20, 5), [5..10, 15..20]);
    }

    #[test]
    fn invert_window_boundary_len_below_window_is_absorbed() {
        // One byte below the window is merged into the surrounding gap.
        assert_eq!(invert_vec(&[0..5, 10..14], 20, 5), [5..20]);
    }

    #[test]
    fn invert_window_does_not_merge_small_gaps_between_large_ranges() {
        // `window` only suppresses short *downloaded* entries; it never swallows
        // a gap. Two large entries separated by a 2-byte gap still yield that
        // gap, because those 2 bytes genuinely are missing.
        assert_eq!(invert_vec(&[0..10, 12..22], 22, 5), [10..12]);
    }

    #[test]
    fn invert_trailing_gap_emitted_regardless_of_window() {
        // A trailing gap is real missing data, so it is emitted even when it is
        // far shorter than the window.
        assert_eq!(invert_vec(&[0..28], 30, 5), [28..30]);
        assert_eq!(invert_vec(&[0..28], 30, 100), [28..30]);
    }

    #[test]
    fn invert_multiple_gaps_across_next_calls() {
        // Each `next()` yields at most one gap, so `prev_end` must carry over
        // correctly between calls.
        let mut it = invert([0..5, 10..20, 25..30].iter().cloned(), 30, 1);
        assert_eq!(it.next(), Some(5..10));
        assert_eq!(it.next(), Some(20..25));
        assert_eq!(it.next(), None);
    }

    #[test]
    fn invert_small_entry_contiguous_to_prior_is_not_a_gap() {
        // A short entry touching the previous one counts as downloaded rather
        // than being turned into a gap.
        assert_eq!(invert_vec(&[0..10, 10..12], 30, 5), [12..30]);
    }

    #[test]
    fn invert_reversed_entry_in_middle_is_ignored() {
        // Reversed entries are tolerated anywhere in the stream, not just first.
        let reversed = core::ops::Range { start: 30, end: 5 };
        assert_eq!(invert_vec(&[0..10, reversed], 50, 1), [10..50]);
    }

    #[test]
    #[cfg_attr(debug_assertions, should_panic)]
    #[allow(clippy::should_panic_without_expect)]
    fn invert_overlapping_input_fails_fast() {
        // Overlapping input violates the precondition and would otherwise emit a
        // reversed gap silently; the debug assertion turns it into a panic.
        let _ = invert_vec(&[0..10, 5..15], 20, 1);
    }

    #[test]
    #[cfg_attr(debug_assertions, should_panic)]
    #[allow(clippy::should_panic_without_expect)]
    fn invert_unsorted_input_fails_fast() {
        // Same for unsorted input.
        let _ = invert_vec(&[10..20, 0..5], 20, 1);
    }
}
