use crate::task_list::TaskList;
use core::{
    ops::Range,
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Debug)]
pub struct Task {
    start: AtomicU64,
    end: AtomicU64,
}

impl Task {
    pub fn remain(&self) -> u64 {
        let start = self.start();
        let end = self.end();
        end.saturating_sub(start)
    }

    pub fn start(&self) -> u64 {
        self.start.load(Ordering::Acquire)
    }
    pub fn set_start(&self, start: u64) {
        self.start.store(start, Ordering::Release);
    }
    pub fn fetch_add_start(&self, value: u64) -> u64 {
        self.start.fetch_add(value, Ordering::AcqRel)
    }
    pub fn fetch_sub_start(&self, value: u64) -> u64 {
        self.start.fetch_sub(value, Ordering::AcqRel)
    }
    pub fn end(&self) -> u64 {
        self.end.load(Ordering::Acquire)
    }
    pub fn set_end(&self, end: u64) {
        self.end.store(end, Ordering::Release);
    }
    pub fn fetch_add_end(&self, value: u64) -> u64 {
        self.end.fetch_add(value, Ordering::AcqRel)
    }
    pub fn fetch_sub_end(&self, value: u64) -> u64 {
        self.end.fetch_sub(value, Ordering::AcqRel)
    }

    pub fn new(start: u64, end: u64) -> Self {
        Self {
            start: AtomicU64::new(start),
            end: AtomicU64::new(end),
        }
    }
}

impl PartialEq for Task {
    fn eq(&self, other: &Self) -> bool {
        self.start() == other.start() && self.end() == other.end()
    }
}

impl From<&(u64, u64)> for Task {
    fn from(value: &(u64, u64)) -> Self {
        Self::new(value.0, value.1)
    }
}

impl From<&Range<u64>> for Task {
    fn from(value: &Range<u64>) -> Self {
        Self::new(value.start, value.end)
    }
}

impl From<&TaskList> for Task {
    fn from(value: &TaskList) -> Self {
        Self::new(0, value.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_task() {
        let task = Task::new(10, 20);
        assert_eq!(task.start(), 10);
        assert_eq!(task.end(), 20);
        assert_eq!(task.remain(), 10);
    }

    #[test]
    fn test_from_tuple() {
        let task: Task = (&(5, 15)).into();
        assert_eq!(task.start(), 5);
        assert_eq!(task.end(), 15);
    }

    #[test]
    fn test_from_range() {
        let range = 3..8;
        let task: Task = (&range).into();
        assert_eq!(task.start(), range.start);
        assert_eq!(task.end(), range.end);
    }

    #[test]
    fn test_setters() {
        let task = Task::new(0, 0);
        task.set_start(7);
        task.set_end(14);
        assert_eq!(task.start(), 7);
        assert_eq!(task.end(), 14);
    }

    #[test]
    fn test_remain() {
        let task = Task::new(10, 25);
        assert_eq!(task.remain(), 15);

        task.set_start(20);
        assert_eq!(task.remain(), 5);
    }

    #[test]
    fn test_partial_eq() {
        let task1 = Task::new(1, 10);
        let task2 = Task::new(1, 10);
        let task3 = Task::new(2, 10);
        let task4 = Task::new(1, 11);

        assert_eq!(task1, task2);
        assert_ne!(task1, task3);
        assert_ne!(task1, task4);
    }

    #[test]
    fn test_from_task_list() {
        let task_list = TaskList::from(&[10..42, 80..84][..]);
        let task: Task = (&task_list).into();
        assert_eq!(task.start(), 0);
        assert_eq!(task.end(), 36);
    }
}
