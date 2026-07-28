//! Merging of [`ProgressEntry`](crate::ProgressEntry) ranges into a sorted list.

use crate::ProgressEntry;

/// Trait for merging a new [`ProgressEntry`] into a sorted list of existing entries.
///
/// Used to consolidate downloaded ranges and remove redundant gaps.
pub trait Merge {
    /// Merge `new` into the existing (sorted) progress list, coalescing overlaps
    /// so the list stays sorted and gap-free where ranges touch.
    fn merge_progress(&mut self, new: ProgressEntry);
}

impl Merge for Vec<ProgressEntry> {
    fn merge_progress(&mut self, new: ProgressEntry) {
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
}
