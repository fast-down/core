//! Cancellable, lock-free units of work.
//!
//! A [`Task`] stores its remaining work as a single atomic `u128` packing the
//! `start` (high 64 bits) and `end` (low 64 bits) bounds, which lets multiple
//! worker threads advance the same task without locking. Because the crate is
//! `no_std`, only `alloc` is required for the reference-counted state.

extern crate alloc;
use alloc::sync::{Arc, Weak};
use core::{fmt, ops::Range, sync::atomic::Ordering};
use portable_atomic::AtomicU128;

/// A cancellable, concurrent-safe task that tracks a `start..end` range of work.
///
/// The range is stored as a single atomic `u128`, allowing lock-free reads and
/// fine-grained progress updates. Multiple workers can safely steal sub-ranges
/// from the same task via [`split_two`](Task::split_two).
///
/// Two `Task`s are equal iff they point to the same underlying state (see the
/// `PartialEq` impl, which uses `Arc::ptr_eq`).
#[derive(Debug, Clone)]
pub struct Task {
    /// Atomic state packing `start` (high 64 bits) and `end` (low 64 bits).
    ///
    /// Prefer the safe accessors ([`Task::start`], [`Task::end`], [`Task::get`],
    /// [`Task::safe_add_start`]); this field is exposed for advanced use.
    pub state: Arc<AtomicU128>,
}
/// A weak reference to a [`Task`], obtained via [`Task::downgrade`].
///
/// Does not prevent the task from being deallocated. Use [`upgrade`](WeakTask::upgrade)
/// to attempt to obtain a strong [`Task`] reference.
#[derive(Debug, Clone)]
pub struct WeakTask {
    /// Weak reference to the underlying atomic state of the originating [`Task`].
    pub state: Weak<AtomicU128>,
}

impl WeakTask {
    /// Attempts to upgrade to a strong [`Task`].
    ///
    /// Returns `None` if all strong references to the underlying state have already
    /// been dropped.
    #[must_use]
    pub fn upgrade(&self) -> Option<Task> {
        self.state.upgrade().map(|state| Task { state })
    }
    /// Returns the number of strong [`Task`] references to the underlying state.
    #[must_use]
    pub fn strong_count(&self) -> usize {
        self.state.strong_count()
    }
    /// Returns the number of weak [`WeakTask`] references to the underlying state.
    #[must_use]
    pub fn weak_count(&self) -> usize {
        self.state.weak_count()
    }
}

/// Error returned when a task range invariant is violated (`start > end` or overflow).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeError;

impl fmt::Display for RangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Range invariant violated: start > end or overflow")
    }
}

impl core::error::Error for RangeError {}

impl Task {
    #[allow(clippy::inline_always)]
    #[inline(always)]
    const fn pack(range: Range<u64>) -> u128 {
        ((range.start as u128) << 64) | (range.end as u128)
    }
    #[allow(clippy::inline_always)]
    #[inline(always)]
    const fn unpack(state: u128) -> Range<u64> {
        #[allow(clippy::cast_possible_truncation)]
        let end = state as u64;
        (state >> 64) as u64..end
    }

    /// # Panics
    /// Panics when `range.start > range.end`
    #[must_use]
    pub fn new(range: Range<u64>) -> Self {
        assert!(range.start <= range.end);
        Self {
            state: Arc::new(AtomicU128::new(Self::pack(range))),
        }
    }
    /// Returns the current `start..end` range, loaded atomically with `Acquire`
    /// ordering.
    #[must_use]
    pub fn get(&self) -> Range<u64> {
        let state = self.state.load(Ordering::Acquire);
        Self::unpack(state)
    }
    /// Returns the current start of the work range (the high 64 bits of the atomic
    /// state).
    #[must_use]
    pub fn start(&self) -> u64 {
        (self.state.load(Ordering::Acquire) >> 64) as u64
    }
    /// Atomically advances `start` to `min(start + bias, end)`, but only if that
    /// makes forward progress; then returns the slice that was claimed.
    ///
    /// # Errors
    /// Returns [`RangeError`] when `start + bias` would not exceed the current
    /// `start` (no progress, a non-positive bias, or `u64` overflow), or when the
    /// task range invariant `start <= end` is violated. On success returns the
    /// claimed `old_start..new_start` sub-range.
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
    /// Returns the current end of the work range (the low 64 bits of the atomic
    /// state).
    #[must_use]
    pub fn end(&self) -> u64 {
        let state = self.state.load(Ordering::Acquire);
        #[allow(clippy::cast_possible_truncation)]
        let end = state as u64;
        end
    }
    /// Returns `end - start` (saturating), i.e. how much work is left.
    #[must_use]
    pub fn remain(&self) -> u64 {
        let range = self.get();
        range.end.saturating_sub(range.start)
    }
    /// # Errors
    /// 1. Returns [`RangeError`] when `start > end`
    /// 2. Returns `None` when `remain < 2` without modifying itself
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
    /// Atomically claims and returns the entire remaining range `start..end`,
    /// emptying this task (sets `start = end`).
    ///
    /// Returns `None` if the task is already empty.
    #[must_use]
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
    /// Creates a [`WeakTask`] that does not keep the task's state alive.
    #[must_use]
    pub fn downgrade(&self) -> WeakTask {
        WeakTask {
            state: Arc::downgrade(&self.state),
        }
    }
    /// Returns the number of strong ([`Task`]) references to this task's state.
    #[must_use]
    pub fn strong_count(&self) -> usize {
        Arc::strong_count(&self.state)
    }
    /// Returns the number of weak ([`WeakTask`]) references to this task's state.
    #[must_use]
    pub fn weak_count(&self) -> usize {
        Arc::weak_count(&self.state)
    }
}
/// Creates a [`Task`] from a `start..end` range.
///
/// # Panics
/// Panics (via [`Task::new`]) if `range.start > range.end`.
impl From<Range<u64>> for Task {
    fn from(value: Range<u64>) -> Self {
        Self::new(value)
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
    #![allow(clippy::unwrap_used)]
    extern crate std;
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::vec::Vec;

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

    #[test]
    fn test_safe_add_start_no_progress() {
        let task = Task::new(10..20);
        // bias 0 -> start does not advance
        assert_eq!(task.safe_add_start(10, 0), Err(RangeError));
        // bias would not exceed current start
        assert_eq!(task.safe_add_start(8, 2), Err(RangeError));
    }

    #[test]
    fn test_safe_add_start_claims_span() {
        let task = Task::new(10..20);
        let span = task.safe_add_start(10, 5).unwrap();
        assert_eq!(span, 10..15);
        assert_eq!(task.start(), 15);
        assert_eq!(task.remain(), 5);
    }

    #[test]
    fn test_safe_add_start_capped_at_end() {
        let task = Task::new(10..12);
        let span = task.safe_add_start(10, 100).unwrap();
        assert_eq!(span, 10..12);
        assert_eq!(task.remain(), 0);
    }

    #[test]
    fn test_take_empties() {
        let task = Task::new(5..9);
        assert_eq!(task.take(), Some(5..9));
        assert_eq!(task.take(), None);
        assert_eq!(task.remain(), 0);
    }

    #[test]
    fn test_downgrade_upgrade() {
        let task = Task::new(1..10);
        let weak = task.downgrade();
        assert_eq!(weak.strong_count(), 1);
        assert_eq!(weak.upgrade().unwrap().get(), 1..10);
        drop(task);
        assert_eq!(weak.upgrade(), None);
    }

    #[test]
    fn test_partial_eq_by_ptr() {
        let a = Task::new(1..10);
        let b = a.clone();
        assert_eq!(a, b);
        let c = Task::new(1..10);
        assert_ne!(a, c);
    }

    #[test]
    fn test_split_two_halves() {
        let task = Task::new(0..100);
        let range = task.split_two().unwrap().unwrap();
        assert_eq!(range, 50..100);
        assert_eq!(task.get(), 0..50);
    }

    #[test]
    fn weak_task_reports_strong_and_weak_counts() {
        // Covers WeakTask::strong_count (task.rs 51-52) and WeakTask::weak_count
        // (task.rs 55-56).
        let task = Task::new(1..10);
        let weak = task.downgrade();
        assert_eq!(weak.strong_count(), 1);
        assert_eq!(weak.weak_count(), 1);
        // A second weak reference bumps the weak count.
        let weak2 = task.downgrade();
        assert_eq!(weak2.weak_count(), 2);
        drop(weak2);
        assert_eq!(weak.weak_count(), 1);
    }

    #[test]
    fn task_weak_count_reflects_weak_refs() {
        // Covers Task::weak_count (task.rs 218-220).
        let task = Task::new(1..10);
        assert_eq!(task.weak_count(), 0);
        let _w1 = task.downgrade();
        assert_eq!(task.weak_count(), 1);
        let _w2 = task.downgrade();
        assert_eq!(task.weak_count(), 2);
    }

    #[test]
    fn safe_add_start_survives_contention() {
        // Many threads advancing the same task forces the CAS in
        // `safe_add_start` to fail and retry, exercising the `Err(x) => old_state = x`
        // branch (`task.rs` line 135).
        let task = Arc::new(Task::new(0..2_000));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let t = task.clone();
            handles.push(thread::spawn(move || {
                loop {
                    let s = t.start();
                    if s >= 2_000 {
                        break;
                    }
                    if t.safe_add_start(s, 1).is_err() {
                        // Lost the CAS race; loop and retry (exercises the err path).
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(task.get(), 2_000..2_000);
        assert_eq!(task.remain(), 0);
    }

    #[test]
    fn split_two_survives_contention() {
        // Contended `split_two` exercises its CAS-failure branch (`task.rs` line 176).
        let task = Arc::new(Task::new(0..2_000));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let t = task.clone();
            handles.push(thread::spawn(
                move || {
                    while t.split_two().unwrap().is_some() {}
                },
            ));
        }
        for h in handles {
            h.join().unwrap();
        }
        // split_two leaves a single (un-splittable) remaining element.
        assert_eq!(task.remain(), 1);
    }

    #[test]
    fn take_survives_contention() {
        // Contended `take` exercises its CAS-failure branch (`task.rs` line 200).
        let task = Arc::new(Task::new(0..2_000));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let t = task.clone();
            handles.push(thread::spawn(move || while t.take().is_some() {}));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(task.remain(), 0);
    }

    #[test]
    fn split_two_reports_invariant_violation_when_start_gt_end() {
        // `Task::new` panics on start > end, so a corrupted/invalid state can
        // only be built through the public `state` field. This pins the
        // `range.start > range.end` guard in `split_two` (task.rs line 162).
        let bad = Task {
            state: std::sync::Arc::new(portable_atomic::AtomicU128::new((20u128 << 64) | 0xA)),
        };
        assert_eq!(bad.split_two(), Err(RangeError));
    }
}
