use crate::task::Task;

pub trait SplitTask {
    fn split_task(&self, n: u64) -> impl Iterator<Item = Task>;
    fn split_two(&self) -> (u64, u64);
}

impl SplitTask for Task {
    fn split_task(&self, n: u64) -> impl Iterator<Item = Task> {
        debug_assert!(n > 0, "n must be greater than 0");
        let total = self.remain();
        let offset = self.start();
        let per_group = total / n;
        let remainder = total % n;
        (0..n).map(move |i| {
            let start = offset + i * per_group + i.min(remainder);
            let end = start + per_group + if i < remainder { 1 } else { 0 };
            Task::new(start, end)
        })
    }

    fn split_two(&self) -> (u64, u64) {
        let start = self.start();
        let end = self.end();
        let mid = (start + end) / 2;
        self.set_end(mid);
        (mid, end)
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    #[test]
    fn test_split_task() {
        let task = Task::new(1, 6); // 1, 2, 3, 4, 5
        let groups: Vec<_> = task.split_task(3).collect(); // 5 / 3 = 1 remainder 2

        assert_eq!(
            groups,
            vec![Task::new(1, 3), Task::new(3, 5), Task::new(5, 6)]
        );
    }

    #[test]
    fn test_split_two() {
        let task = Task::new(1, 6); // 1, 2, 3, 4, 5
        let (mid, end) = task.split_two();

        assert_eq!(task.start(), 1);
        assert_eq!(task.end(), 3);
        assert_eq!(mid, 3);
        assert_eq!(end, 6);
    }
}
