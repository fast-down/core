use fast_down::Progress;
use std::cmp::{max, min};

pub trait ProgressOverlap {
    fn overlap(&self, other: &Self) -> u64;
}

impl ProgressOverlap for Progress {
    fn overlap(&self, other: &Self) -> u64 {
        let overlap_start = max(self.start, other.start);
        let overlap_end = min(self.end, other.end);
        overlap_end.checked_sub(overlap_start).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_overlap() {
        assert_eq!((0..5u64).overlap(&(0..20)), 5);
        assert_eq!((0..5u64).overlap(&(10..15)), 0);
        assert_eq!((10..15u64).overlap(&(0..5)), 0);
        assert_eq!((0..10u64).overlap(&(5..15)), 5);
        assert_eq!((5..15u64).overlap(&(0..10)), 5);
        assert_eq!((0..20u64).overlap(&(5..10)), 5);
        assert_eq!((5..10u64).overlap(&(0..20)), 5);
        assert_eq!((0..10u64).overlap(&(0..10)), 10);
        assert_eq!((0..5u64).overlap(&(5..10)), 0);
        assert_eq!((5..10u64).overlap(&(0..5)), 0);
        assert_eq!((0..0u64).overlap(&(0..5)), 0);
        assert_eq!((0..5u64).overlap(&(0..0)), 0);
        assert_eq!((5..5u64).overlap(&(0..10)), 0);
        assert_eq!((0..10u64).overlap(&(5..5)), 0);
        assert_eq!((0..0u64).overlap(&(0..0)), 0);
        assert_eq!((5..5u64).overlap(&(5..5)), 0);
        assert_eq!((0..0u64).overlap(&(5..5)), 0);
        assert_eq!((0..3u64).overlap(&(1..2)), 1);
        assert_eq!((1..2u64).overlap(&(0..3)), 1);
    }
}
