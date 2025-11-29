use core::ops::Range;

pub type ProgressEntry = Range<u64>;

pub trait Mergeable {
    fn can_merge(&self, other: &Self) -> bool;
}

impl Mergeable for ProgressEntry {
    #[inline(always)]
    fn can_merge(&self, b: &Self) -> bool {
        self.start == b.end || b.start == self.end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_can_merge_adjacent() {
        let a = 0..5;
        let b = 5..10;
        assert!(a.can_merge(&b));
        assert!(b.can_merge(&a));
    }

    #[test]
    fn test_cannot_merge_non_adjacent_non_overlapping() {
        let a = 0..5;
        let b = 7..10;
        assert!(!a.can_merge(&b));
        assert!(!b.can_merge(&a));
    }

    #[test]
    fn test_cannot_merge_disjoint() {
        let a = 0..5;
        let b = 6..15;
        assert!(!a.can_merge(&b));
        assert!(!b.can_merge(&a));
    }
}
