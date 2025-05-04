use fast_down::Progress;

pub fn reverse_progress(progress: &[Progress], total_size: usize) -> Vec<Progress> {
    if progress.is_empty() {
        return vec![0..total_size];
    }
    let mut result = Vec::with_capacity(progress.len());
    let mut prev_end = 0;
    for range in progress {
        if range.start > prev_end {
            result.push(prev_end..range.start);
        }
        prev_end = range.end;
    }
    if prev_end < total_size {
        result.push(prev_end..total_size);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reverse_progress() {
        assert_eq!(reverse_progress(&vec![], 10), vec![0..10]);
        assert_eq!(reverse_progress(&vec![0..5], 10), vec![5..10]);
        assert_eq!(reverse_progress(&vec![5..10], 10), vec![0..5]);
        assert_eq!(reverse_progress(&vec![0..5, 7..10], 10), vec![5..7]);
        assert_eq!(reverse_progress(&vec![0..3, 5..8], 10), vec![3..5, 8..10]);
        assert_eq!(
            reverse_progress(&vec![1..3, 5..8], 10),
            vec![0..1, 3..5, 8..10]
        );
    }
}
