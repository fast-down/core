use core::ops::Range;
use core::cmp::min;

pub fn overlaps(range: Range<u64>, block_size: u64, count: u64) -> impl Iterator<Item = u64> {
    if block_size == 0 || count == 0 || range.start >= range.end {
        return 1..=0;
    }

    let first_overlapping_block = range.start / block_size;

    let last_overlapping_block_by_range = (range.end.saturating_sub(1)) / block_size;

    let last_valid_block_by_count = (count.saturating_sub(1)) / block_size;

    let end_block = min(last_overlapping_block_by_range, last_valid_block_by_count);

    first_overlapping_block..=end_block
}

#[test]
fn test_overlaps() {
    assert_eq!(
        overlaps(0..20, 10, 30).collect::<Vec<_>>(),
        vec![0, 1]
    );

    assert_eq!(
        overlaps(0..30, 10, 50).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );

    assert_eq!(
        overlaps(10..30, 10, 50).collect::<Vec<_>>(),
        vec![1, 2]
    );
}