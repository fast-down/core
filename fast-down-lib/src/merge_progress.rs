use crate::progress::{ProgresTrait, Progress};
extern crate alloc;
use alloc::vec::Vec;

pub trait MergeProgress {
    fn merge_progress(&mut self, new: Progress);
}

impl MergeProgress for Vec<Progress> {
    fn merge_progress(&mut self, new: Progress) {
        let i = self.partition_point(|old| old.start < new.start);
        if i == self.len() {
            match self.last_mut() {
                Some(last) if last.end >= new.start => {
                    last.end = new.end;
                }
                _ => self.push(new),
            }
        } else {
            let u1 = if i == 0 {
                false
            } else {
                self[i - 1].can_merge(&new)
            };
            let u2 = self[i].can_merge(&new);
            if u1 && u2 {
                self[i - 1].end = self[i].end;
                self.remove(i);
            } else if u1 {
                self[i - 1].end = new.end;
            } else if u2 {
                self[i].start = new.start;
            } else {
                self.insert(i, new);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::Progress;
    use alloc::vec;

    #[test]
    fn test_merge_into_empty_vec() {
        let mut v: Vec<Progress> = Vec::new();
        let new = 10..20;
        v.merge_progress(new.clone());
        assert_eq!(v, vec![new]);
    }

    #[test]
    fn test_append_non_overlapping() {
        let mut v = vec![1..5];
        v.merge_progress(6..10);
        assert_eq!(v, vec![1..5, 6..10]);
    }

    #[test]
    fn test_prepend_non_overlapping() {
        let mut v = vec![6..10];
        v.merge_progress(1..5);
        assert_eq!(v, vec![1..5, 6..10]);
    }

    #[test]
    fn test_merge_with_last() {
        let mut v = vec![1..5];
        v.merge_progress(5..10);
        assert_eq!(v, vec![1..10]);
    }

    #[test]
    fn test_merge_between_two() {
        let mut v = vec![1..5, 10..15];
        v.merge_progress(5..10);
        assert_eq!(v, vec![1..15]);
    }

    #[test]
    fn test_merge_with_previous_only() {
        let mut v = vec![1..5, 10..15];
        v.merge_progress(5..8);
        assert_eq!(v, vec![1..8, 10..15]);
    }

    #[test]
    fn test_merge_with_next_only() {
        let mut v = vec![1..5, 10..15];
        v.merge_progress(8..10);
        assert_eq!(v, vec![1..5, 8..15]);
    }

    #[test]
    fn test_insert_between_two() {
        let mut v = vec![1..5, 10..15];
        v.merge_progress(6..8);
        assert_eq!(v, vec![1..5, 6..8, 10..15]);
    }
}
