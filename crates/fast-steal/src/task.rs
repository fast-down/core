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

/// A cancellable, concurrent-safe unit of work that tracks a `start..end` range.
///
/// `Task` is a reference-counted *handle*: each worker holds a strong `Task`
/// (an `Arc<TaskInner>`), giving every worker a **distinct identity** even when
/// two workers speculatively share the same progress cursor. This identity is
/// what `set_threads`'s liveness sweep keys off (via `WeakTask`), so a dead
/// worker is reclaimed regardless of how many twins still reference its cursor.
///
/// The range is stored as a single atomic `u128` inside `TaskInner`, allowing
/// lock-free reads and fine-grained progress updates. Multiple workers can
/// safely steal sub-ranges from the same task via [`split_two`](Task::split_two).
///
/// Two `Task`s are equal iff they point to the same underlying state (see the
/// `PartialEq` impl, which uses `Arc::ptr_eq`).
#[derive(Debug, Clone)]
pub struct Task(Arc<TaskInner>);

/// The identity-bearing inner of `Task`.
///
/// Kept separate from `state` so the liveness refcount counts *worker identity*,
/// not the shared progress cursor. Only the `state` field is shared between
/// speculative twins; each twin still owns its own `TaskInner` allocation.
#[derive(Debug)]
struct TaskInner {
    /// Atomic state packing `start` (high 64 bits) and `end` (low 64 bits).
    ///
    /// Private to this module, as is [`TaskInner`] itself: the only way in is
    /// through [`Task`]'s accessors ([`Task::start`], [`Task::end`],
    /// [`Task::get`], [`Task::safe_add_start`], ...), which is what keeps the
    /// packed encoding an implementation detail.
    state: Arc<AtomicU128>,
}

/// A weak reference to a `Task`, obtained via [`Task::downgrade`].
///
/// Does not prevent the task from being deallocated. Use [`upgrade`](WeakTask::upgrade)
/// to attempt to obtain a strong `Task` reference.
///
/// A `WeakTask` points at a worker's *identity* (`TaskInner`), not at the shared
/// progress cursor — so [`strong_count`](WeakTask::strong_count) and
/// [`is_alive`](WeakTask::is_alive) report whether that worker is still around.
#[derive(Debug, Clone)]
pub struct WeakTask(Weak<TaskInner>);

impl WeakTask {
    /// Attempts to upgrade to a strong [`Task`].
    ///
    /// Returns `None` if all strong references to the underlying task identity
    /// have already been dropped (i.e. the worker that owned it has exited).
    #[must_use]
    pub fn upgrade(&self) -> Option<Task> {
        self.0.upgrade().map(Task)
    }
    /// Returns the number of strong [`Task`] references to the underlying task
    /// identity. Used by [`is_alive`](WeakTask::is_alive), which the liveness
    /// sweep in `set_threads` relies on.
    #[must_use]
    pub fn strong_count(&self) -> usize {
        self.0.strong_count()
    }
    /// Returns the number of weak [`WeakTask`] references to the underlying task
    /// identity.
    #[must_use]
    pub fn weak_count(&self) -> usize {
        self.0.weak_count()
    }
    /// Returns `true` if at least one strong [`Task`] reference to this worker's
    /// identity still exists. This is the exact "is this worker alive?" test the
    /// liveness sweep relies on — it is independent of how many twins share the
    /// worker's progress cursor.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.0.strong_count() > 0
    }
}

/// Error returned when a task range invariant is violated (`start > end`), or
/// when [`safe_add_start`](Task::safe_add_start) cannot make forward progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeError;

impl fmt::Display for RangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "range error: start > end, no forward progress, or a start ahead of the cursor"
        )
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
        Self(Arc::new(TaskInner {
            state: Arc::new(AtomicU128::new(Self::pack(range))),
        }))
    }
    /// Returns the current `start..end` range, loaded atomically with `Acquire`
    /// ordering.
    #[must_use]
    pub fn get(&self) -> Range<u64> {
        let state = self.0.state.load(Ordering::Acquire);
        Self::unpack(state)
    }
    /// Returns the current start of the work range (the high 64 bits of the atomic
    /// state).
    ///
    /// Loads independently of [`end`](Task::end): combining the two
    /// (`task.end() - task.start()`) can observe a torn snapshot under
    /// concurrency. Use [`get`](Task::get) or [`remain`](Task::remain) when a
    /// consistent view of both bounds is required.
    #[must_use]
    pub fn start(&self) -> u64 {
        (self.0.state.load(Ordering::Acquire) >> 64) as u64
    }
    /// Atomically advances `start` to `min(start + bias, end)`, but only if that
    /// makes forward progress; then returns the slice that was claimed.
    ///
    /// `start` must be a cursor value the caller actually observed via
    /// [`start`](Task::start) or [`get`](Task::get); a `start` that runs *ahead*
    /// of the task's real cursor is rejected, because honouring it would skip
    /// work no worker ever executed. A *stale* `start` is still accepted, but the
    /// returned span then begins at the real cursor and is shorter than `bias` —
    /// always consume the returned span, never assume `start..start + bias`.
    ///
    /// # Errors
    /// Returns [`RangeError`] when `start + bias` would not exceed the current
    /// `start` (no progress, a non-positive bias), or when
    /// `start` runs ahead of the task's cursor.
    pub fn safe_add_start(&self, start: u64, bias: u64) -> Result<Range<u64>, RangeError> {
        let new_start = start.saturating_add(bias);
        let mut old_state = self.0.state.load(Ordering::Acquire);
        loop {
            let mut range = Self::unpack(old_state);
            if start > range.start {
                // The caller's cursor runs ahead of reality: accepting it would
                // jump over `range.start..start`, silently discarding work.
                break Err(RangeError);
            }
            let new_start = new_start.min(range.end);
            if new_start <= range.start {
                break Err(RangeError);
            }
            let span = range.start..new_start;
            range.start = new_start;
            let new_state = Self::pack(range);
            match self.0.state.compare_exchange_weak(
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
    ///
    /// Loads independently of [`start`](Task::start); see that method for the
    /// torn-snapshot caveat when combining the two.
    #[must_use]
    pub fn end(&self) -> u64 {
        #[allow(clippy::cast_possible_truncation)]
        let end = self.0.state.load(Ordering::Acquire) as u64;
        end
    }
    /// Returns `end - start` (saturating), i.e. how much work is left.
    #[must_use]
    pub fn remain(&self) -> u64 {
        let range = self.get();
        range.end.saturating_sub(range.start)
    }
    /// Splits the work range in half, handing `mid..end` back to the caller as a
    /// new task and keeping `start..mid` in `self`.
    ///
    /// The split only proceeds when both halves are at least `min_chunk_size`,
    /// i.e. `remain >= min_chunk_size * 2`. That test runs *inside* the
    /// compare-and-swap loop against the same atomic snapshot the commit
    /// observes, so a concurrent cursor-sharer (`share_state`) advancing `start`
    /// between the call and the commit cannot leak a half smaller than
    /// `min_chunk_size`. When the range is too small to split under
    /// `min_chunk_size`, returns `Ok(None)` without modifying `self`.
    ///
    /// `min_chunk_size == 0` is refused with the same `Ok(None)`: a zero minimum
    /// makes the size test vacuous, and on an empty range `midpoint` collapses to
    /// `start`, so the split would report success while returning the empty
    /// `start..start` and leaving `self` unchanged — a public
    /// `while task.split_two(0)?.is_some() {}` drain loop would never terminate.
    /// The refusal is unconditional for `0` so that "a reported split always
    /// consumed something" stays a property of the whole API; callers that want to
    /// split down to single items pass `1`.
    ///
    /// # Errors
    /// 1. Returns [`RangeError`] when `start > end`
    /// 2. Returns `None` when `min_chunk_size == 0` or `remain < min_chunk_size * 2`,
    ///    without modifying itself
    pub fn split_two(&self, min_chunk_size: u64) -> Result<Option<Range<u64>>, RangeError> {
        let mut old_state = self.0.state.load(Ordering::Acquire);
        loop {
            let range = Self::unpack(old_state);
            if range.start > range.end {
                return Err(RangeError);
            }
            if min_chunk_size == 0 {
                return Ok(None);
            }
            if range.end - range.start < min_chunk_size.saturating_mul(2) {
                return Ok(None);
            }
            let mid = range.start.midpoint(range.end);
            let new_state = Self::pack(range.start..mid);
            match self.0.state.compare_exchange_weak(
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
    /// emptying this task by moving `end` back to `start`.
    ///
    /// Shares its error contract with [`split_two`](Task::split_two): both
    /// consume remaining work, so both report a violated range invariant instead
    /// of normalising it away.
    ///
    /// # Errors
    /// 1. Returns [`RangeError`] when `start > end`
    /// 2. Returns `Ok(None)` when the task is already empty, without modifying it
    pub fn take(&self) -> Result<Option<Range<u64>>, RangeError> {
        let mut old_state = self.0.state.load(Ordering::Acquire);
        loop {
            let range = Self::unpack(old_state);
            if range.start > range.end {
                return Err(RangeError);
            }
            if range.start == range.end {
                return Ok(None);
            }
            let new_state = Self::pack(range.start..range.start);
            match self.0.state.compare_exchange_weak(
                old_state,
                new_state,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(Some(range)),
                Err(x) => old_state = x,
            }
        }
    }
    /// Creates a [`WeakTask`] that does not keep the task's state alive.
    #[must_use]
    pub fn downgrade(&self) -> WeakTask {
        WeakTask(Arc::downgrade(&self.0))
    }
    /// Returns the number of strong [`Task`] references to this worker's *identity*
    /// (`TaskInner`).
    ///
    /// `Task` and [`WeakTask`] are a paired strong/weak view of the same identity
    /// allocation, so this equals [`WeakTask::strong_count`] on a matching
    /// `WeakTask` — exactly like `Arc`/`Weak`. The (different) cursor-sharer count
    /// that `steal` caps on is exposed separately as the crate-internal
    /// `sharer_count` accessor.
    #[must_use]
    pub fn strong_count(&self) -> usize {
        Arc::strong_count(&self.0)
    }
    /// Returns the number of weak [`WeakTask`] references to this worker's *identity*
    /// (`TaskInner`). Pairs with [`strong_count`](Task::strong_count), and equals
    /// [`WeakTask::weak_count`] on a matching `WeakTask`.
    #[must_use]
    pub fn weak_count(&self) -> usize {
        Arc::weak_count(&self.0)
    }
    /// Returns the number of [`Task`] references currently sharing this task's
    /// progress *cursor* (the `state` `Arc<AtomicU128>`).
    ///
    /// This is a *different* quantity from [`strong_count`](Task::strong_count):
    /// the latter counts worker identities, whereas this counts how many workers
    /// have aliased the same cursor via [`share_state`](Task::share_state)
    /// (speculative sharing). Only `steal` consults it, to cap how many workers
    /// share one cursor.
    #[must_use]
    pub(crate) fn sharer_count(&self) -> usize {
        Arc::strong_count(&self.0.state)
    }
    /// Rebinds this task to share `other`'s progress cursor while keeping its own
    /// distinct identity.
    ///
    /// Used by speculative sharing in `steal`: the caller's task aliases the
    /// victim's cursor without copying it, and — unlike a plain `clone` — remains
    /// a *separate* worker identity. That separation is what lets the liveness
    /// sweep in `set_threads` still track each worker independently even after
    /// sharing.
    pub(crate) fn share_state(&mut self, other: &Self) {
        *self = Self(Arc::new(TaskInner {
            state: other.0.state.clone(),
        }));
    }
    /// Builds a `Task` from a raw state, bypassing the range invariant checked by
    /// [`new`](Task::new). For tests only: fabricates a corrupted (inverted-range)
    /// state that `new` would refuse.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn from_raw_state(state: Arc<AtomicU128>) -> Self {
        Self(Arc::new(TaskInner { state }))
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
        Arc::ptr_eq(&self.0.state, &other.0.state)
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
        let range = task.split_two(1).unwrap().unwrap();
        assert_eq!(task.start(), 1);
        assert_eq!(task.end(), 3);
        assert_eq!(range.start, 3);
        assert_eq!(range.end, 6);
    }

    #[test]
    fn test_split_empty() {
        let task = Task::new(1..1);
        let range = task.split_two(1).unwrap();
        assert_eq!(task.start(), 1);
        assert_eq!(task.end(), 1);
        assert_eq!(range, None);
    }

    #[test]
    fn test_split_one() {
        let task = Task::new(1..2);
        let range = task.split_two(1).unwrap();
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
        assert_eq!(task.take(), Ok(Some(5..9)));
        assert_eq!(task.take(), Ok(None));
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
        let range = task.split_two(1).unwrap().unwrap();
        assert_eq!(range, 50..100);
        assert_eq!(task.get(), 0..50);
    }

    #[test]
    fn split_two_respects_min_chunk_size() {
        // `remain == 2 * min - 1` cannot yield two halves each >= min.
        let task = Task::new(0..(2 * 8 - 1)); // remain = 15, min = 8
        assert_eq!(task.split_two(8), Ok(None));
        assert_eq!(task.get(), 0..15); // unchanged on refusal

        // `remain == 2 * min` splits into exactly two `min`-sized halves.
        let task = Task::new(0..16);
        let range = task.split_two(8).unwrap().unwrap();
        assert_eq!(range, 8..16);
        assert_eq!(task.get(), 0..8);

        // A chunk smaller than `min` is never handed out.
        let task = Task::new(0..10);
        assert_eq!(task.split_two(8), Ok(None));
    }

    /// A zero `min_chunk_size` is refused instead of "succeeding" with an empty
    /// half. Before the guard, the `remain < min * 2` threshold was vacuous
    /// (`min * 2 == 0`), so an empty range returned `Ok(Some(start..start))` while
    /// leaving `self` unchanged -- and `while task.split_two(0)?.is_some() {}`
    /// spun forever. The queue hid this behind its own `.max(1)`; the public API
    /// must terminate on its own.
    #[test]
    fn split_two_refuses_a_zero_min_chunk_size() {
        // The sharp case: an empty range, where `midpoint` collapses to `start`.
        let task = Task::new(5..5);
        assert_eq!(task.split_two(0), Ok(None));
        assert_eq!(
            task.get(),
            5..5,
            "a refused split leaves the task untouched"
        );

        // A non-empty range is refused too, so the outcome never depends on
        // whether the range happens to be empty.
        let task = Task::new(0..100);
        assert_eq!(task.split_two(0), Ok(None));
        assert_eq!(task.get(), 0..100);

        let task = Task::new(0..1);
        assert_eq!(task.split_two(0), Ok(None));

        // Termination itself is the contract: this loop must end immediately.
        let task = Task::new(7..7);
        let mut rounds = 0u32;
        while task.split_two(0).unwrap().is_some() {
            rounds += 1;
            assert!(rounds < 4, "split_two(0) kept reporting a successful split");
        }
    }

    #[test]
    fn weak_task_reports_strong_and_weak_counts() {
        // `WeakTask` mirrors the counts of the identity it points at.
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
        // Weak refs come from `downgrade`, so they target the identity, not the cursor.
        let task = Task::new(1..10);
        assert_eq!(task.weak_count(), 0);
        let _w1 = task.downgrade();
        assert_eq!(task.weak_count(), 1);
        let _w2 = task.downgrade();
        assert_eq!(task.weak_count(), 2);
    }

    #[test]
    fn safe_add_start_survives_contention() {
        // Contention forces the CAS in `safe_add_start` to fail and retry.
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
        // Contention exercises `split_two`'s CAS-failure branch.
        let task = Arc::new(Task::new(0..2_000));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let t = task.clone();
            handles.push(thread::spawn(
                move || {
                    while t.split_two(1).unwrap().is_some() {}
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
        // Contention exercises `take`'s CAS-failure branch.
        let task = Arc::new(Task::new(0..2_000));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let t = task.clone();
            handles.push(thread::spawn(
                move || {
                    while matches!(t.take(), Ok(Some(_))) {}
                },
            ));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(task.remain(), 0);
    }

    #[test]
    fn split_two_reports_invariant_violation_when_start_gt_end() {
        // Only `from_raw_state` can build the inverted state `new` refuses;
        // `split_two` reports it instead of normalising it away.
        let bad = Task::from_raw_state(std::sync::Arc::new(portable_atomic::AtomicU128::new(
            (20u128 << 64) | 0xA,
        )));
        assert_eq!(bad.split_two(1), Err(RangeError));
    }

    /// `RangeError`'s rendered text and its `Error` impl (no source).
    #[test]
    fn range_error_display_and_error_impl() {
        use std::{error::Error, format, string::ToString};
        let e = RangeError;
        assert_eq!(
            e.to_string(),
            "range error: start > end, no forward progress, or a start ahead of the cursor"
        );
        assert_eq!(
            format!("{e}"),
            "range error: start > end, no forward progress, or a start ahead of the cursor"
        );
        let as_dyn: &dyn Error = &e;
        assert!(as_dyn.source().is_none());
    }

    /// `Task::new` panics on an inverted range.
    #[test]
    #[should_panic(expected = "assertion failed")]
    fn new_panics_when_start_gt_end() {
        // Inline `10..5` would trip `clippy::reversed_empty_ranges`.
        let _ = Task::new(core::ops::Range { start: 10, end: 5 });
    }

    /// `From` inherits `new`'s panic contract; `TaskQueue::new` funnels user
    /// ranges through it.
    #[test]
    #[should_panic(expected = "assertion failed")]
    fn from_range_panics_when_start_gt_end() {
        let _ = Task::from(core::ops::Range { start: 10, end: 5 });
    }

    /// The `From<Range<u64>>` happy path.
    #[test]
    fn from_range_builds_equivalent_task() {
        let t = Task::from(3..9);
        assert_eq!(t.get(), 3..9);
        let t2: Task = (0..0).into();
        assert_eq!(t2.get(), 0..0);
        assert_eq!(t2.remain(), 0);
    }

    /// Packing round-trips at the `u64` bounds, where a sloppy shift or truncate
    /// would corrupt the range.
    #[test]
    fn pack_unpack_round_trips_at_u64_bounds() {
        for range in [
            0..0,
            0..u64::MAX,
            u64::MAX..u64::MAX,
            (u64::MAX - 1)..u64::MAX,
        ] {
            let t = Task::new(range.clone());
            assert_eq!(t.get(), range, "round-trip lost bits");
            assert_eq!(t.start(), range.start);
            assert_eq!(t.end(), range.end);
        }
        assert_eq!(Task::new(0..u64::MAX).remain(), u64::MAX);
    }

    /// An overflowing bias saturates to `u64::MAX` and is clamped by `min(end)`,
    /// so it claims the whole remaining range.
    #[test]
    fn safe_add_start_saturates_on_u64_overflow() {
        let task = Task::new(0..u64::MAX);
        let span = task.safe_add_start(0, u64::MAX).unwrap();
        assert_eq!(span, 0..u64::MAX);
        assert_eq!(task.get(), u64::MAX..u64::MAX);
        assert_eq!(task.remain(), 0);

        // Saturation at a non-zero cursor still clamps at `end`, never beyond.
        let task = Task::new((u64::MAX - 5)..u64::MAX);
        let span = task.safe_add_start(u64::MAX - 5, u64::MAX).unwrap();
        assert_eq!(span, (u64::MAX - 5)..u64::MAX);
        assert_eq!(task.remain(), 0);
    }

    /// A `start` ahead of the real cursor is rejected and leaves the task
    /// untouched, so no work is silently skipped.
    #[test]
    fn safe_add_start_rejects_caller_start_ahead_of_cursor() {
        let task = Task::new(0..100);
        // Caller asks to advance 1 step "from 50", but the cursor is at 0.
        assert_eq!(task.safe_add_start(50, 1), Err(RangeError));
        assert_eq!(task.get(), 0..100, "a rejected call must not mutate state");
        // Even a start beyond `end` is refused rather than clamped by `min`.
        assert_eq!(task.safe_add_start(500, 1), Err(RangeError));
        assert_eq!(task.get(), 0..100);
        // One step ahead is still ahead.
        task.safe_add_start(0, 10).unwrap(); // cursor -> 10
        assert_eq!(task.safe_add_start(11, 1), Err(RangeError));
        assert_eq!(task.get(), 10..100);
        // ...while the exact cursor keeps working.
        assert_eq!(task.safe_add_start(10, 1), Ok(10..11));
    }

    /// The mirror case, which *is* the intended usage: a caller whose `start`
    /// is stale still succeeds if `start + bias` overtakes the real cursor, but
    /// the returned span begins at the real cursor and is therefore shorter
    /// than `bias`. Callers must use the returned span, never assume
    /// `start..start + bias`.
    #[test]
    fn safe_add_start_span_shrinks_when_caller_start_is_stale() {
        let task = Task::new(0..100);
        task.safe_add_start(0, 5).unwrap(); // cursor -> 5
        let span = task.safe_add_start(3, 5).unwrap(); // stale 3, target 8
        assert_eq!(
            span,
            5..8,
            "span must start at the real cursor, not the stale one"
        );
        assert_eq!(task.start(), 8);
    }

    /// `take` shares `split_two`'s `start > end -> Err` contract, so a corrupted
    /// state is reported rather than normalised into a silently empty range.
    #[test]
    fn take_reports_invariant_violation_on_corrupted_state() {
        let bad = Task::from_raw_state(Arc::new(portable_atomic::AtomicU128::new(
            (20u128 << 64) | 0xA,
        )));
        // Inline `20..10` would trip `clippy::reversed_empty_ranges`.
        let inverted = core::ops::Range {
            start: 20u64,
            end: 10u64,
        };
        assert_eq!(bad.get(), inverted);
        assert_eq!(bad.take(), Err(RangeError));
        // Preserved, not normalised away: still diagnosable, and `take` agrees
        // with `split_two` on the same input.
        assert_eq!(bad.get(), inverted);
        assert_eq!(bad.split_two(1), Err(RangeError));
    }

    /// `remain` saturates to 0 on an inverted range, which is why a corrupted
    /// task looks "finished" to `steal`'s `max_by_key`.
    #[test]
    fn remain_saturates_to_zero_on_corrupted_state() {
        let bad = Task::from_raw_state(Arc::new(portable_atomic::AtomicU128::new(
            (20u128 << 64) | 0xA,
        )));
        assert_eq!(bad.remain(), 0);
    }

    /// `weak_count` collapses to 0 once the last strong ref dies: it does not
    /// report surviving weak refs. `Task`'s paired accessor behaves identically.
    #[test]
    fn weak_task_weak_count_collapses_to_zero_without_strong_refs() {
        let task = Task::new(0..10);
        let w1 = task.downgrade();
        let _w2 = task.downgrade();
        assert_eq!(w1.weak_count(), 2);
        assert_eq!(w1.strong_count(), 1);
        drop(task);
        assert_eq!(w1.strong_count(), 0);
        assert_eq!(
            w1.weak_count(),
            0,
            "weak_count collapses once the strong count hits 0"
        );
        assert!(w1.upgrade().is_none());
    }

    /// `sharer_count` counts strong refs to the *shared cursor*: a speculative
    /// sharer (`share_state`) bumps it, a plain `clone` does not.
    #[test]
    fn task_sharer_count_tracks_cursor_sharers() {
        let task = Task::new(0..10);
        assert_eq!(task.sharer_count(), 1);

        // A speculative sharer aliases the same cursor and holds its own strong
        // ref to it.
        let mut twin = Task::new(0..0);
        twin.share_state(&task);
        assert_eq!(
            task.sharer_count(),
            2,
            "the twin holds a strong ref to the cursor"
        );
        assert_eq!(twin, task, "twins compare equal via the shared cursor");
        drop(twin);
        assert_eq!(
            task.sharer_count(),
            1,
            "dropping the twin releases its cursor ref"
        );

        // A plain `clone` shares the worker identity, not an extra cursor ref, so
        // it must NOT change `sharer_count`.
        let _alias = task.clone();
        assert_eq!(
            task.sharer_count(),
            1,
            "clone shares identity, not a cursor ref"
        );

        // An upgraded `WeakTask` holds a strong ref to the *identity* (TaskInner),
        // not to the cursor, so it also leaves `sharer_count` unchanged.
        let w = task.downgrade();
        let _up = w.upgrade().unwrap();
        assert_eq!(
            task.sharer_count(),
            1,
            "upgrade keeps the cursor count unchanged"
        );
    }

    /// `strong_count` counts *worker identity* (like `WeakTask::strong_count`),
    /// never the shared cursor, and the paired accessors stay equal.
    #[test]
    fn task_strong_count_counts_identity_not_cursor() {
        let task = Task::new(0..10);
        let weak = task.downgrade();

        // Paired strong/weak views of the same identity: equal counts.
        assert_eq!(task.strong_count(), 1);
        assert_eq!(weak.strong_count(), 1);

        // A plain `clone` shares the identity, so it bumps `strong_count`...
        let _alias = task.clone();
        assert_eq!(task.strong_count(), 2);
        // ...and the paired `WeakTask` accessor tracks the very same identity.
        assert_eq!(weak.strong_count(), 2);

        // A `clone` does NOT touch the cursor, so `sharer_count` is unchanged.
        assert_eq!(task.sharer_count(), 1, "clone does not alias the cursor");

        // `share_state` aliases the cursor (new identity) and bumps `sharer_count`,
        // but leaves `strong_count` (identity) untouched.
        let mut twin = Task::new(0..0);
        twin.share_state(&task);
        assert_eq!(task.sharer_count(), 2, "share_state aliases the cursor");
        assert_eq!(task.strong_count(), 2, "share_state keeps its own identity");
    }

    /// `WeakTask::is_alive` reports whether the worker *identity* is still held,
    /// independent of how many twins share the cursor.
    #[test]
    fn weak_task_is_alive_tracks_identity_not_cursor() {
        let task = Task::new(0..10);
        let weak = task.downgrade();
        assert!(weak.is_alive());
        drop(task);
        assert!(!weak.is_alive(), "identity dropped -> not alive");

        // A speculative twin holds its OWN identity, so the victim's death does
        // not flip the twin's `is_alive`.
        let victim = Task::new(0..10);
        let mut twin = Task::new(0..0);
        twin.share_state(&victim);
        let twin_weak = twin.downgrade();
        assert!(twin_weak.is_alive());
        drop(victim);
        assert!(
            twin_weak.is_alive(),
            "twin keeps its own identity alive after the victim dies"
        );
    }

    /// `mid = start + (end - start) / 2` must not overflow at the top of the
    /// u64 range, where the naive `(start + end) / 2` would wrap.
    #[test]
    fn split_two_handles_u64_extremes_without_overflow() {
        let task = Task::new(0..u64::MAX);
        let hi = task.split_two(1).unwrap().unwrap();
        assert_eq!(hi, (u64::MAX / 2)..u64::MAX);
        assert_eq!(task.get(), 0..(u64::MAX / 2));

        let task = Task::new((u64::MAX - 3)..u64::MAX);
        let hi = task.split_two(1).unwrap().unwrap();
        assert_eq!(hi, (u64::MAX - 2)..u64::MAX);
        assert_eq!(task.get(), (u64::MAX - 3)..(u64::MAX - 2));
    }
}
