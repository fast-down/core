use crate::ProgressEntry;

/// Iterator that yields the *gaps* (non-downloaded ranges) from a list of [`ProgressEntry`]s.
///
/// Entries shorter than `window` are merged into adjacent gaps to reduce fragmentation.
#[derive(Debug)]
pub struct InvertIter<I: Iterator<Item = ProgressEntry>> {
    iter: I,
    prev_end: u64,
    total_size: u64,
    window: u64,
}

impl<I> Iterator for InvertIter<I>
where
    I: Iterator<Item = ProgressEntry>,
{
    type Item = ProgressEntry;
    fn next(&mut self) -> Option<Self::Item> {
        let mut gap_start = self.prev_end;
        for range in self.iter.by_ref() {
            if range.start == gap_start {
                gap_start = range.end;
                continue;
            }
            let len = range.end - range.start;
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
}
