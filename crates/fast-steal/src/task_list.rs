extern crate alloc;
use alloc::{sync::Arc, vec::Vec};
use core::ops::Range;

#[derive(Debug)]
pub struct TaskList {
    tasks: Arc<[Range<u64>]>,
    start_point: Arc<[u64]>,
    pub len: u64,
}

impl From<&[Range<u64>]> for TaskList {
    fn from(tasks: &[Range<u64>]) -> Self {
        let mut len = 0;
        let mut start_point = Vec::with_capacity(tasks.len());
        for range in tasks {
            start_point.push(len);
            len += range.end - range.start;
        }
        Self {
            tasks: tasks.into(),
            start_point: start_point.into(),
            len,
        }
    }
}

impl TaskList {
    #[inline(always)]
    pub fn position(&self, index: u64) -> usize {
        self.start_point.partition_point(|&x| x <= index) - 1
    }

    #[inline]
    pub fn get(&self, index: u64) -> u64 {
        let point = self.position(index);
        self.tasks[point].start + index - self.start_point[point]
    }

    pub fn get_range(&self, range: Range<u64>) -> Vec<Range<u64>> {
        if range.is_empty() {
            return Vec::new();
        }

        let start_seg = self.position(range.start);
        let end_seg = self.position(range.end - 1);
        let tasks_len = self.tasks.len();
        let mut result = Vec::with_capacity(end_seg - start_seg + 1);

        for seg in start_seg..=end_seg {
            // 获取当前段的全局索引范围
            let seg_start = self.start_point[seg];
            let seg_end = if seg + 1 < tasks_len {
                self.start_point[seg + 1]
            } else {
                self.len
            };

            // 计算当前段在请求范围内的实际截取部分
            let curr_start = seg_start.max(range.start);
            let curr_end = seg_end.min(range.end);

            if curr_start < curr_end {
                // 转换为实际数值范围
                let actual_start = self.tasks[seg].start + (curr_start - seg_start);
                let actual_end = self.tasks[seg].start + (curr_end - seg_start);
                result.push(actual_start..actual_end);
            }
        }

        result
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::single_range_in_vec_init)]
    use super::*;

    #[test]
    fn test_empty_list() {
        let tasks = TaskList::from(&[][..]);
        assert_eq!(tasks.len, 0);
    }

    #[test]
    fn test_single_range() {
        let tasks = TaskList::from(&[10..15][..]);
        assert_eq!(tasks.len, 5);

        assert_eq!(tasks.get(0), 10);
        assert_eq!(tasks.get(1), 11);
        assert_eq!(tasks.get(4), 14);
    }

    #[test]
    fn test_multiple_ranges() {
        let tasks = TaskList::from(&[10..12, 20..25, 30..35][..]);
        assert_eq!(tasks.len, (12 - 10) + (25 - 20) + (35 - 30));

        // First range
        assert_eq!(tasks.get(0), 10);
        assert_eq!(tasks.get(1), 11);

        // Second range
        assert_eq!(tasks.get(2), 20);
        assert_eq!(tasks.get(3), 21);
        assert_eq!(tasks.get(6), 24);

        // Third range
        assert_eq!(tasks.get(7), 30);
        assert_eq!(tasks.get(11), 34);
    }

    #[test]
    fn test_consecutive_ranges() {
        let tasks = TaskList::from(&[10..15, 15..20][..]);
        assert_eq!(tasks.len, 10);

        assert_eq!(tasks.get(0), 10);
        assert_eq!(tasks.get(4), 14);
        assert_eq!(tasks.get(5), 15);
        assert_eq!(tasks.get(9), 19);
    }

    #[test]
    fn test_non_contiguous_ranges() {
        let tasks = TaskList::from(&[100..101, 200..203, 300..305][..]);
        assert_eq!(tasks.len, 1 + 3 + 5);

        assert_eq!(tasks.get(0), 100);
        assert_eq!(tasks.get(1), 200);
        assert_eq!(tasks.get(2), 201);
        assert_eq!(tasks.get(4), 300);
        assert_eq!(tasks.get(8), 304);
    }

    #[test]
    fn test_zero_length_ranges() {
        let tasks = TaskList::from(&[10..10, 20..20, 30..35][..]);
        assert_eq!(tasks.len, 5);

        assert_eq!(tasks.get(0), 30);
        assert_eq!(tasks.get(4), 34);
    }

    #[test]
    fn test_get_range() {
        let tasks = TaskList::from(&[10..15, 20..25][..]);

        // 单任务段范围
        assert_eq!(tasks.get_range(0..5), &[10..15][..]);
        // 跨任务段范围
        assert_eq!(tasks.get_range(3..7), &[13..15, 20..22][..]);
        // 空范围
        assert_eq!(tasks.get_range(5..5), &[][..]);
        // 边界测试
        assert_eq!(tasks.get_range(4..5), &[14..15][..]);
        assert_eq!(tasks.get_range(4..6), &[14..15, 20..21][..]);
        assert_eq!(tasks.get_range(5..7), &[20..22][..]);
        // 完整覆盖多个段
        assert_eq!(tasks.get_range(0..10), &[10..15, 20..25][..]);
    }

    #[test]
    fn test_get_range2() {
        let tasks = TaskList::from(&[20..25, 10..15][..]);

        // 单任务段范围
        assert_eq!(tasks.get_range(0..5), &[20..25][..]);
        // 跨任务段范围
        assert_eq!(tasks.get_range(3..7), &[23..25, 10..12][..]);
        // 空范围
        assert_eq!(tasks.get_range(5..5), &[][..]);
        // 边界测试
        assert_eq!(tasks.get_range(4..5), &[24..25][..]);
        assert_eq!(tasks.get_range(4..6), &[24..25, 10..11][..]);
        assert_eq!(tasks.get_range(5..7), &[10..12][..]);
        // 完整覆盖多个段
        assert_eq!(tasks.get_range(0..10), &[20..25, 10..15][..]);
    }
}
