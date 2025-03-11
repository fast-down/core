use std::fmt::Display;

#[derive(Debug, Clone)]
/// 这是一个闭区间 `[start, end]`，表示下载进度
pub struct DownloadProgress {
    pub start: usize,
    pub end: usize,
}

impl PartialEq for DownloadProgress {
    fn eq(&self, other: &Self) -> bool {
        self.start == other.start && self.end == other.end
    }
}

impl Display for DownloadProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", self.start, self.end)
    }
}

impl DownloadProgress {
    pub fn new(start: usize, end: usize) -> Self {
        if end < start {
            panic!("end < start");
        }
        Self { start, end }
    }

    pub fn can_merge(&self, b: &Self) -> bool {
        self.start <= b.end + 1 && b.start <= self.end + 1
    }

    pub fn size(&self) -> usize {
        self.end - self.start + 1
    }

    pub fn split_at(&self, pos: usize) -> (Self, Self) {
        if pos < self.start || pos >= self.end {
            panic!("pos out of range");
        }
        (Self::new(self.start, pos), Self::new(pos + 1, self.end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let progress = DownloadProgress::new(0, 100);
        assert_eq!(progress.start, 0);
        assert_eq!(progress.end, 100);

        let progress = DownloadProgress::new(0, 0);
        assert_eq!(progress.start, 0);
        assert_eq!(progress.end, 0);
    }

    #[test]
    #[should_panic = "end < start"]
    fn test_new_error() {
        DownloadProgress::new(10, 9);
    }

    #[test]
    fn test_size() {
        let progress = DownloadProgress::new(1, 100);
        assert_eq!(progress.size(), 100);
    }

    #[test]
    fn test_split_at() {
        let progress = DownloadProgress::new(10, 100);
        let (first, second) = progress.split_at(50);

        assert_eq!(first.start, 10);
        assert_eq!(first.end, 50);
        assert_eq!(second.start, 51);
        assert_eq!(second.end, 100);
    }

    #[test]
    fn test_can_merge_adjacent() {
        let a = DownloadProgress::new(0, 5);
        let b = DownloadProgress::new(6, 10);
        assert!(a.can_merge(&b));
        assert!(b.can_merge(&a));
    }

    #[test]
    fn test_can_merge_overlapping() {
        let a = DownloadProgress::new(0, 5);
        let b = DownloadProgress::new(4, 10);
        assert!(a.can_merge(&b));
        assert!(b.can_merge(&a));
    }

    #[test]
    fn test_cannot_merge_non_adjacent_non_overlapping() {
        let a = DownloadProgress::new(0, 5);
        let b = DownloadProgress::new(7, 10);
        assert!(!a.can_merge(&b));
        assert!(!b.can_merge(&a));
    }

    #[test]
    fn test_can_merge_same_range() {
        let a = DownloadProgress::new(0, 5);
        let b = DownloadProgress::new(0, 5);
        assert!(a.can_merge(&b));
        assert!(b.can_merge(&a));
    }

    #[test]
    fn test_can_merge_start_end_overlap() {
        let a = DownloadProgress::new(0, 5);
        let b = DownloadProgress::new(5, 10);
        assert!(a.can_merge(&b));
        assert!(b.can_merge(&a));
    }

    #[test]
    fn test_can_merge_sub() {
        let a = DownloadProgress::new(5, 10);
        let b = DownloadProgress::new(0, 20);
        assert!(a.can_merge(&b));
        assert!(b.can_merge(&a));

        let a = DownloadProgress::new(5, 10);
        let b = DownloadProgress::new(0, 10);
        assert!(a.can_merge(&b));
        assert!(b.can_merge(&a));

        let a = DownloadProgress::new(5, 10);
        let b = DownloadProgress::new(5, 20);
        assert!(a.can_merge(&b));
        assert!(b.can_merge(&a));
    }

    #[test]
    fn test_cannot_merge_disjoint() {
        let a = DownloadProgress::new(0, 5);
        let b = DownloadProgress::new(10, 15);
        assert!(!a.can_merge(&b));
        assert!(!b.can_merge(&a));
    }
}
