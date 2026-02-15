extern crate alloc;
use alloc::sync::{Arc, Weak};
use core::{fmt, ops::Range, sync::atomic::Ordering};
use portable_atomic::AtomicU128;

#[derive(Debug, Clone)]
pub struct Task {
    pub state: Arc<AtomicU128>,
}
#[derive(Debug, Clone)]
pub struct WeakTask {
    pub state: Weak<AtomicU128>,
}

impl WeakTask {
    pub fn upgrade(&self) -> Option<Task> {
        self.state.upgrade().map(|state| Task { state })
    }
    pub fn strong_count(&self) -> usize {
        self.state.strong_count()
    }
    pub fn weak_count(&self) -> usize {
        self.state.weak_count()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeError;

impl fmt::Display for RangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Range invariant violated: start > end or overflow")
    }
}

impl Task {
    #[inline(always)]
    fn pack(range: Range<u64>) -> u128 {
        ((range.start as u128) << 64) | (range.end as u128)
    }
    #[inline(always)]
    fn unpack(state: u128) -> Range<u64> {
        (state >> 64) as u64..state as u64
    }

    pub fn new(range: Range<u64>) -> Self {
        assert!(range.start <= range.end);
        Self {
            state: Arc::new(AtomicU128::new(Self::pack(range))),
        }
    }
    pub fn get(&self) -> Range<u64> {
        let state = self.state.load(Ordering::Acquire);
        Self::unpack(state)
    }
    pub fn set(&self, range: Range<u64>) {
        assert!(range.start <= range.end);
        self.state.store(Self::pack(range), Ordering::Release);
    }
    pub fn start(&self) -> u64 {
        (self.state.load(Ordering::Acquire) >> 64) as u64
    }
    /// 当 start + bias <= old_start 时返回 RangeError
    /// 否则返回 old_start..new_start.min(end)
    pub fn safe_add_start(&self, start: u64, bias: u64) -> Result<Range<u64>, RangeError> {
        let new_start = start.checked_add(bias).ok_or(RangeError)?;
        let mut old_state = self.state.load(Ordering::Acquire);
        loop {
            let mut range = Self::unpack(old_state);
            let new_start = new_start.min(range.end);
            if new_start <= range.start {
                break Err(RangeError);
            }
            let span = range.start..new_start;
            range.start = new_start;
            let new_state = Self::pack(range);
            match self.state.compare_exchange_weak(
                old_state,
                new_state,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break Ok(span),
                Err(x) => old_state = x,
            }
        }
    }
    // /// 1. 当 start + value 溢出时返回 RangeError
    //
    // pub fn fetch_add_start(&self, value: u64) -> Result<u64, RangeError> {
    //     let mut old_state = self.state.load(Ordering::Acquire);
    //     loop {
    //         let mut range = Self::unpack(old_state);
    //         let old_start = range.start;
    //         range.start = range.start.checked_add(value).ok_or(RangeError)?;
    //         if range.start > range.end {
    //             break Err(RangeError);
    //         }
    //         let new_state = Self::pack(range);
    //         match self.state.compare_exchange_weak(
    //             old_state,
    //             new_state,
    //             Ordering::AcqRel,
    //             Ordering::Acquire,
    //         ) {
    //             Ok(_) => break Ok(old_start),
    //             Err(x) => old_state = x,
    //         }
    //     }
    // }
    pub fn end(&self) -> u64 {
        self.state.load(Ordering::Acquire) as u64
    }
    pub fn remain(&self) -> u64 {
        let range = self.get();
        range.end.saturating_sub(range.start)
    }
    /// 1. 当 start > end 时返回 RangeError
    /// 2. 当 remain < 2 时返回 None 并且不会修改自己
    pub fn split_two(&self) -> Result<Option<Range<u64>>, RangeError> {
        let mut old_state = self.state.load(Ordering::Acquire);
        loop {
            let range = Self::unpack(old_state);
            if range.start > range.end {
                return Err(RangeError);
            }
            let mid = range.start + (range.end - range.start) / 2;
            if mid == range.start {
                return Ok(None);
            }
            let new_state = Self::pack(range.start..mid);
            match self.state.compare_exchange_weak(
                old_state,
                new_state,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(Some(mid..range.end)),
                Err(x) => old_state = x,
            }
        }
    }
    pub fn take(&self) -> Option<Range<u64>> {
        let mut old_state = self.state.load(Ordering::Acquire);
        loop {
            let range = Self::unpack(old_state);
            if range.start == range.end {
                return None;
            }
            let new_state = Self::pack(range.start..range.start);
            match self.state.compare_exchange_weak(
                old_state,
                new_state,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(range),
                Err(x) => old_state = x,
            }
        }
    }
    pub fn downgrade(&self) -> WeakTask {
        WeakTask {
            state: Arc::downgrade(&self.state),
        }
    }
}
impl From<&Range<u64>> for Task {
    fn from(value: &Range<u64>) -> Self {
        Self::new(value.clone())
    }
}

impl PartialEq for Task {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }
}
impl Eq for Task {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_task() {
        let task = Task::new(10..20);
        assert_eq!(task.start(), 10);
        assert_eq!(task.end(), 20);
        assert_eq!(task.remain(), 10);
    }

    #[test]
    fn test_remain() {
        let task = Task::new(10..25);
        assert_eq!(task.remain(), 15);
    }

    #[test]
    fn test_split_two() {
        let task = Task::new(1..6); // 1, 2, 3, 4, 5
        let range = task.split_two().unwrap().unwrap();
        assert_eq!(task.start(), 1);
        assert_eq!(task.end(), 3);
        assert_eq!(range.start, 3);
        assert_eq!(range.end, 6);
    }

    #[test]
    fn test_split_empty() {
        let task = Task::new(1..1);
        let range = task.split_two().unwrap();
        assert_eq!(task.start(), 1);
        assert_eq!(task.end(), 1);
        assert_eq!(range, None);
    }

    #[test]
    fn test_split_one() {
        let task = Task::new(1..2);
        let range = task.split_two().unwrap();
        assert_eq!(task.start(), 1);
        assert_eq!(task.end(), 2);
        assert_eq!(range, None);
    }
}
