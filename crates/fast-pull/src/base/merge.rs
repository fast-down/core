//! Merging of [`ProgressEntry`](crate::ProgressEntry) ranges into a sorted list.

use crate::ProgressEntry;

/// Trait for merging a new [`ProgressEntry`] into a sorted list of existing entries.
///
/// Used to consolidate downloaded ranges and remove redundant gaps.
pub trait Merge {
    /// Merge `new` into the existing (sorted) progress list, coalescing overlaps
    /// so the list stays sorted and gap-free where ranges touch.
    ///
    /// # Preconditions
    ///
    /// The list must already be **sorted by `start` and non-overlapping**. This
    /// holds automatically for lists built exclusively through this method; it is
    /// checked by a `debug_assert` for hand-built lists.
    fn merge_progress(&mut self, new: ProgressEntry);
}

impl Merge for Vec<ProgressEntry> {
    fn merge_progress(&mut self, new: ProgressEntry) {
        debug_assert!(
            self.windows(2).all(|w| w[0].end <= w[1].start),
            "merge_progress requires a sorted, non-overlapping list; merge the entries first"
        );
        if new.start >= new.end {
            return;
        }
        let i = self.partition_point(|x| x.end < new.start);
        if i == self.len() {
            self.push(new);
            return;
        }
        if self[i].start <= new.start && self[i].end >= new.end {
            return;
        }
        let mut current_merge = new;
        let mut j = i;
        while j < self.len() {
            let entry = &self[j];
            if entry.start > current_merge.end {
                break;
            }
            current_merge.start = current_merge.start.min(entry.start);
            current_merge.end = current_merge.end.max(entry.end);
            j += 1;
        }
        if j > i {
            self.drain(i..j);
        }
        self.insert(i, current_merge);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge() {
        #![allow(clippy::single_range_in_vec_init)]
        let mut v = vec![1..5, 8..10];
        v.merge_progress(5..10);
        assert_eq!(v, vec![1..10]);
        v.merge_progress(10..20);
        assert_eq!(v, vec![1..20]);
        v.merge_progress(30..40);
        assert_eq!(v, vec![1..20, 30..40]);
        v.merge_progress(21..40);
        assert_eq!(v, vec![1..20, 21..40]);
        v.merge_progress(19..21);
        assert_eq!(v, vec![1..40]);
        v.merge_progress(50..60);
        assert_eq!(v, vec![1..40, 50..60]);
        v.merge_progress(50..60);
        assert_eq!(v, vec![1..40, 50..60]);
        v.merge_progress(52..60);
        assert_eq!(v, vec![1..40, 50..60]);
        v.merge_progress(52..53);
        assert_eq!(v, vec![1..40, 50..60]);
        v.merge_progress(52..61);
        assert_eq!(v, vec![1..40, 50..61]);
        v.merge_progress(62..70);
        assert_eq!(v, vec![1..40, 50..61, 62..70]);
        v.merge_progress(40..62);
        assert_eq!(v, vec![1..70]);
        v.merge_progress(72..82);
        assert_eq!(v, vec![1..70, 72..82]);
        v.merge_progress(0..90);
        assert_eq!(v, vec![0..90]);
    }

    #[test]
    fn test_merge_empty_range_is_dropped() {
        // An empty range contained inside an existing entry must be a no-op.
        let mut v = vec![1..5, 10..20];
        v.merge_progress(3..3);
        assert_eq!(v, vec![1..5, 10..20]);

        // An empty range landing in a gap or at the end must NOT be inserted
        // as a degenerate entry.
        v.merge_progress(7..7);
        assert_eq!(v, vec![1..5, 10..20]);
        v.merge_progress(25..25);
        assert_eq!(v, vec![1..5, 10..20]);
    }

    #[test]
    fn test_merge_before_front_with_gap() {
        #![allow(clippy::single_range_in_vec_init)]
        // `new` sits entirely before `self[0]`, leaving a gap -> inserted at front.
        let mut v = vec![5..10];
        v.merge_progress(1..3);
        assert_eq!(v, vec![1..3, 5..10]);
    }

    #[test]
    fn test_merge_extends_before_first_and_spans_gaps() {
        #![allow(clippy::single_range_in_vec_init)]
        // `new` starts before the first entry and spans across multiple gaps,
        // absorbing every overlapping/touching entry into a single coalesced range.
        let mut v = vec![1..5, 8..10, 12..15];
        v.merge_progress(0..13);
        assert_eq!(v, vec![0..15]);
    }

    #[test]
    fn test_merge_reversed_range_is_dropped() {
        #![allow(clippy::single_range_in_vec_init)]
        // A reversed range (`start > end`) is invalid and must not enter the list.
        let mut v = vec![10..20];
        #[allow(clippy::reversed_empty_ranges)]
        v.merge_progress(30..5);
        assert_eq!(v, vec![10..20]);
    }

    #[test]
    fn test_merge_into_empty_vec() {
        #![allow(clippy::single_range_in_vec_init)]
        let mut v: Vec<ProgressEntry> = vec![];
        v.merge_progress(5..10);
        assert_eq!(v, vec![5..10]);
    }

    #[test]
    fn test_merge_superset_absorbs_all() {
        #![allow(clippy::single_range_in_vec_init)]
        let mut v = vec![1..5, 8..10, 20..30];
        v.merge_progress(0..40);
        assert_eq!(v, vec![0..40]);
    }

    #[test]
    fn test_merge_exact_duplicate_is_noop() {
        #![allow(clippy::single_range_in_vec_init)]
        let mut v = vec![1..5];
        v.merge_progress(1..5);
        assert_eq!(v, vec![1..5]);
    }

    #[test]
    fn test_merge_touching_right_extends() {
        #![allow(clippy::single_range_in_vec_init)]
        let mut v = vec![1..5];
        v.merge_progress(5..8);
        assert_eq!(v, vec![1..8]);
    }

    #[test]
    fn test_merge_touching_left_extends() {
        #![allow(clippy::single_range_in_vec_init)]
        let mut v = vec![5..10];
        v.merge_progress(1..5);
        assert_eq!(v, vec![1..10]);
    }

    /// Simulate concurrent, out-of-order chunk delivery where each chunk is
    /// *adjacent* (touching but not overlapping) the next — the worst case for
    /// `download_complete`'s `x.len() == 1` check in `overwrite.rs`. This must
    /// still coalesce into a single `[0..200]` entry, otherwise the download
    /// would be wrongly reported as incomplete and the `.part` never renamed.
    #[test]
    fn test_merge_out_of_order_adjacent_coalesces_to_single() {
        let mut v: Vec<ProgressEntry> = vec![];
        // 4 chunks of 50 bytes on a 200-byte file, arriving in a scrambled order.
        v.merge_progress(0..50);
        v.merge_progress(150..200);
        v.merge_progress(100..150);
        v.merge_progress(50..100);
        assert_eq!(v, vec![0..200]);

        // And the canonical "fully covered, single entry" invariant holds for
        // any interleaving that covers the whole span.
        let mut w: Vec<ProgressEntry> = vec![];
        for r in [0..70, 140..200, 70..140] {
            w.merge_progress(r);
        }
        assert_eq!(w, vec![0..200]);
        assert!(w.len() == 1 && w[0] == (0..200));
    }

    /// Independent reference implementation: sort, then sweep and coalesce.
    ///
    /// Used as an oracle to check that the incremental `merge_progress` always
    /// produces the same covered set as a batch merge, regardless of insertion
    /// order.
    fn reference_merge(ranges: &[ProgressEntry]) -> Vec<ProgressEntry> {
        let mut sorted: Vec<ProgressEntry> =
            ranges.iter().filter(|r| r.start < r.end).cloned().collect();
        if sorted.is_empty() {
            return vec![];
        }
        sorted.sort_by_key(|r| r.start);
        let mut merged: Vec<ProgressEntry> = vec![sorted[0].clone()];
        for r in sorted.iter().skip(1) {
            let last = merged.last_mut().unwrap();
            if r.start <= last.end {
                last.end = last.end.max(r.end); // overlapping or touching -> extend
            } else {
                merged.push(r.clone());
            }
        }
        merged
    }

    #[test]
    fn merge_agrees_with_reference_oracle() {
        // Whatever the insertion order, the incremental merge must equal the
        // covered set computed by the batch oracle.
        let cases: &[&[ProgressEntry]] = &[
            &[1..5, 8..10, 3..12],
            &[0..10, 20..30, 5..25],
            &[10..20, 0..5, 4..6, 19..21],
            &[0..50, 40..60, 55..70, 10..45],
            &[100..200, 0..50, 50..100],
            &[0..10, 30..40, 10..30], // last entry exactly fills the gap
        ];
        for ranges in cases {
            let mut v: Vec<ProgressEntry> = vec![];
            for r in *ranges {
                v.merge_progress(r.clone());
            }
            assert_eq!(v, reference_merge(ranges), "case {ranges:?}");
        }
    }

    #[test]
    fn merge_full_file_coalesces_to_single_regardless_of_order() {
        // 50 four-byte chunks of a 200-byte file, merged in a shuffled order,
        // must collapse into a single [0..200] entry. `download_complete` relies
        // on this: it tests the list length instead of the covered byte count.
        let chunks: Vec<ProgressEntry> = (0..50).map(|i| (i * 4)..(i * 4 + 4)).collect();
        let mut order: Vec<usize> = (0..50).collect();
        order.sort_by_key(|&i| (i % 7, i)); // deterministic shuffle
        let mut v: Vec<ProgressEntry> = vec![];
        for &i in &order {
            v.merge_progress(chunks[i].clone());
        }
        assert_eq!(v, vec![0..200]);
    }

    #[test]
    fn merge_new_fills_gap_and_coalesces_all_three() {
        #![allow(clippy::single_range_in_vec_init)]
        // A new entry that exactly fills the gap between two disjoint entries,
        // touching both ends, coalesces all three into one.
        let mut v = vec![0..10, 30..40];
        v.merge_progress(10..30);
        assert_eq!(v, vec![0..40]);
    }

    #[test]
    fn merge_accepts_touching_entries() {
        #![allow(clippy::single_range_in_vec_init)]
        // `merge_progress` itself never leaves touching entries behind, but a
        // hand-built list containing them still satisfies the "sorted and
        // non-overlapping" precondition and merges correctly.
        let mut v = vec![0..10, 10..20];
        v.merge_progress(5..15);
        assert_eq!(v, vec![0..20]);
    }
}
