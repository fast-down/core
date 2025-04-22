use core::cmp::min;
use core::ops::Range;
use core::ops::{Div, Sub};
use core::ops::RangeInclusive;
use num_traits::{One, SaturatingSub, Zero, };

pub fn overlaps<T: Clone + Zero + One + Ord + Div<Output = T> + Sub + SaturatingSub>(
    range: Range<T>,
    block_size: T,
    count: T,
) -> impl Iterator<Item = T> where RangeInclusive<T> : Iterator<Item = T> {
    if block_size == T::zero() || count == T::zero() || range.start >= range.end {
        return T::one()..=T::zero();
    }

    let first_overlapping_block = range.start / block_size.clone();

    let last_overlapping_block_by_range =
        (range.end.saturating_sub(&T::one())) / block_size.clone();

    let last_valid_block_by_count = (count.saturating_sub(&T::one())) / block_size;

    let end_block = min(last_overlapping_block_by_range, last_valid_block_by_count);

    first_overlapping_block..=end_block
}

#[test]
fn test_overlaps() {
    assert_eq!(overlaps(0..20, 10, 30).collect::<Vec<_>>(), vec![0, 1]);

    assert_eq!(overlaps(0..30, 10, 50).collect::<Vec<_>>(), vec![0, 1, 2]);

    assert_eq!(overlaps(10..30, 10, 50).collect::<Vec<_>>(), vec![1, 2]);
}
