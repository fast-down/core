use crate::Task;
use crate::split_task::SplitTask;
use core::borrow::Borrow;

pub trait StealTask {
    fn steal<T: Borrow<Self>>(&self, tasks: &[T], min_chunk_size: u64) -> bool;
}

impl StealTask for Task {
    fn steal<T: Borrow<Self>>(&self, tasks: &[T], min_chunk_size: u64) -> bool {
        debug_assert!(min_chunk_size > 1, "min_chunk_size must be greater than 1");
        let (max_pos, max_remain) = tasks
            .iter()
            .enumerate()
            .map(|(i, w)| (i, w.borrow().remain()))
            .max_by_key(|(_, remain)| *remain)
            .unwrap_or((0, 0));
        if max_remain < min_chunk_size {
            return false;
        }
        let (start, end) = tasks[max_pos].borrow().split_two();
        self.set_end(end);
        self.set_start(start);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;
    use alloc::vec;
    use alloc::vec::Vec;

    #[test]
    fn test_steal_no_tasks() {
        let thief = Task::new(0, 0);
        let tasks: Vec<Task> = vec![];
        assert!(!thief.steal(&tasks, 10));
    }

    #[test]
    fn test_steal_all_too_small() {
        let thief = Task::new(0, 0);
        let tasks = vec![
            Task::new(0, 5),  // remain = 5
            Task::new(8, 10), // remain = 2
        ];
        assert!(!thief.steal(&tasks, 10));
    }

    #[test]
    fn test_steal_successful() {
        let thief = Task::new(0, 0);
        let tasks = vec![
            Task::new(0, 5),   // remain = 5
            Task::new(10, 25), // remain = 15 (will be stolen from)
            Task::new(30, 40), // remain = 10
        ];

        assert!(thief.steal(&tasks, 10));

        assert_eq!(thief.start(), 17);
        assert_eq!(thief.end(), 25);

        assert_eq!(tasks[1].start(), 10);
        assert_eq!(tasks[1].end(), 17);
    }

    #[test]
    fn test_steal_exact_min_size() {
        let thief = Task::new(0, 0);
        let tasks = vec![
            Task::new(0, 10), // remain = 10
        ];

        assert!(thief.steal(&tasks, 10));

        assert_eq!(thief.start(), 5);
        assert_eq!(thief.end(), 10);

        assert_eq!(tasks[0].start(), 0);
        assert_eq!(tasks[0].end(), 5);
    }
}
