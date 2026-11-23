//! Cancellable units of work.
//!
//! A [`Task`] stores its remaining work as a single atomic `u128` packing the
//! `start` (high 64 bits) and `end` (low 64 bits) bounds, which lets multiple
//! worker threads advance the same task without locking on targets that provide
//! native 128-bit atomics; `portable-atomic` falls back to a lock where they are
//! missing (`thumbv7em-none-eabi`, for example). Because the crate is `no_std`,
//! only `alloc` is required for the reference-counted state.

extern crate alloc;
use alloc::sync::Arc;
use core::{
    fmt,
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use portable_atomic::AtomicU128;

/// A cancellable, concurrent-safe unit of work that tracks a `start..end` range.
///
/// `Task` is a reference-counted *handle*: each worker holds a strong `Task`
/// (an `Arc<TaskInner>`), giving every worker a **distinct identity** even when
/// two workers speculatively share the same progress cursor. The queue keeps its
/// own view of that identity in its running set, so a dead worker is reclaimed
/// without waiting for every clone to be dropped.
///
/// Liveness is *not* the identity's strong count. The task handed to a worker
/// by [`Executor::execute`](crate::Executor::execute) carries an internal
/// `AliveGuard` that is dropped, marking the worker dead, when that very task
/// value is dropped. `Clone` shares the identity and cursor but deliberately
/// leaves the guard behind, so a clone held by an observer cannot pin a worker
/// that has already exited.
///
/// The range is stored as a single atomic `u128` inside `TaskInner`, allowing
/// lock-free reads and fine-grained progress updates on targets with native
/// 128-bit atomics (`portable-atomic` uses a lock elsewhere). Multiple workers
/// can safely steal sub-ranges from the same task via `split_two`.
///
/// `Task` deliberately implements neither `PartialEq` nor `Eq`: equality had two
/// meanings (same progress cursor, same worker identity) that callers kept
/// confusing. Ask for one explicitly with `same_cursor` or `same_identity`.
pub struct Task {
    inner: Arc<TaskInner>,
    /// Set only on the value moved into a worker: dropping it marks that
    /// registration dead. Never cloned (see the `Clone` impl), and never read —
    /// it exists for its `Drop`.
    #[allow(dead_code, reason = "held only for its Drop impl")]
    keep_alive: Option<AliveGuard>,
}

/// Renders the cursor as a plain range plus the worker's liveness, which is what
/// a caller can observe through this handle.
///
/// Hand-written rather than derived: the packed `AtomicU128` state is an
/// implementation detail, and printing it would be both unreadable and a
/// snapshot taken field-by-field *outside* the atomic (it could show a torn
/// range that never existed). A derived impl would also demand `Debug` of the
/// non-`Clone` liveness guard.
impl fmt::Debug for Task {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Task")
            .field("range", &self.get())
            .field("alive", &self.is_alive())
            .finish()
    }
}

/// Non-`Clone` liveness guard for one registered worker.
///
/// Created when the queue registers a task (grow or refill) and carried inside
/// the [`Task`] value that [`Executor::execute`](crate::Executor::execute) moves
/// into the worker future. Dropping it clears the identity's alive flag, which
/// is what [`Task::is_alive`] reports. Because it cannot be cloned, no
/// external handle can extend a dead worker's lifetime.
#[derive(Debug)]
struct AliveGuard(Arc<AtomicBool>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The identity-bearing inner of `Task`.
///
/// Kept separate from `state` so liveness tracks *worker identity*, not the
/// shared progress cursor. Only the `state` field is shared between speculative
/// twins; each twin still owns its own `TaskInner` allocation and alive flag.
#[derive(Debug)]
struct TaskInner {
    /// Atomic state packing `start` (high 64 bits) and `end` (low 64 bits).
    ///
    /// Private to this module, as is [`TaskInner`] itself: the only way in is
    /// through [`Task`]'s accessors ([`Task::get`], [`Task::safe_add_start`],
    /// ...), which is what keeps the packed encoding an implementation detail.
    state: Arc<AtomicU128>,
    /// `true` until the identity's `AliveGuard` is dropped (or forever for an
    /// identity that was never registered, which then liveness-tracks like a
    /// plain shared allocation).
    alive: Arc<AtomicBool>,
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
        Self::from_state(Arc::new(AtomicU128::new(Self::pack(range))))
    }
    /// Builds an unregistered identity around `state`: no liveness guard, and
    /// the alive flag starts `false` because nothing has armed this identity.
    ///
    /// This is also how tests fabricate a state `new` would refuse (an inverted
    /// range): [`new`](Task::new) is the only constructor that checks the range
    /// invariant.
    pub(crate) fn from_state(state: Arc<AtomicU128>) -> Self {
        Self {
            inner: Arc::new(TaskInner {
                state,
                alive: Arc::new(AtomicBool::new(false)),
            }),
            keep_alive: None,
        }
    }
    /// Builds a registered identity around an existing cursor `state`.
    ///
    /// The building block shared by [`new_armed`](Task::new_armed) (which also
    /// makes a cursor) and [`share_state`](Task::share_state) (which takes its
    /// peer's): a fresh identity and guard over the same packed state.
    pub(crate) fn armed_on(state: Arc<AtomicU128>) -> Self {
        let alive = Arc::new(AtomicBool::new(true));
        Self {
            inner: Arc::new(TaskInner {
                state,
                alive: alive.clone(),
            }),
            keep_alive: Some(AliveGuard(alive)),
        }
    }
    /// Builds an armed task straight from `range`, for a queue refill.
    ///
    /// Equivalent to [`new`](Task::new) plus an armed identity, without the
    /// unarmed intermediate: that intermediate's own `TaskInner` was allocated
    /// only to be dropped one line later, and every refill runs while the
    /// queue's lock is held.
    ///
    /// # Panics
    /// Panics when `range.start > range.end`, like [`new`](Task::new).
    pub(crate) fn new_armed(range: Range<u64>) -> Self {
        assert!(range.start <= range.end);
        Self::armed_on(Arc::new(AtomicU128::new(Self::pack(range))))
    }
    /// Returns `true` while the worker registered against this identity is alive.
    ///
    /// The flag starts `false` for an unregistered identity (every [`Task::new`]
    /// and every clone of one) and becomes `true` only for the value
    /// [`Executor::execute`](crate::Executor::execute) hands to a worker; dropping
    /// that value clears it again.
    #[must_use]
    pub(crate) fn is_alive(&self) -> bool {
        self.inner.alive.load(Ordering::Acquire)
    }
    /// Returns the current `start..end` range, loaded atomically with `Acquire`
    /// ordering.
    #[must_use]
    pub fn get(&self) -> Range<u64> {
        let state = self.inner.state.load(Ordering::Acquire);
        Self::unpack(state)
    }
    /// Returns the current start of the work range (the high 64 bits of the atomic
    /// state).
    ///
    /// Loads independently of [`end`](Task::end): combining the two
    /// (`task.end() - task.start()`) can observe a torn snapshot under
    /// concurrency. Use `get` or `remain` when a
    /// consistent view of both bounds is required.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn start(&self) -> u64 {
        (self.inner.state.load(Ordering::Acquire) >> 64) as u64
    }
    /// Atomically advances `start` to `min(start + bias, end)`, but only if that
    /// makes forward progress; then returns the slice that was claimed.
    ///
    /// `start` must be a cursor value the caller actually observed via
    /// [`get`](Task::get); a `start` that runs *ahead*
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
        let mut old_state = self.inner.state.load(Ordering::Acquire);
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
            match self.inner.state.compare_exchange_weak(
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
    #[cfg(test)]
    #[must_use]
    pub(crate) fn end(&self) -> u64 {
        #[allow(clippy::cast_possible_truncation)]
        let end = self.inner.state.load(Ordering::Acquire) as u64;
        end
    }
    /// Returns `end - start` (saturating), i.e. how much work is left.
    #[must_use]
    pub(crate) fn remain(&self) -> u64 {
        let range = self.get();
        range.end.saturating_sub(range.start)
    }
    /// Splits the work range in half, handing `mid..end` back to the caller as a
    /// new task and keeping `start..mid` in `self`.
    ///
    /// The split only proceeds when both halves are at least `min_chunk_size`,
    /// i.e. `remain / 2 >= min_chunk_size`. That test runs *inside* the
    /// compare-and-swap loop against the same atomic snapshot the commit
    /// observes, so a concurrent cursor-sharer (`share_state`) advancing `start`
    /// between the call and the commit cannot leak a half smaller than
    /// `min_chunk_size`. When the range is too small to split under
    /// `min_chunk_size`, returns `Ok(None)` without modifying `self`.
    ///
    /// The threshold is `remain / 2 < min` rather than `remain < min * 2`: the
    /// latter doubles `min` and saturates for `min > u64::MAX / 2`, which made
    /// the test vacuous for every `remain` below `u64::MAX` and let a range of
    /// `u64::MAX` split into halves far below `min`.
    ///
    /// `min_chunk_size == 0` is refused with the same `Ok(None)`: a zero minimum
    /// makes the size test vacuous, and on an empty range `midpoint` collapses to
    /// `start`, so the split would report success while returning the empty
    /// `start..start` and leaving `self` unchanged — a
    /// `while task.split_two(0)?.is_some() {}` drain loop would never terminate.
    /// The refusal is unconditional for `0` so that "a reported split always
    /// consumed something" stays a property of the whole API; callers that want to
    /// split down to single items pass `1`.
    ///
    /// # Errors
    /// 1. Returns [`RangeError`] when `start > end`
    /// 2. Returns `None` when `min_chunk_size == 0` or `remain / 2 < min_chunk_size`,
    ///    without modifying itself
    pub(crate) fn split_two(&self, min_chunk_size: u64) -> Result<Option<Range<u64>>, RangeError> {
        let mut old_state = self.inner.state.load(Ordering::Acquire);
        loop {
            let range = Self::unpack(old_state);
            if range.start > range.end {
                return Err(RangeError);
            }
            if min_chunk_size == 0 {
                return Ok(None);
            }
            if (range.end - range.start) / 2 < min_chunk_size {
                return Ok(None);
            }
            let mid = range.start.midpoint(range.end);
            let new_state = Self::pack(range.start..mid);
            match self.inner.state.compare_exchange_weak(
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
    pub(crate) fn take(&self) -> Result<Option<Range<u64>>, RangeError> {
        let mut old_state = self.inner.state.load(Ordering::Acquire);
        loop {
            let range = Self::unpack(old_state);
            if range.start > range.end {
                return Err(RangeError);
            }
            if range.start == range.end {
                return Ok(None);
            }
            let new_state = Self::pack(range.start..range.start);
            match self.inner.state.compare_exchange_weak(
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
    /// Returns `true` when both handles share the same progress cursor.
    ///
    /// This is the relation `Task` used to express through `PartialEq`. It is now
    /// a named question because the other one — [`same_identity`](Task::same_identity)
    /// — has the same shape but a different answer for a speculative twin.
    #[must_use]
    pub(crate) fn same_cursor(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner.state, &other.inner.state)
    }
    /// Returns `true` when both handles point at the same worker identity rather
    /// than merely sharing a progress cursor.
    ///
    /// [`same_cursor`](Task::same_cursor) answers the weaker question and treats
    /// a speculative twin as equal to its peer. This is the stricter one a
    /// registration asks: is this the exact handle the queue handed to that
    /// worker? A `clone` shares the identity; a twin does not.
    #[must_use]
    pub(crate) fn same_identity(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
    /// Rebinds this task to share `other`'s progress cursor while keeping its own
    /// distinct liveness identity.
    ///
    /// Used by speculative sharing in `steal`: the caller's task aliases the
    /// victim's cursor without copying it, and — unlike a plain `clone` — remains
    /// a *separate* worker identity with its own guard. That separation is what
    /// lets the liveness sweep in `set_threads` still track each worker
    /// independently even after sharing.
    pub(crate) fn share_state(&mut self, other: &Self) {
        *self = Self::armed_on(other.inner.state.clone());
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

/// Clones share the identity and progress cursor but not the liveness guard:
/// an observer's clone must never keep an exited worker alive.
impl Clone for Task {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            keep_alive: None,
        }
    }
}

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

    /// A guard marks exactly the value that carries it: a clone reports the
    /// same alive flag but cannot extend the worker's lifetime, because it
    /// carries no guard of its own.
    #[test]
    fn armed_marks_only_the_value_that_carries_the_guard() {
        let armed = Task::new_armed(1..10);
        let observer = armed.clone();
        assert!(armed.is_alive());
        assert!(observer.is_alive(), "a clone shares the identity's flag");
        drop(armed);
        assert!(
            !observer.is_alive(),
            "dropping the armed value clears the shared flag"
        );
        assert_eq!(observer.get(), 1..10, "the observer keeps the cursor");
    }

    #[test]
    fn test_partial_eq_by_ptr() {
        let a = Task::new(1..10);
        let b = a.clone();
        assert!(a.same_cursor(&b));
        let c = Task::new(1..10);
        assert!(!a.same_cursor(&c));
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
        let bad = Task::from_state(std::sync::Arc::new(portable_atomic::AtomicU128::new(
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
        let bad = Task::from_state(Arc::new(portable_atomic::AtomicU128::new(
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
        let bad = Task::from_state(Arc::new(portable_atomic::AtomicU128::new(
            (20u128 << 64) | 0xA,
        )));
        assert_eq!(bad.remain(), 0);
    }

    /// `is_alive` follows the worker's own guard, independent of how many twins
    /// share the cursor.
    #[test]
    fn is_alive_tracks_the_guard_not_the_cursor() {
        let armed = Task::new_armed(0..10);
        let observer = armed.clone();
        assert!(observer.is_alive());

        // A speculative twin carries its own guard, so the victim's death does
        // not flip the twin's flag.
        let victim = Task::new_armed(0..10);
        let mut twin = Task::new(0..0);
        twin.share_state(&victim);
        let twin_observer = twin.clone();
        drop(victim);
        assert!(
            twin_observer.is_alive(),
            "the twin owns a separate guard and identity"
        );

        drop(armed);
        assert!(
            !observer.is_alive(),
            "dropping the armed value marks its worker dead"
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

    /// A `min_chunk_size` above half of `u64::MAX` must still be honoured.
    ///
    /// The old `remain < min * 2` threshold saturated at `u64::MAX`, making the
    /// test vacuous: a `u64::MAX` range split into halves far below `min`.
    #[test]
    fn split_two_honours_a_min_chunk_size_above_half_the_range() {
        let big_min = u64::MAX / 2 + 1;
        let task = Task::new(0..u64::MAX);
        assert_eq!(task.split_two(big_min), Ok(None));
        assert_eq!(task.get(), 0..u64::MAX, "a refused split changes nothing");

        // Exactly half is the boundary: both halves are at least `min`.
        let task = Task::new(0..u64::MAX);
        let hi = task.split_two(u64::MAX / 2).unwrap().unwrap();
        assert_eq!(hi.start, u64::MAX / 2);
        assert!(hi.end - hi.start >= u64::MAX / 2);
        assert!(task.remain() >= u64::MAX / 2);
    }

    /// `new_armed` builds the cursor and the armed identity in one step, without
    /// the throw-away unarmed intermediate: same range, a live guard, and a drop
    /// that clears it.
    #[test]
    fn new_armed_is_an_armed_task_over_the_range() {
        let armed = Task::new_armed(3..9);
        assert_eq!(armed.get(), 3..9);
        assert!(armed.is_alive());

        let observer = armed.clone();
        assert!(observer.is_alive());
        drop(armed);
        assert!(
            !observer.is_alive(),
            "dropping the armed value marks the worker dead"
        );
        assert_eq!(observer.get(), 3..9, "the cursor survives the guard");
    }

    /// `new_armed` inherits `new`'s panic contract.
    #[test]
    #[should_panic(expected = "assertion failed")]
    fn new_armed_panics_on_an_inverted_range() {
        let _ = Task::new_armed(core::ops::Range { start: 10, end: 5 });
    }

    /// `Debug` shows the observable cursor and liveness instead of the packed
    /// `AtomicU128` state.
    #[test]
    fn debug_shows_the_range_not_the_packed_state() {
        use std::format;
        let t = Task::new_armed(3..9);
        let text = format!("{t:?}");
        assert!(text.contains("3..9"), "range missing from {text}");
        assert!(text.contains("alive: true"), "liveness missing from {text}");
        assert!(
            !text.contains("AtomicU128") && !text.contains("state"),
            "the packed state leaked into Debug: {text}"
        );
    }
}
