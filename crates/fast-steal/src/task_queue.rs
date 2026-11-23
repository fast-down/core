//! A concurrent work-stealing queue.
//!
//! [`TaskQueue`] holds pending and running [`Task`](crate::Task)s and lets worker
//! threads pull fresh work or steal a sub-range from a busy peer via
//! [`steal`](TaskQueue::steal). The number of running workers can be adjusted at
//! runtime with [`set_threads`](TaskQueue::set_threads).

#![allow(clippy::significant_drop_tightening)]
extern crate alloc;
use crate::{Executor, Handle, Task};
use alloc::{collections::vec_deque::VecDeque, sync::Arc, vec::Vec};
use core::{fmt, ops::Range};
// The queue's critical sections are short and synchronous (a few `VecDeque`
// operations, never held across an await), so both locks are safe. With `std`
// the queue uses `parking_lot` (parks waiting threads instead of spinning); on
// `no_std` (the `spin` feature) it uses `spin`. Exactly one backend is required.
#[cfg(feature = "std")]
use parking_lot::Mutex;
#[cfg(all(not(feature = "std"), feature = "spin"))]
use spin::Mutex;
#[cfg(not(any(feature = "std", feature = "spin")))]
compile_error!(
    "fast-steal needs a lock backend: enable the default `std` feature, or `spin` for `no_std`"
);

/// What [`TaskQueue::steal`] did with the caller's task.
///
/// Reported instead of a `bool` so the three cases stay distinguishable: a
/// worker that was never registered is a programming error, whereas a worker
/// that found no work is simply leaving. Both used to look like `false`, so a
/// broken [`Handle::is_self`](crate::Handle::is_self) was indistinguishable from
/// an empty queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use]
pub enum StealOutcome {
    /// `task` now holds fresh work and the worker is registered against it.
    Refilled,
    /// The worker was registered but nothing was stealable. It has been
    /// deregistered as a side effect (it is leaving); only
    /// [`set_threads`](TaskQueue::set_threads) registers it again.
    NoWork,
    /// The caller was registered but `task` is not its registered task, or `id`
    /// matches no registered worker. Nothing was changed.
    Unregistered,
}

/// A concurrent work-stealing queue that manages a set of [`Task`]s.
///
/// Workers created by [`Executor::execute`] call [`steal`](TaskQueue::steal) to obtain
/// new work when their current task is exhausted. The queue supports splitting,
/// speculative execution, and dynamic thread adjustment.
pub struct TaskQueue<H: Handle> {
    inner: Arc<Mutex<TaskQueueInner<H>>>,
}
/// Shows the pool sizes only, never the handles: a `Handle` implementation is
/// not required to be `Debug`, and the queue is the wrong place to print
/// runtime-specific state.
///
/// Uses `try_lock`, so formatting the queue from inside a queue operation (a
/// panic message while the lock is held, for example) reports `<locked>` instead
/// of deadlocking on a non-reentrant lock.
impl<H: Handle> fmt::Debug for TaskQueue<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut dbg = f.debug_struct("TaskQueue");
        match self.inner.try_lock() {
            Some(guard) => dbg
                .field("running", &guard.running.len())
                .field("waiting", &guard.waiting.len()),
            None => dbg.field("lock", &"<locked>"),
        };
        dbg.finish()
    }
}
impl<H: Handle> Clone for TaskQueue<H> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}
struct TaskQueueInner<H: Handle> {
    running: VecDeque<(Task, H)>,
    waiting: VecDeque<Task>,
}

impl<H: Handle> TaskQueueInner<H> {
    /// The running task with the most work left, optionally excluding `exclude`.
    fn fattest(&self, exclude: Option<&Task>) -> Option<Task> {
        self.running
            .iter()
            .map(|(task, _)| task)
            .filter(|task| task.is_alive() && exclude.is_none_or(|e| !task.same_cursor(e)))
            .max_by_key(|task| task.remain())
            .cloned()
    }
    /// Number of live registrations sharing `task`'s progress cursor.
    ///
    /// This is what the speculative cap must count. Sizing the cursor's `Arc`
    /// refcount instead would also count dead registrations, plain identity
    /// clones held by observers, and tasks parked in `waiting`, all of which
    /// inflate the count without putting another worker on the cursor.
    fn live_sharers(&self, task: &Task) -> usize {
        self.running
            .iter()
            .filter(|(registered, _)| registered.is_alive() && registered.same_cursor(task))
            .count()
    }
    /// Drops dead registrations, returning any range they had not claimed to
    /// `waiting`.
    ///
    /// The queue is the last owner of a registration's task, so dropping the
    /// entry would lose work a surviving worker could still do. Returns whether a
    /// live worker remains; after the retain every entry is live.
    fn sweep_dead(&mut self) -> bool {
        let mut reclaimed = Vec::new();
        self.running.retain(|(task, _)| {
            if task.is_alive() {
                return true;
            }
            if let Ok(Some(range)) = task.take() {
                reclaimed.push(Task::new(range));
            }
            false
        });
        if !reclaimed.is_empty() {
            self.waiting.extend(reclaimed);
        }
        !self.running.is_empty()
    }
}
impl<H: Handle> TaskQueue<H> {
    /// Creates a queue from an iterator of `start..end` ranges, each wrapped in its
    /// own [`Task`].
    ///
    /// # Panics
    /// Panics if any range is inverted (`start > end`), via [`Task::new`].
    pub fn new(tasks: impl Iterator<Item = Range<u64>>) -> Self {
        let waiting: VecDeque<_> = tasks.map(Task::new).collect();
        Self {
            inner: Arc::new(Mutex::new(TaskQueueInner {
                // `running` starts empty: pre-sizing it to `waiting.len()` would
                // allocate a slot per pending task that no worker has claimed.
                running: VecDeque::new(),
                waiting,
            })),
        }
    }
    /// Appends a [`Task`] to the waiting queue so a future
    /// [`steal`](TaskQueue::steal) or [`set_threads`](TaskQueue::set_threads) can
    /// pick it up.
    ///
    /// Before appending, dead registrations are swept and their unclaimed ranges
    /// join `waiting` (see [`steal`](TaskQueue::steal), which does the same), so
    /// an `add` after a worker panicked does not leave that worker's range
    /// stranded behind a live-looking entry.
    ///
    /// Returns `true` if at least one worker is currently registered and live
    /// (so the task will be picked up on that worker's next [`steal`](TaskQueue::steal)), or
    /// `false` if no live worker exists — in which case the task stays stranded
    /// in `waiting` until a [`set_threads`](TaskQueue::set_threads) call spawns a
    /// worker to rescue it.
    ///
    /// The return value is **best effort**: it reports whether a live worker
    /// exists at the moment of the call, not that one will call `steal` again. A
    /// worker that has already decided to exit (it found no work, or its handle
    /// was aborted) still counts, and the task then waits for `set_threads`.
    #[must_use]
    pub fn add(&self, task: Task) -> bool {
        let mut guard = self.inner.lock();
        let live = guard.sweep_dead();
        guard.waiting.push_back(task);
        live
    }
    /// Tries to refill `task` with more work for the worker identified by `id`.
    ///
    /// The caller must pass its own currently-held [`Task`] plus `id` (compared via
    /// [`Handle::is_self`](crate::Handle::is_self)). The function first hands out a
    /// pending task from the waiting queue; if none is available it steals a half
    /// range from the busiest running task via `Task::split_two`
    /// (when each half would still be at least `min_chunk_size` long), or, if
    /// `max_speculative > 1`
    /// and fewer than `max_speculative` workers already share that task's progress
    /// cursor, shares that same task speculatively.
    ///
    /// `min_chunk_size` is clamped to at least 1, so a caller passing `0` (e.g. an
    /// option that was never set) still splits down to single items rather than
    /// reaching `Task::split_two`'s zero-minimum refusal.
    ///
    /// `max_speculative` caps the number of workers that share one progress cursor.
    /// It is compared against the count of *live registrations* sharing that
    /// cursor: dead entries, observer clones and tasks parked in `waiting` do not
    /// count, because none of them is another worker executing the cursor.
    /// `<= 1` disables speculation entirely, so a peer too small to split is left
    /// alone.
    ///
    /// # Preconditions
    ///
    /// - `id` must be the id of a worker the queue registered (a handle returned
    ///   by [`Executor::execute`](crate::Executor::execute) from this queue), and
    ///   that entry must still be registered. `steal` authenticates the caller
    ///   solely through [`Handle::is_self`](crate::Handle::is_self), which it calls
    ///   under the queue lock for every registered worker; see that method's
    ///   contract for what such a predicate may do.
    /// - **`task` must be the task that worker is registered against.** This is
    ///   enforced at run time, not only under `debug_assertions`: the queue keeps
    ///   a strong `Task` per worker and replaces it on a refill, so a caller
    ///   passing any other task would overwrite another worker's registration and
    ///   strand its range. A mismatch reports
    ///   [`Unregistered`](StealOutcome::Unregistered) and changes nothing. Identity
    ///   is what counts (`same_identity`): sharing the progress cursor is not
    ///   enough, because a speculative twin has its own registration.
    /// - **`task` should be exhausted.** Every path that reports
    ///   [`Refilled`](StealOutcome::Refilled) **overwrites `*task`**, dropping the
    ///   `Task` the caller passed in — and with it the worker's identity and
    ///   liveness guard (and, for a speculative sharer, its reference to the
    ///   shared cursor). Whatever is still unclaimed in it is first taken and put
    ///   back into the queue, so a caller that skips this precondition loses no
    ///   work; it is still a misuse, because the range is handed out again while
    ///   the caller may believe it owns it.
    ///
    /// A dead registration is swept before the caller is authenticated: its
    /// unclaimed range joins `waiting`, exactly as in
    /// [`set_threads`](TaskQueue::set_threads). Without that, a worker that
    /// panicked would leave its range stranded behind an entry the liveness scan
    /// skips.
    ///
    /// Returns a [`StealOutcome`] describing the three possible outcomes:
    /// - [`Refilled`](StealOutcome::Refilled): `*task` now holds fresh work (the
    ///   previous value is overwritten, see above) and the worker's registration
    ///   is re-pointed at it. "Fresh" is what the queue handed out, not a
    ///   guarantee that work remains: a speculative twin can drain the shared
    ///   cursor before the caller looks again.
    /// - [`NoWork`](StealOutcome::NoWork): the worker was registered, but
    ///   nothing was stealable. It is *deregistered* as a side effect — finding
    ///   no work means it is leaving — and only
    ///   [`set_threads`](TaskQueue::set_threads) registers it again. `*task` is
    ///   left as the caller passed it.
    /// - [`Unregistered`](StealOutcome::Unregistered): `id` matches no
    ///   registered worker, or `task` is not that worker's registered task. E.g.
    ///   [`Handle::is_self`](crate::Handle::is_self) misidentifies its worker, a
    ///   previous `NoWork` already deregistered it, or the caller passed a peer's
    ///   task. Nothing is changed: no queue state is touched, no worker is
    ///   deregistered, no work is handed out, and `*task` keeps its previous
    ///   value.
    pub fn steal(
        &self,
        id: &H::Id,
        task: &mut Task,
        min_chunk_size: u64,
        max_speculative: usize,
    ) -> StealOutcome {
        let min_chunk_size = min_chunk_size.max(1);
        let mut guard = self.inner.lock();
        guard.sweep_dead();
        let Some(worker_idx) = guard.running.iter().position(|(_, h)| h.is_self(id)) else {
            return StealOutcome::Unregistered;
        };
        // Authenticated: only now is there a registration to validate the
        // caller's task against. An `Unregistered` caller above has already
        // returned, so it cannot disturb the queue.
        if !guard.running[worker_idx].0.same_identity(task) {
            return StealOutcome::Unregistered;
        }
        // `*task` is about to be overwritten. Hand anything it still holds back
        // to the queue so a caller that skipped the exhaustion precondition
        // cannot silently drop a range: the queue is the only other owner.
        if let Ok(Some(range)) = task.take() {
            guard.waiting.push_back(Task::new(range));
        }
        let mut found = false;
        while let Some(new_task) = guard.waiting.pop_front() {
            // Exhausted (`Ok(None)`) and corrupted (`Err`) tasks are skipped,
            // never handed to a worker.
            if let Ok(Some(range)) = new_task.take() {
                *task = Task::new_armed(range);
                found = true;
                break;
            }
        }
        if !found && let Some(steal_task) = guard.fattest(Some(task)) {
            if let Ok(Some(range)) = steal_task.split_two(min_chunk_size) {
                *task = Task::new_armed(range);
                found = true;
            } else if max_speculative > 1
                && guard.live_sharers(&steal_task) < max_speculative
                && steal_task.remain() > 0
            {
                task.share_state(&steal_task);
                found = true;
            }
        }
        if found {
            guard.running[worker_idx].0 = task.clone();
            StealOutcome::Refilled
        } else {
            guard.running.remove(worker_idx);
            StealOutcome::NoWork
        }
    }
    /// Adjusts the number of registered workers, spawning or aborting as needed.
    ///
    /// # What it actually does
    ///
    /// - **`threads` is taken literally, clamped only to at least 1.**
    ///   `set_threads(0, ..)` keeps one worker rather than pausing the pool.
    ///   Growth is fed by splitting the fattest range, so the pool stops on its
    ///   own once no range can be split under `min_chunk_size`; how many workers
    ///   a machine can actually run is the executor's call, not this queue's.
    ///   `min_chunk_size` is clamped to at least 1.
    /// - It first sweeps dead workers out of `running` (`Task::is_alive`), taking
    ///   any unclaimed range they still held back into `waiting`, then
    ///   grows or shrinks toward the requested count.
    /// - **Growth drains `waiting` with the same `take` handshake as
    ///   [`steal`](TaskQueue::steal).** Each registration has its range claimed
    ///   via `Task::take`; exhausted and corrupted ones are dropped instead of
    ///   spending a spawn slot, and registrations aliasing one cursor are
    ///   deduplicated by that claim: the first takes the remaining range, the
    ///   rest come back empty and are skipped. The worker is spawned on a fresh
    ///   task built from the claimed range, so growth never adds a second worker
    ///   to an existing cursor.
    /// - **`max_speculative` does not apply here.** It is a
    ///   [`steal`](TaskQueue::steal) parameter bounding only how many workers
    ///   `steal` may let *speculatively share* one progress cursor.
    ///   `set_threads` cannot exceed that bound because it never makes a new
    ///   worker share a cursor: waiting claims partition the cursor, and each
    ///   split off the fattest task hands out a brand-new one.
    /// - **Returning `Some(())` does not mean `threads` was reached.** Growth is
    ///   fed from `waiting` and from halves split off the fattest running task; when
    ///   both run dry (an empty `waiting` plus a single unsplittable task) it
    ///   returns `Ok` having spawned fewer workers, leaving the shortfall for a
    ///   later call to fill. It returns `None` only when growth is needed *right
    ///   now* and no `executor` was supplied.
    /// - **Shrink puts the aborted workers' unclaimed ranges back into `waiting`.**
    ///   Every worker beyond `threads` has [`Handle::abort`] called on it and
    ///   its registered `Task` is pushed onto `waiting`, so a surviving worker's
    ///   next `steal` (or the next growth) can `take` the range an aborted
    ///   worker had not claimed yet. Only *unclaimed* work is recoverable: whatever
    ///   the aborted worker had already claimed (moved the cursor past) before it
    ///   observed the abort is gone, which is exactly why [`Handle::abort`] carries
    ///   its own contract. A speculative sharer's reclaimed `Task` aliases its
    ///   surviving twin's cursor, and the shared `take` handshake partitions that
    ///   cursor safely instead of executing it twice. **Every path that drops a
    ///   registration reclaims first** — this shrink, the liveness sweep, and the
    ///   sweeps at the top of [`steal`](TaskQueue::steal) and
    ///   [`add`](TaskQueue::add) — so a worker that exited or panicked does not
    ///   take its unclaimed range with it.
    /// - **[`Executor::execute`] is called while the queue's lock is held** (both for
    ///   the `waiting` drain and for each split), so an executor must not re-enter
    ///   the queue synchronously from it.
    ///
    /// Returns `None` when growth is needed and no `executor` was supplied;
    /// `Some(())` otherwise, including when the request was already satisfied or no
    /// work was available to spawn for. The liveness sweep runs before that
    /// decision, so a `None` return may already have dropped dead registrations
    /// and moved their unclaimed ranges back to `waiting`; call again with an
    /// executor to grow.
    #[must_use]
    pub fn set_threads<E: Executor<Handle = H>>(
        &self,
        threads: usize,
        min_chunk_size: u64,
        executor: Option<&E>,
    ) -> Option<()> {
        let threads = threads.max(1);
        let min_chunk_size = min_chunk_size.max(1);
        let mut guard = self.inner.lock();
        // One scan both reclaims dead registrations and yields the live count:
        // after the sweep every entry is live, so `running.len()` is the pool
        // size to compare against `threads`.
        guard.sweep_dead();
        let len = guard.running.len();
        if len < threads {
            let executor = executor?;
            // Same filter and cursor handshake as `steal`'s waiting drain: claim
            // each range with `take`, skipping exhausted/corrupted tasks instead
            // of spending a spawn slot on them. Taking the range also dedupes
            // registrations that alias one cursor -- the first empties it, the
            // rest come back empty.
            while guard.running.len() < threads {
                let Some(task) = guard.waiting.pop_front() else {
                    break;
                };
                let Ok(Some(range)) = task.take() else {
                    continue;
                };
                // The guard has to travel with the value moved into `execute`;
                // the queue keeps an unarmed clone (see `Task::clone`) that
                // shares the identity and reads the same alive flag.
                let armed = Task::new_armed(range);
                let registered = armed.clone();
                let handle = executor.execute(armed, self.clone());
                guard.running.push_back((registered, handle));
            }
            while guard.running.len() < threads
                && let Some(steal_task) = guard.fattest(None)
                && let Ok(Some(range)) = steal_task.split_two(min_chunk_size)
            {
                let armed = Task::new_armed(range);
                let registered = armed.clone();
                let handle = executor.execute(armed, self.clone());
                guard.running.push_back((registered, handle));
            }
        } else if len > threads {
            let mut temp = Vec::with_capacity(len - threads);
            let iter = guard.running.drain(threads..);
            for (task, mut handle) in iter {
                temp.push(task);
                handle.abort();
            }
            guard.waiting.extend(temp);
        }
        Some(())
    }
    /// Aborts every running task equal to `task` that does not belong to the
    /// worker `id`.
    ///
    /// The call is a no-op unless `id` identifies a currently registered worker
    /// whose own registry entry aliases `task` (`same_cursor` compares the shared
    /// cursor, so a speculative twin counts as its peer's task). Both
    /// guards are load-bearing:
    ///
    /// - An unregistered caller matches no entry in `running`, which makes the
    ///   `is_self` guard vacuous: *every* twin would be aborted, including the
    ///   one that should survive, leaving work in `waiting` with no worker to
    ///   claim it. A caller can legitimately reach this state after a
    ///   [`set_threads`](TaskQueue::set_threads) shrink deregisters it, because
    ///   the abort that follows is cooperative and the worker keeps running
    ///   until it observes the signal.
    /// - A registered caller that passes *another* worker's `task` would abort
    ///   and deregister that peer, whose remaining range is neither reclaimed
    ///   nor kept. Requiring the caller to own the task it cancels turns such a
    ///   mismatched call into a no-op instead of letting one worker destroy
    ///   another's work.
    ///
    /// Aborted twins are *deregistered* (removed from `running`) so a later
    /// [`set_threads`](TaskQueue::set_threads) liveness sweep does not mistake
    /// them for live workers. Their remaining work is **not** reclaimed into
    /// `waiting`: an aborted twin shares the caller's cursor, so the caller's own
    /// still-live task already owns and advances whatever range is left.
    /// Reclaiming would only add a redundant `waiting` entry — the `take`
    /// handshake that `steal` applies to a shared cursor partitions it
    /// atomically, so a reclaimed twin would *not* execute the same bytes twice.
    ///
    /// # Caller obligations
    ///
    /// "Abort the twins once the shared range is finished" is *not* a safe
    /// precondition to rely on. `fast-pull`, the production caller, invokes this
    /// as soon as its own claim reaches the shared end — before it publishes
    /// (writes) that claim — so a twin may still hold a span it has claimed but
    /// not yet written. No range is lost here (the twin's cursor is shared, and
    /// its claim is gone from it), but an [`Handle::abort`] that kills the twin
    /// mid-publish *does* truncate that span, which is exactly what that method's
    /// contract forbids. A caller that aborts twins must therefore implement
    /// cooperative cancellation, or cancel only after the twins have observed the
    /// end of the cursor.
    pub fn cancel_task(&self, task: &Task, id: &H::Id) {
        let mut guard = self.inner.lock();
        // Both guards at once: a registered caller, whose own entry aliases the
        // task it claims to be cancelling twins of.
        let owns_task = guard
            .running
            .iter()
            .find(|(_, h)| h.is_self(id))
            .is_some_and(|(own, _)| own.same_cursor(task));
        if !owns_task {
            return;
        }
        // Abort every twin whose task matches but is not the caller's own, and
        // deregister it so a later liveness sweep does not count it as live.
        guard.running.retain_mut(|(own, handle)| {
            let is_twin = own.same_cursor(task) && !handle.is_self(id);
            if is_twin {
                handle.abort();
            }
            !is_twin
        });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    extern crate std;
    use crate::{Executor, Handle, StealOutcome, Task, TaskQueue};
    use std::{
        collections::{HashMap, HashSet},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        vec::Vec,
    };
    use tokio::{sync::mpsc, task::AbortHandle};

    struct TokioExecutor {
        tx: mpsc::UnboundedSender<(u64, u64)>,
        speculative: usize,
        next_id: std::sync::atomic::AtomicUsize,
    }
    #[derive(Clone)]
    struct TokioHandle {
        id: usize,
        abort: AbortHandle,
    }

    impl Handle for TokioHandle {
        type Id = usize;
        fn abort(&mut self) {
            self.abort.abort();
        }
        fn is_self(&self, id: &Self::Id) -> bool {
            self.id == *id
        }
    }

    impl Executor for TokioExecutor {
        type Handle = TokioHandle;
        fn execute(&self, mut task: Task, task_queue: TaskQueue<Self::Handle>) -> Self::Handle {
            let id = self
                .next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tx = self.tx.clone();
            let speculative = self.speculative;
            let handle = tokio::spawn(async move {
                loop {
                    // The worker's cooperative cancellation point: `abort` lands
                    // here, between claiming one unit and claiming the next, so a
                    // claimed span is always published. Without it the future
                    // never suspends and `abort` could never take effect.
                    tokio::task::yield_now().await;
                    let range = task.get();
                    if range.start >= range.end {
                        if task_queue.steal(&id, &mut task, 1, speculative)
                            != StealOutcome::Refilled
                        {
                            break;
                        }
                        continue;
                    }
                    let i = range.start;
                    let res = fib_fast(i);
                    // Another (speculative) worker may have claimed `i` first; its
                    // claim wins and this duplicate result is simply dropped.
                    if task.safe_add_start(i, 1).is_ok() {
                        tx.send((i, res)).unwrap();
                    }
                }
            });
            let abort_handle = handle.abort_handle();
            TokioHandle {
                id,
                abort: abort_handle,
            }
        }
    }

    fn fib_fast(n: u64) -> u64 {
        let mut a = 0;
        let mut b = 1;
        for _ in 0..n {
            (a, b) = (b, a + b);
        }
        a
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_task_queue() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let executor = TokioExecutor {
            tx,
            speculative: 1,
            next_id: std::sync::atomic::AtomicUsize::new(0),
        };
        let pre_data = [1..20, 41..48];
        let task_queue = TaskQueue::new(pre_data.iter().cloned());
        task_queue.set_threads(8, 1, Some(&executor)).unwrap();
        drop(executor);
        let mut data = HashMap::new();
        while let Some((i, res)) = rx.recv().await {
            assert!(
                data.insert(i, res).is_none(),
                "number {i} with value {res} was computed twice"
            );
        }
        for range in pre_data {
            for i in range {
                assert_eq!((i, data.get(&i)), (i, Some(&fib_fast(i))));
                data.remove(&i);
            }
        }
        assert_eq!(data.len(), 0);
    }

    /// End-to-end proof that shrinking the worker pool mid-run does not lose work.
    ///
    /// Eight workers are started, allowed to make progress, then the pool is cut to
    /// two. The aborted workers' in-progress tasks are reclaimed into `waiting` and
    /// picked up by the survivors via [`steal`], so every number must still be
    /// computed exactly once with no gaps.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_set_threads_decrease_keeps_all_work() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let executor = StealExecutor {
            tx,
            speculative: 1,
            next_id: Arc::new(Mutex::new(0)),
            released: released.clone(),
            steals: Arc::new(Mutex::new(0)),
        };
        let pre_data = [1..20, 41..48];
        let task_queue = TaskQueue::new(pre_data.iter().cloned());
        // Spin up 8 workers and hold them in-flight on the `released` gate, then
        // shrink to 2 mid-run. The 6 excess workers are genuinely cancelled (the
        // worker loop's `.await` point makes `abort` effective) and their
        // remaining ranges are reclaimed into `waiting`.
        task_queue.set_threads(8, 1, Some(&executor)).unwrap();
        task_queue.set_threads(2, 1, Some(&executor)).unwrap();
        // The decrease branch must have actually reduced the running pool.
        assert_eq!(task_queue.inner.lock().running.len(), 2);
        // Release the survivors so they drain `waiting` via a working `steal`
        // and finish every number exactly once.
        released.store(true, std::sync::atomic::Ordering::Relaxed);
        drop(executor);
        let mut seen = HashSet::new();
        while let Some((_, i, res)) = rx.recv().await {
            assert!(seen.insert(i), "number {i} was computed twice");
            assert_eq!(res, i);
        }
        // Every number must be present despite the mid-run shrink: the reclaimed
        // ranges were picked up by the survivors via a working `steal` (this is
        // the invariant `TokioExecutor`'s broken `is_self` could never prove).
        for range in pre_data {
            for i in range {
                assert!(seen.contains(&i), "number {i} was never computed");
            }
        }
        assert_eq!(seen.len(), 26);
    }

    /// A *correct* executor used to genuinely exercise work-stealing and
    /// mid-run reclaim. Unlike [`TokioExecutor`], its handle carries a real
    /// worker id so [`Handle::is_self`] resolves, and the worker loop has an
    /// `.await` point so [`Handle::abort`] actually cancels in-flight tasks.
    ///
    /// This is what lets a shrink truly reclaim a busy worker's remaining range
    /// into `waiting` and have the survivors pick it up via [`steal`], instead
    /// of the existing tests' brute-force completion + `safe_add_start`
    /// deduplication.
    struct StealExecutor {
        tx: mpsc::UnboundedSender<(usize, u64, u64)>,
        speculative: usize,
        next_id: Arc<Mutex<usize>>,
        released: Arc<std::sync::atomic::AtomicBool>,
        steals: Arc<Mutex<usize>>,
    }
    #[derive(Clone)]
    struct StealHandle {
        abort: AbortHandle,
        id: usize,
    }
    impl Handle for StealHandle {
        type Id = usize;
        fn abort(&mut self) {
            self.abort.abort();
        }
        fn is_self(&self, id: &usize) -> bool {
            self.id == *id
        }
    }
    impl Executor for StealExecutor {
        type Handle = StealHandle;
        fn execute(&self, mut task: Task, q: TaskQueue<Self::Handle>) -> Self::Handle {
            let id = {
                let mut g = self.next_id.lock().unwrap();
                let i = *g;
                *g += 1;
                i
            };
            let tx = self.tx.clone();
            let speculative = self.speculative;
            let released = self.released.clone();
            let steals = self.steals.clone();
            // Stay in-flight (and keep `abort` effective via the `.await` point)
            // until the test flips `released`, so a mid-run shrink sees this
            // worker as still running.
            let handle = tokio::spawn(async move {
                while !released.load(std::sync::atomic::Ordering::Relaxed) {
                    tokio::task::yield_now().await;
                }
                loop {
                    while task.start() < task.end() {
                        // Yield between numbers so a busy worker stays
                        // schedulable while its peers steal from it, instead of
                        // finishing its whole range in one uninterruptible burst.
                        tokio::task::yield_now().await;
                        let i = task.start();
                        let res = i;
                        if task.safe_add_start(i, 1).is_err() {
                            continue;
                        }
                        tx.send((id, i, res)).unwrap();
                    }
                    tokio::task::yield_now().await;
                    if q.steal(&id, &mut task, 1, speculative) != StealOutcome::Refilled {
                        break;
                    }
                    *steals.lock().unwrap() += 1;
                }
            });
            StealHandle {
                abort: handle.abort_handle(),
                id,
            }
        }
    }

    /// Genuinely verifies work-stealing: one worker gets a huge range while the
    /// other seven get single-number crumbs, so the crumb workers *must* `steal`
    /// from the busy peer to finish. With a working `steal` every crumb worker
    /// ends up computing more than its initial 1-number crumb; if `steal` were a
    /// no-op (e.g. `is_self` broken) only the single big worker does >1 number.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_steal_distributes_work() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let executor = StealExecutor {
            tx,
            speculative: 1,
            next_id: Arc::new(Mutex::new(0)),
            released: released.clone(),
            steals: Arc::new(Mutex::new(0)),
        };
        // One big task [7..1000] plus seven single-number crumbs.
        let pre_data = [0..1, 1..2, 2..3, 3..4, 4..5, 5..6, 6..7, 7..1000];
        let task_queue = TaskQueue::new(pre_data.iter().cloned());
        task_queue.set_threads(8, 1, Some(&executor)).unwrap();
        drop(executor);
        released.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut seen = std::collections::HashSet::new();
        let mut per_worker = HashMap::new();
        while let Some((wid, i, res)) = rx.recv().await {
            assert!(seen.insert(i), "number {i} computed twice");
            assert_eq!(res, i);
            *per_worker.entry(wid).or_insert(0) += 1;
        }
        assert_eq!(seen.len(), 1000, "not all numbers were computed");
        // The discriminating check: with working steal, crumb workers steal from
        // the big peer, so at least one of them ends up doing far more than its
        // initial 1-number crumb. A broken `is_self` makes `steal` a no-op (no
        // `worker_idx` is found), leaving *exactly one* worker (the big one)
        // above 1.
        //
        // The threshold is therefore `>= 2`: it is the sound invariant, not a
        // statistical guess. A working steal *always* yields at least 2 workers
        // above 1 (the big worker plus at least one stealer) — the only way to
        // land at 1 is zero steals, i.e. the broken case. Asserting a higher
        // count (>= 3) was flaky: how many crumb workers get a bite depends on
        // scheduling, and on a fast or loaded runner two hot workers can drain
        // the whole range before their peers win the race to steal,
        // legitimately leaving exactly 2 workers above 1. That is still correct
        // stealing, so it must not fail the test.
        let multi = per_worker.values().filter(|&&c| c > 1).count();
        assert!(
            multi >= 2,
            "steal did not distribute work; only {multi} workers exceeded their \
             initial crumb (per-worker counts: {per_worker:?})"
        );
    }

    /// Genuinely verifies mid-run reclaim: 8 workers split a big task, then the
    /// pool is cut to 2. The 6 aborted workers are truly cancelled (the worker
    /// loop's `.await` point makes `abort` effective) and their remaining ranges
    /// are reclaimed into `waiting`, where the 2 survivors pick them up via
    /// `steal`. If reclaim or `steal` failed, those reclaimed ranges would be
    /// lost and the count would fall short of 1000.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_set_threads_decrease_reclaims_via_steal() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let executor = StealExecutor {
            tx,
            speculative: 1,
            next_id: Arc::new(Mutex::new(0)),
            released: released.clone(),
            steals: Arc::new(Mutex::new(0)),
        };
        let task_queue = TaskQueue::new(std::iter::once(0..1000));
        task_queue.set_threads(8, 1, Some(&executor)).unwrap();
        // Workers are spinning on `released` (in-flight), so the shrink sees
        // them as still running.
        task_queue.set_threads(2, 1, Some(&executor)).unwrap();
        assert_eq!(task_queue.inner.lock().running.len(), 2);
        released.store(true, std::sync::atomic::Ordering::Relaxed);
        drop(executor);
        let mut seen = std::collections::HashSet::new();
        while let Some((_, i, res)) = rx.recv().await {
            assert!(seen.insert(i), "number {i} computed twice (reclaim failed)");
            assert_eq!(res, i);
        }
        // All 1000 must be present: the reclaimed ranges were picked up by the
        // survivors via steal. A broken `is_self` (steal no-op) loses them.
        assert_eq!(seen.len(), 1000, "reclaimed work was lost");
    }

    /// The README work-stealing example is `no_run`, so its
    /// doctest only compiles and never actually executes a steal. This mirrors
    /// that example (one big task + crumb workers, a genuine `is_self`) and
    /// asserts that steals *did* happen -- not merely that the result is correct,
    /// which static pre-slicing would also satisfy.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_readme_steal_actually_happens() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let released = Arc::new(AtomicBool::new(false));
        let steals = Arc::new(Mutex::new(0));
        let executor = StealExecutor {
            tx,
            speculative: 1,
            next_id: Arc::new(Mutex::new(0)),
            released: released.clone(),
            steals: steals.clone(),
        };
        // README pattern: one big task plus seven single-number crumbs.
        let pre_data = [0..1, 1..2, 2..3, 3..4, 4..5, 5..6, 6..7, 7..1000];
        let task_queue = TaskQueue::new(pre_data.iter().cloned());
        task_queue.set_threads(8, 1, Some(&executor)).unwrap();
        drop(executor);
        released.store(true, Ordering::Relaxed);
        let mut seen = HashSet::new();
        while let Some((_, i, res)) = rx.recv().await {
            assert!(seen.insert(i), "number {i} computed twice");
            assert_eq!(res, i);
        }
        assert_eq!(seen.len(), 1000, "not all numbers were computed");
        // The discriminating check: steals must have actually occurred.
        assert!(
            *steals.lock().unwrap() > 0,
            "no steal ever happened -- the README example would be silently broken"
        );
    }

    /// A lightweight executor used only to exercise `set_threads` bookkeeping
    /// without performing real work. It records how many workers it spawned and
    /// keeps each handed [`Task`] alive in a never-ending background task so the
    /// `running` deque stays populated and inspectable after the call returns.
    struct HoldExecutor {
        spawned: Arc<Mutex<usize>>,
        tasks: Arc<Mutex<Vec<Task>>>,
    }
    struct HoldHandle;
    impl Handle for HoldHandle {
        type Id = ();
        fn abort(&mut self) {}
        fn is_self(&self, (): &()) -> bool {
            false
        }
    }
    impl Executor for HoldExecutor {
        type Handle = HoldHandle;
        fn execute(&self, task: Task, _q: TaskQueue<Self::Handle>) -> Self::Handle {
            *self.spawned.lock().unwrap() += 1;
            // Keep the `Task` alive so `set_threads` shrink can move it back to
            // `waiting`. No real work is done and no background task is spawned,
            // so the test runtime exits cleanly.
            self.tasks.lock().unwrap().push(task);
            HoldHandle
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_set_threads_increase_spawns_exact_count() {
        let ex = HoldExecutor {
            spawned: Arc::new(Mutex::new(0)),
            tasks: Arc::new(Mutex::new(Vec::new())),
        };
        let q = TaskQueue::new(std::iter::once(0..100));
        q.set_threads(4, 1, Some(&ex)).unwrap();
        assert_eq!(q.inner.lock().running.len(), 4);
        assert_eq!(*ex.spawned.lock().unwrap(), 4);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_set_threads_decrease_aborts_and_reclaims() {
        let ex = HoldExecutor {
            spawned: Arc::new(Mutex::new(0)),
            tasks: Arc::new(Mutex::new(Vec::new())),
        };
        let q = TaskQueue::new(std::iter::once(0..100));
        q.set_threads(4, 1, Some(&ex)).unwrap();
        assert_eq!(q.inner.lock().running.len(), 4);
        // Shrink to a single worker: the other 3 must be aborted and reclaimed.
        q.set_threads(1, 1, Some(&ex)).unwrap();
        assert_eq!(q.inner.lock().running.len(), 1);
        // Tasks are reclaimed, not re-spawned: spawn count is unchanged.
        assert_eq!(*ex.spawned.lock().unwrap(), 4);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_set_threads_noop_keeps_worker_count() {
        let ex = HoldExecutor {
            spawned: Arc::new(Mutex::new(0)),
            tasks: Arc::new(Mutex::new(Vec::new())),
        };
        let q = TaskQueue::new(std::iter::once(0..100));
        q.set_threads(2, 1, Some(&ex)).unwrap();
        assert_eq!(q.inner.lock().running.len(), 2);
        // Calling again with the same count must be a no-op.
        q.set_threads(2, 1, Some(&ex)).unwrap();
        assert_eq!(q.inner.lock().running.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_set_threads_none_executor_early_return() {
        let ex = HoldExecutor {
            spawned: Arc::new(Mutex::new(0)),
            tasks: Arc::new(Mutex::new(Vec::new())),
        };
        let q = TaskQueue::new(std::iter::once(0..100));
        // Need more workers but no executor available -> early return `None`.
        assert!(q.set_threads::<HoldExecutor>(4, 1, None).is_none());
        assert_eq!(q.inner.lock().running.len(), 0);
        // Bring up 2 workers with a real executor.
        q.set_threads(2, 1, Some(&ex)).unwrap();
        assert_eq!(q.inner.lock().running.len(), 2);
        // len == threads -> no-op branch, returns `Some(())` even without executor.
        assert!(q.set_threads::<HoldExecutor>(2, 1, None).is_some());
        assert_eq!(q.inner.lock().running.len(), 2);
    }

    /// Repeated increase/decrease must never lose or duplicate a task.
    ///
    /// Fifty independent single-element ranges are used so `remain == 1` and
    /// `split_two` never fires; every move is then a pure `waiting` <-> `running`
    /// transfer. The core invariant `waiting.len() + running.len() == total` must
    /// hold after every resize, and `running` must land exactly at the requested
    /// size (clamped to what `waiting` can supply). A broken hand-off (e.g. a
    /// task dropped during reclaim, or a double-drain on increase) would break
    /// one of these assertions immediately.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_set_threads_oscillate_keeps_invariant() {
        let ex = HoldExecutor {
            spawned: Arc::new(Mutex::new(0)),
            tasks: Arc::new(Mutex::new(Vec::new())),
        };
        // 50 independent single-element tasks: remain == 1, so `split_two` is dead
        // code here and each resize is a deterministic transfer.
        let q = TaskQueue::new((0..50).map(|i| i..i + 1));
        let total = {
            let g = q.inner.lock();
            g.waiting.len() + g.running.len()
        };
        assert_eq!(total, 50);
        // Oscillate the pool size many times across increase and decrease.
        let pattern = [8usize, 2, 8, 3, 8, 1, 8, 4, 8, 2, 5, 8, 1];
        for &threads in &pattern {
            q.set_threads(threads, 1, Some(&ex)).unwrap();
            let guard = q.inner.lock();
            let running = guard.running.len();
            let waiting = guard.waiting.len();
            // No task may vanish or be double-claimed across a resize.
            assert_eq!(
                running + waiting,
                total,
                "task lost/duplicated at threads={threads}"
            );
            // `running` must match the request, clamped to the available pool.
            assert_eq!(
                running,
                threads.min(total),
                "running {running} != min({threads}, {total}) at threads={threads}"
            );
            drop(guard);
        }
        // `threads.max(1)` clamps zero to one; with `running == 1` after the
        // oscillation pattern this is a no-op, leaving the single worker alive.
        q.set_threads(0, 1, Some(&ex)).unwrap();
        let guard = q.inner.lock();
        assert_eq!(guard.running.len(), 1);
        assert_eq!(guard.waiting.len(), total - 1);
        drop(guard);
    }

    /// `add` pushes onto the waiting queue, and a live worker's `steal` then pulls
    /// that freshly-added task off `waiting` before falling back to stealing from a
    /// busy peer.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_add_then_steal_pulls_from_waiting() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let released = Arc::new(AtomicBool::new(false));
        let executor = StealExecutor {
            tx,
            speculative: 1,
            next_id: Arc::new(Mutex::new(0)),
            released: released.clone(),
            steals: Arc::new(Mutex::new(0)),
        };
        // A single tiny initial task so exactly one worker is registered and the
        // waiting queue starts empty.
        let q = TaskQueue::new(std::iter::once(0..1));
        q.set_threads(1, 1, Some(&executor)).unwrap();
        // `add` lands a brand-new task in `waiting`. A live worker
        // exists, so `add` reports `true`.
        assert!(
            q.add(Task::new(100..110)),
            "a live worker exists to pick up the added task"
        );
        assert_eq!(q.inner.lock().waiting.len(), 1);

        drop(executor);
        released.store(true, Ordering::Relaxed);

        let mut seen = HashSet::new();
        while let Some((_id, i, _res)) = rx.recv().await {
            seen.insert(i);
        }
        // The initial task ran...
        assert!(seen.contains(&0), "initial task was not executed");
        // ...and the `add`ed task was pulled from `waiting` via `steal`.
        for i in 100..110 {
            assert!(
                seen.contains(&i),
                "added task range element {i} was never stolen"
            );
        }
    }

    // ---------------------------------------------------------------------
    // Deterministic, runtime-free queue-level tests.
    //
    // The tokio executors above drive the queue end-to-end but can only observe
    // *outcomes*: which branch of `steal` runs is decided by scheduling luck.
    // `SyncExecutor` spawns nothing at all -- it hands each worker a real id and
    // parks its `Task` in a slot -- so a test can call `steal` / `cancel_task` /
    // `set_threads` directly from the test thread and assert on the queue's
    // internal bookkeeping with zero races.
    // ---------------------------------------------------------------------

    struct SyncExecutor {
        /// One slot per spawned worker. `None` means that worker has exited and
        /// released the strong reference it held on its task.
        slots: Arc<Mutex<Vec<Option<Task>>>>,
        aborted: Arc<Mutex<Vec<usize>>>,
    }
    struct SyncHandle {
        id: usize,
        aborted: Arc<Mutex<Vec<usize>>>,
    }
    impl Handle for SyncHandle {
        type Id = usize;
        fn abort(&mut self) {
            self.aborted.lock().unwrap().push(self.id);
        }
        fn is_self(&self, id: &usize) -> bool {
            self.id == *id
        }
    }
    impl Executor for SyncExecutor {
        type Handle = SyncHandle;
        fn execute(&self, task: Task, _q: TaskQueue<Self::Handle>) -> Self::Handle {
            // Locks a mutex that is *not* the queue's, honouring the "never
            // re-enter TaskQueue from execute" contract documented in executor.rs.
            let id = {
                let mut slots = self.slots.lock().unwrap();
                let id = slots.len();
                slots.push(Some(task));
                id
            };
            SyncHandle {
                id,
                aborted: self.aborted.clone(),
            }
        }
    }
    impl SyncExecutor {
        fn new() -> Self {
            Self {
                slots: Arc::new(Mutex::new(Vec::new())),
                aborted: Arc::new(Mutex::new(Vec::new())),
            }
        }
        /// The task worker `id` currently holds, as a state-sharing clone.
        fn task_of(&self, id: usize) -> Task {
            self.slots.lock().unwrap()[id].clone().unwrap()
        }
        /// The task worker `id` holds after draining its range, as a
        /// state-sharing clone.
        ///
        /// `steal` requires the caller's task to be exhausted, and a real worker
        /// only calls it after claiming everything it had -- leaving the cursor
        /// at `end..end`, not at `start..start`. Draining here the same way keeps
        /// the clone's identity while reproducing that state.
        fn drained_task_of(&self, id: usize) -> Task {
            let task = self.task_of(id);
            let range = task.get();
            assert!(range.start < range.end, "worker {id} had no work to drain");
            task.safe_add_start(range.start, range.end - range.start)
                .expect("range invariant holds");
            assert_eq!(task.get(), range.end..range.end, "worker {id} not drained");
            task
        }
        /// Mirror a real worker's local `task` variable being replaced by `steal`.
        fn rebind(&self, id: usize, task: &Task) {
            self.slots.lock().unwrap()[id] = Some(task.clone());
        }
        /// Simulate worker `id` exiting: it drops its strong reference.
        fn kill(&self, id: usize) {
            self.slots.lock().unwrap()[id] = None;
        }
        fn live_workers(&self) -> usize {
            self.slots.lock().unwrap().iter().flatten().count()
        }
        fn aborted(&self) -> Vec<usize> {
            self.aborted.lock().unwrap().clone()
        }
    }

    /// A task whose range invariant is broken (`start > end`), built through the
    /// raw state field because `Task::new` would (correctly) refuse to make one.
    fn corrupted_task() -> Task {
        Task::from_state(Arc::new(portable_atomic::AtomicU128::new(
            (20u128 << 64) | 0xA,
        )))
    }

    /// `Clone` is hand-written rather than derived (a derive would wrongly demand
    /// `H: Clone`). It must alias the shared inner state, not copy it.
    #[test]
    fn clone_shares_the_same_inner_queue() {
        let q: TaskQueue<SyncHandle> = TaskQueue::new(core::iter::empty());
        let q2 = q.clone();
        let _ = q2.add(Task::new(0..5));
        assert_eq!(q.inner.lock().waiting.len(), 1);
        assert!(Arc::ptr_eq(&q.inner, &q2.inner));
    }

    /// `TaskQueue::new` inherits `Task::from`'s panic contract.
    #[test]
    #[should_panic(expected = "range.start <= range.end")]
    fn new_inherits_the_reversed_range_panic() {
        // Inline `10..5` would trip `clippy::reversed_empty_ranges`.
        let bad = core::ops::Range {
            start: 10u64,
            end: 5u64,
        };
        let _: TaskQueue<SyncHandle> = TaskQueue::new(core::iter::once(bad));
    }

    /// `add` only appends, so with no live worker the task is stranded until an
    /// explicit `set_threads` runs. The same test pins that `set_threads` reports
    /// success even when it spawned nothing because there was no work to hand out.
    #[test]
    fn add_is_inert_without_a_live_worker() {
        let ex = SyncExecutor::new();
        let q: TaskQueue<SyncHandle> = TaskQueue::new(core::iter::empty());
        assert!(q.set_threads(4, 1, Some(&ex)).is_some());
        assert_eq!(q.inner.lock().running.len(), 0, "nothing was spawned");

        assert!(
            !q.add(Task::new(0..100)),
            "no live worker: the task is stranded"
        );
        assert_eq!(q.inner.lock().waiting.len(), 1);
        // No worker exists to call `steal`, so only an explicit `set_threads`
        // ever rescues the task.
        q.set_threads(1, 1, Some(&ex)).unwrap();
        let guard = q.inner.lock();
        assert_eq!(guard.running.len(), 1);
        assert_eq!(guard.waiting.len(), 0);
        drop(guard);
    }

    /// The `true` branch of `add`: when at least one live worker exists,
    /// `add` reports `true` (a worker can pick the new task up via `steal`).
    #[test]
    fn add_returns_true_when_a_live_worker_exists() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new(core::iter::once(0..1));
        q.set_threads(1, 1, Some(&ex)).unwrap();
        assert_eq!(q.inner.lock().running.len(), 1, "one live worker spawned");
        assert!(
            q.add(Task::new(5..6)),
            "a live worker exists to pick up the new task"
        );
    }

    /// Liveness is the registration's non-`Clone` guard, not the identity's
    /// strong count: a `Task::clone` held by an observer outlives the worker but
    /// must not keep it alive.
    #[test]
    fn external_task_clone_does_not_keep_an_exited_worker_alive() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new(core::iter::once(0..10));
        q.set_threads(1, 1, Some(&ex)).unwrap();

        // An observer's clone: it shares the identity but carries no guard.
        let _observer = ex.task_of(0);
        assert!(q.inner.lock().running[0].0.is_alive());

        // The worker exits: the executor drops the armed task it was handed.
        ex.kill(0);
        assert!(
            !q.inner.lock().running[0].0.is_alive(),
            "an observer's clone must not keep the exited worker alive"
        );

        // The liveness sweep agrees and collects the dead entry, then grows back
        // to one worker on the reclaimed range.
        q.set_threads(1, 1, Some(&ex)).unwrap();
        let guard = q.inner.lock();
        assert_eq!(guard.running.len(), 1);
        assert!(
            guard.running[0].0.is_alive(),
            "the dead entry was replaced, not kept"
        );
        drop(guard);
    }

    /// `add`'s return value tracks real liveness, not strong counts: with the
    /// worker gone and only an observer's clone left, no worker will pick the
    /// task up and `add` must say so.
    #[test]
    fn add_reports_real_liveness_not_a_lingering_clone() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new(core::iter::once(0..10));
        q.set_threads(1, 1, Some(&ex)).unwrap();
        let _observer = ex.task_of(0);
        assert!(
            q.add(Task::new(100..110)),
            "the registered worker is alive and will pick the task up"
        );

        ex.kill(0);
        assert!(
            !q.add(Task::new(200..210)),
            "a clone outliving the worker must not count as a live worker"
        );
        // `add` also sweeps the dead registration, handing its range back.
        let guard = q.inner.lock();
        assert!(guard.running.is_empty(), "the dead worker was reaped");
        assert_eq!(
            guard.waiting.len(),
            3,
            "the first add, the dead worker's range, and the second add all wait"
        );
        assert_eq!(guard.waiting[1].get(), 0..10, "the reaped range is queued");
    }

    /// An unregistered caller is reported as [`StealOutcome::Unregistered`] — not
    /// as the [`StealOutcome::NoWork`] a registered worker gets on an empty queue
    /// — and nothing is consumed from `waiting`.
    ///
    /// The task passed here still has all its work left on purpose: the
    /// exhausted-task and registration assertions only run *after*
    /// authentication, so an unregistered caller must not trip them in debug
    /// builds.
    #[test]
    fn steal_rejects_an_unregistered_worker() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new(core::iter::once(0..100));
        q.set_threads(1, 1, Some(&ex)).unwrap();
        let _ = q.add(Task::new(500..510));
        let mut t = Task::new(0..10);
        assert_eq!(
            q.steal(&999, &mut t, 1, 1),
            StealOutcome::Unregistered,
            "a caller that was never registered is a programming error"
        );
        assert_eq!(t.get(), 0..10);
        assert_eq!(
            q.inner.lock().waiting.len(),
            1,
            "waiting must not be disturbed by an unknown caller"
        );
    }

    /// A registered caller may only refill *its own* registered task. Passing a
    /// peer's task must be rejected at run time, in every build: a debug-only
    /// assertion would let a release build overwrite the peer's registration and
    /// strand its range.
    #[test]
    fn steal_rejects_a_task_registered_to_another_worker() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..10, 100..200].into_iter());
        q.set_threads(2, 1, Some(&ex)).unwrap();
        // Worker 1's task, drained so only the identity check can fire; the
        // mismatch is purely in who the task is registered to.
        let mut t = ex.drained_task_of(1);
        assert_eq!(
            q.steal(&0, &mut t, 1, 1),
            StealOutcome::Unregistered,
            "a caller may not refill with a peer's registration"
        );
        assert_eq!(t.get(), 200..200, "the rejected task is untouched");
        let guard = q.inner.lock();
        assert_eq!(guard.running.len(), 2, "nobody was deregistered");
        assert_eq!(
            guard.running[0].0.get(),
            0..10,
            "the caller's registration was not overwritten"
        );
        assert_eq!(guard.waiting.len(), 0, "no work was handed out");
    }

    /// The three outcomes are distinct: `NoWork` deregisters the registered worker
    /// that found nothing, whereas `Unregistered` changes nothing at all.
    #[test]
    fn steal_outcome_separates_no_work_from_unregistered() {
        // Empty queue: the registered worker gets `NoWork` and is deregistered.
        let ex = SyncExecutor::new();
        let q = TaskQueue::new(core::iter::once(0..1));
        q.set_threads(1, 1, Some(&ex)).unwrap();
        let mut t = ex.drained_task_of(0);
        assert_eq!(q.steal(&0, &mut t, 1, 1), StealOutcome::NoWork);
        assert_eq!(
            q.inner.lock().running.len(),
            0,
            "`NoWork` deregisters the worker that found nothing"
        );

        // An id no handle recognises: `Unregistered`, with the queue untouched --
        // the live worker stays registered and the waiting work stays available.
        let ex = SyncExecutor::new();
        let q = TaskQueue::new(core::iter::once(0..1));
        q.set_threads(1, 1, Some(&ex)).unwrap();
        let _ = q.add(Task::new(100..200));
        let mut t = Task::new(0..0);
        assert_eq!(q.steal(&999, &mut t, 1, 1), StealOutcome::Unregistered);
        assert_eq!(
            q.inner.lock().running.len(),
            1,
            "`Unregistered` deregisters nobody"
        );
        assert_eq!(
            q.inner.lock().waiting.len(),
            1,
            "`Unregistered` consumes no work"
        );
        assert_eq!(t.get(), 0..0);
    }

    #[test]
    fn steal_prefers_waiting_over_robbing_a_peer() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..2, 100..200].into_iter());
        q.set_threads(2, 1, Some(&ex)).unwrap();
        let _ = q.add(Task::new(500..510));
        let mut t = ex.drained_task_of(0);
        assert_eq!(q.steal(&0, &mut t, 1, 1), StealOutcome::Refilled);
        assert_eq!(t.get(), 500..510);
        assert_eq!(
            ex.task_of(1).get(),
            100..200,
            "the fat peer keeps its range: waiting wins over split_two"
        );
        // The worker is re-registered against its brand-new task.
        assert!(q.inner.lock().running[0].0.same_cursor(&t));
    }

    /// The waiting drain loop pops until it finds *usable* work: an exhausted task
    /// yields `Ok(None)` and a corrupted one yields `Err`; both are discarded
    /// rather than handed to a worker.
    #[test]
    fn steal_skips_exhausted_and_corrupted_waiting_tasks() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new(core::iter::once(0..2));
        q.set_threads(1, 1, Some(&ex)).unwrap();
        let _ = q.add(Task::new(7..7));
        let _ = q.add(corrupted_task());
        let _ = q.add(Task::new(500..510));
        let mut t = ex.drained_task_of(0);
        assert_eq!(q.steal(&0, &mut t, 1, 1), StealOutcome::Refilled);
        assert_eq!(t.get(), 500..510);
        assert_eq!(
            q.inner.lock().waiting.len(),
            0,
            "all three were popped; the two unusable ones are dropped"
        );
    }

    /// A lone worker that has drained its range cannot be refilled from itself.
    ///
    /// Under the exhausted-task precondition the caller's own task has no work
    /// left, so self-exclusion is no longer *separately* observable: a
    /// zero-remain task is unsplittable and unsharable whether or not the victim
    /// scan filters it out. What this pins is the outcome -- `NoWork`, never a
    /// phantom refill with the caller's own cursor -- plus deregistration.
    #[test]
    fn steal_excludes_the_caller_as_a_victim() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new(core::iter::once(0..1000));
        q.set_threads(1, 1, Some(&ex)).unwrap();
        let mut t = ex.drained_task_of(0);
        assert_eq!(q.steal(&0, &mut t, 1, 2), StealOutcome::NoWork);
        assert_eq!(
            t.get(),
            1000..1000,
            "the caller's own drained task is not refilled"
        );
        assert_eq!(
            q.inner.lock().running.len(),
            0,
            "a worker that found no work is deregistered"
        );
    }

    /// A caller that passes a task with work still left must not lose it: the
    /// refill overwrites `*task`, so the unclaimed range is taken and handed back
    /// to the queue first.
    #[test]
    fn steal_reclaims_a_callers_unclaimed_range() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..10, 100..200].into_iter());
        q.set_threads(2, 1, Some(&ex)).unwrap();
        // Worker 1 still owns all of 100..200.
        let mut t = ex.task_of(1);
        assert_eq!(q.steal(&1, &mut t, 1, 1), StealOutcome::Refilled);
        // The range it held came back through `waiting` and was handed to the
        // same worker, so none of it was dropped.
        assert_eq!(t.get(), 100..200);
        assert_eq!(
            q.inner.lock().waiting.len(),
            0,
            "the reclaimed range was handed out, not duplicated"
        );
        assert_eq!(
            ex.task_of(1).get(),
            100..100,
            "the old cursor was drained by the reclaim, so nobody re-executes it"
        );
    }

    #[test]
    fn steal_splits_the_busiest_peer() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..2, 10..14, 100..200].into_iter());
        q.set_threads(3, 1, Some(&ex)).unwrap();
        let mut t = ex.drained_task_of(0);
        assert_eq!(q.steal(&0, &mut t, 1, 1), StealOutcome::Refilled);
        // 100..200 has the most work left, so it -- not 10..14 -- is halved.
        assert_eq!(t.get(), 150..200);
        assert_eq!(ex.task_of(2).get(), 100..150);
        assert_eq!(
            ex.task_of(1).get(),
            10..14,
            "the smaller peer is left alone"
        );
    }

    /// When the fattest peer is too small to halve (`remain < min_chunk_size * 2`)
    /// the queue falls back to *sharing* it -- but only with speculation enabled.
    ///
    /// The two cases are tested separately because a worker that exhausts its
    /// task and finds no stealable work is deregistered from `running` (so a
    /// concurrent shrink does not mistakenly wait for it). In production a
    /// worker exits after its first `steal` reports anything but
    /// [`StealOutcome::Refilled`]; retrying with different parameters on a
    /// deregistered worker is a test-only scenario.
    #[test]
    fn steal_shares_speculatively_only_when_allowed() {
        // Case 1: speculation disabled -> the crumb is left alone.
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..1, 100..101].into_iter());
        q.set_threads(2, 1, Some(&ex)).unwrap();
        let mut t = ex.drained_task_of(0);
        assert_eq!(q.steal(&0, &mut t, 1, 1), StealOutcome::NoWork);
        assert_eq!(
            ex.task_of(1).get(),
            100..101,
            "the crumb is left alone when speculation is off"
        );

        // Case 2: speculation enabled -> the caller aliases the peer's state.
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..1, 100..101].into_iter());
        q.set_threads(2, 1, Some(&ex)).unwrap();
        let mut t = ex.drained_task_of(0);
        assert_eq!(q.steal(&0, &mut t, 1, 2), StealOutcome::Refilled);
        assert_eq!(t.get(), 100..101);
        assert!(
            t.same_cursor(&ex.task_of(1)),
            "speculation must alias, not copy"
        );
    }

    /// The speculation cap limits how many *live registrations* share one cursor.
    /// The cap `live_sharers() < max_speculative` admits a new sharer only while
    /// fewer than `max_speculative` live workers already alias that cursor,
    /// keeping the total at `max_speculative`.
    #[test]
    fn steal_caps_the_number_of_speculative_sharers() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..1, 1..2, 2..3].into_iter());
        q.set_threads(3, 1, Some(&ex)).unwrap();
        // All crumbs tie on `remain`, and `max_by_key` documents that ties resolve
        // to the *last* element, so worker 0 joins worker 2.
        let mut t0 = ex.drained_task_of(0);
        assert_eq!(q.steal(&0, &mut t0, 1, 2), StealOutcome::Refilled);
        ex.rebind(0, &t0);
        assert!(t0.same_cursor(&ex.task_of(2)));
        // Worker 1 tries to become the third sharer of that same crumb and is
        // refused: the live sharer count has reached the cap.
        let mut t1 = ex.drained_task_of(1);
        assert_eq!(q.steal(&1, &mut t1, 1, 2), StealOutcome::NoWork);
        let target = ex.task_of(2);
        let guard = q.inner.lock();
        let sharers = guard
            .running
            .iter()
            .filter(|(task, _)| task.is_alive() && task.same_cursor(&target))
            .count();
        assert_eq!(
            sharers, 2,
            "the refused worker must not become a third sharer"
        );
    }

    /// `min_chunk_size * 2` used to saturate, so a huge caller-supplied value
    /// (forwarded by `fast-pull` from `options.min_chunk_size`) skipped the split
    /// instead of panicking or wrapping.
    #[test]
    fn steal_skips_split_on_a_huge_min_chunk_size() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..2, 100..200].into_iter());
        q.set_threads(2, 1, Some(&ex)).unwrap();
        let mut t = ex.drained_task_of(0);
        // No panic, no silent disable: the split branch is simply never taken
        // (`remain / 2 < min`), so the fat peer keeps its whole range.
        assert_eq!(q.steal(&0, &mut t, u64::MAX, 1), StealOutcome::NoWork);
        assert_eq!(
            ex.task_of(1).get(),
            100..200,
            "the peer is untouched when the split is skipped"
        );
    }

    /// The same threshold in `set_threads`'s split-to-grow loop refuses a huge
    /// `min_chunk_size`, so the branch is skipped rather than panicking.
    #[test]
    fn set_threads_skips_split_on_a_huge_min_chunk_size() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new(core::iter::once(0..100));
        assert!(q.set_threads(2, u64::MAX, Some(&ex)).is_some());
        // The single waiting task is still spawned, but no extra split worker is
        // created because `remain / 2 < min` is true.
        assert_eq!(q.inner.lock().running.len(), 1);
    }

    /// Growth must apply the same `take` filter as `steal`: an exhausted (or
    /// corrupted) registration in `waiting` must not consume a spawn slot. With
    /// `[empty, empty, real]` and room for three workers, the real range still
    /// gets a worker and the pool fills by splitting it -- never by spawning
    /// workers on the unusable registrations.
    #[test]
    fn set_threads_growth_skips_empty_and_corrupted_waiting_tasks() {
        let ex = SyncExecutor::new();
        let q: TaskQueue<SyncHandle> = TaskQueue::new(core::iter::empty());
        let _ = q.add(Task::new(7..7));
        let _ = q.add(corrupted_task());
        let _ = q.add(Task::new(500..510));
        q.set_threads(3, 1, Some(&ex)).unwrap();

        let guard = q.inner.lock();
        assert_eq!(guard.running.len(), 3, "the pool is filled");
        assert_eq!(guard.waiting.len(), 0, "the real task was consumed");
        let mut ranges: Vec<_> = guard.running.iter().map(|(task, _)| task.get()).collect();
        drop(guard);
        ranges.sort_by_key(|r| r.start);
        assert!(
            ranges.iter().all(|r| r.start < r.end),
            "growth spent spawn slots on empty tasks: {ranges:?}"
        );
        // The real work is still represented, partitioned across the pool.
        assert_eq!(ranges.first().unwrap().start, 500);
        assert_eq!(ranges.last().unwrap().end, 510);
    }

    /// A shrink puts aborted workers' tasks back into `waiting`, and a later
    /// grow must not spawn one worker per reclaimed registration: several of
    /// them may alias one cursor (a speculative sharer reclaimed while its twin
    /// survives). Growth applies the same `take` handshake, so the reclaimed
    /// cursor is claimed once and the new worker gets a fresh cursor.
    /// A shrink+grow round trip therefore never puts two workers on one cursor
    /// -- the property `max_speculative` bounds for `steal`.
    #[test]
    fn set_threads_shrink_grow_never_duplicates_a_cursor() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..5, 5..10].into_iter());
        // `min_chunk_size = 10` makes both 5-wide ranges unsplittable, so worker
        // 0 can only join worker 1's cursor speculatively.
        q.set_threads(2, 10, Some(&ex)).unwrap();
        let mut t0 = ex.drained_task_of(0);
        assert_eq!(q.steal(&0, &mut t0, 10, 2), StealOutcome::Refilled);
        ex.rebind(0, &t0);
        assert!(
            t0.same_cursor(&ex.task_of(1)),
            "worker 0 aliases worker 1's cursor"
        );

        // Shrink to 1: worker 1 is aborted and its task reclaimed into `waiting`,
        // still aliasing the survivor's cursor.
        q.set_threads(1, 10, Some(&ex)).unwrap();
        assert_eq!(q.inner.lock().running.len(), 1);
        assert_eq!(q.inner.lock().waiting.len(), 1);

        // Grow back. The reclaimed alias must be drained with `take`, not spawned
        // as a second sharer of the survivor's cursor.
        q.set_threads(2, 10, Some(&ex)).unwrap();
        let tasks: Vec<_> = {
            let guard = q.inner.lock();
            assert_eq!(guard.running.len(), 2);
            guard.running.iter().map(|(task, _)| task.clone()).collect()
        };
        for (i, a) in tasks.iter().enumerate() {
            for b in &tasks[i + 1..] {
                assert!(
                    !a.same_cursor(b),
                    "shrink+grow spawned two workers on one cursor, \
                     bypassing the speculative-sharing cap"
                );
            }
        }
    }

    /// Baseline for the next test: with no sharing, one worker owns one task, so
    /// the liveness sweep collects its slot as soon as it exits.
    #[test]
    fn running_sweep_collects_an_exited_worker() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..1, 100..101].into_iter());
        q.set_threads(2, 1, Some(&ex)).unwrap();
        assert_eq!(q.inner.lock().running.len(), 2);
        ex.kill(0);
        // Shrinking to the one live worker needs no executor, and the call has
        // real work to do (drop the dead entry), so `None` is not an early return.
        let _ = q.set_threads::<SyncExecutor>(1, 1, None);
        assert_eq!(q.inner.lock().running.len(), 1, "the dead slot is swept");
    }

    /// A dead worker's unclaimed range goes back to `waiting` when the sweep
    /// collects its slot: the queue is the last owner of that task, so dropping
    /// the entry would lose work a surviving worker could still do.
    #[test]
    fn running_sweep_reclaims_a_dead_workers_range() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..10, 100..110].into_iter());
        q.set_threads(2, 1, Some(&ex)).unwrap();

        // Worker 0 exits without claiming anything.
        ex.kill(0);
        // Shrinking to the one live worker needs no executor, so `None` still
        // performs the sweep instead of early-returning.
        let _ = q.set_threads::<SyncExecutor>(1, 1, None);

        let guard = q.inner.lock();
        assert_eq!(guard.running.len(), 1, "the dead slot is swept");
        assert_eq!(guard.waiting.len(), 1, "its range came back");
        assert_eq!(guard.waiting[0].get(), 0..10);
    }

    /// The liveness sweep keys off the armed identity's alive flag, so a dead
    /// worker is reclaimed even while a twin still shares its cursor.
    #[test]
    fn speculative_sharing_no_longer_defeats_liveness_sweep() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..1, 100..101].into_iter());
        q.set_threads(2, 1, Some(&ex)).unwrap();

        let mut t0 = ex.drained_task_of(0);
        assert_eq!(q.steal(&0, &mut t0, 1, 2), StealOutcome::Refilled);
        ex.rebind(0, &t0);
        // Both `running` entries now share one and the same cursor.
        let guard = q.inner.lock();
        assert!(guard.running[0].0.same_cursor(&guard.running[1].0));
        drop(guard);

        // Worker 0 exits, leaving worker 1 as the only live worker.
        ex.kill(0);
        drop(t0);
        assert_eq!(ex.live_workers(), 1);

        // The liveness sweep alone must reclaim the dead worker's slot, even
        // though its speculative twin still references the shared cursor. The
        // request is the one *live* worker, so no executor is needed and the call
        // is not an early return.
        let _ = q.set_threads::<SyncExecutor>(1, 1, None);
        assert_eq!(
            q.inner.lock().running.len(),
            1,
            "dead worker reclaimed despite its speculative twin"
        );
    }

    /// `cancel_task` is the speculation cleanup path: when one sharer finishes the
    /// shared range it aborts the others, sparing the caller.
    ///
    /// The caller must own the task it passes. Worker 1 becomes a genuine twin by
    /// aliasing worker 0's cursor: its own crumb cannot be split under the large
    /// `min_chunk_size`, so `steal` takes the speculative-share branch.
    #[test]
    fn cancel_task_aborts_twins_but_spares_the_caller() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..1, 100..101].into_iter());
        q.set_threads(2, 1, Some(&ex)).unwrap();

        // Worker 1 shares worker 0's cursor rather than stealing a half range.
        let mut t1 = ex.drained_task_of(1);
        assert_eq!(q.steal(&1, &mut t1, 10, 2), StealOutcome::Refilled);
        ex.rebind(1, &t1);
        assert!(
            t1.same_cursor(&ex.task_of(0)),
            "worker 1 aliases worker 0's cursor"
        );

        // Worker 1 finished the shared range and cancels its twins.
        q.cancel_task(&t1, &1);
        assert_eq!(ex.aborted(), [0], "only the peer sharer is aborted");
        // The aborted twin is now deregistered, leaving only the
        // caller's own entry. (Previously it lingered in `running`.)
        assert_eq!(
            q.inner.lock().running.len(),
            1,
            "aborted twin is deregistered"
        );
    }

    /// `cancel_task` aborts and deregisters the twin but does **not** reclaim the
    /// remaining range into `waiting`: the caller's still-live task already owns
    /// that cursor, so reclaiming would double-execute. Called before the shared
    /// range is finished (i.e. on live work) it loses nothing, because the caller
    /// keeps the whole remainder.
    #[test]
    fn cancel_task_does_not_reclaim_unfinished_work() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..1, 100..200].into_iter());
        q.set_threads(2, 1, Some(&ex)).unwrap();

        // Worker 0 aliases worker 1's 100..200 cursor instead of splitting it:
        // `min_chunk_size = 60` makes `remain < min_chunk_size * 2`, so
        // `split_two` refuses and the speculative-share branch runs.
        let mut t0 = ex.drained_task_of(0);
        assert_eq!(q.steal(&0, &mut t0, 60, 2), StealOutcome::Refilled);
        ex.rebind(0, &t0);
        assert!(
            t0.same_cursor(&ex.task_of(1)),
            "worker 0 aliases worker 1's cursor"
        );
        assert_eq!(t0.remain(), 100);

        // Worker 0 cancels its twin while the shared range is still unfinished.
        q.cancel_task(&t0, &0);
        assert_eq!(ex.aborted(), [1]);
        assert_eq!(
            q.inner.lock().running.len(),
            1,
            "aborted victim is deregistered"
        );
        assert_eq!(
            q.inner.lock().waiting.len(),
            0,
            "100 units of unfinished work were intentionally NOT reclaimed (would double-execute)"
        );
        assert_eq!(
            t0.remain(),
            100,
            "the caller's cursor still owns every remaining unit"
        );
    }

    /// A registered worker that passes *another* worker's task must not be able to
    /// cancel it.
    ///
    /// Without the ownership guard, `retain_mut` aborts and deregisters that peer
    /// on the caller's behalf, and the peer's remaining range is neither
    /// reclaimed into `waiting` nor kept by anyone: it is simply lost.
    #[test]
    fn cancel_task_ignores_a_peer_task_passed_by_a_registered_caller() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..1, 100..200].into_iter());
        q.set_threads(2, 1, Some(&ex)).unwrap();

        let peer = ex.task_of(1);
        assert_eq!(peer.remain(), 100);
        // Worker 0 does not own `peer`'s cursor, so this must be a complete no-op.
        q.cancel_task(&peer, &0);

        assert!(
            ex.aborted().is_empty(),
            "a caller that does not own the task must not abort its owner"
        );
        assert_eq!(q.inner.lock().running.len(), 2, "no worker deregistered");
        assert_eq!(q.inner.lock().waiting.len(), 0, "nothing was reclaimed");
        assert_eq!(
            ex.task_of(1).remain(),
            100,
            "the peer keeps its whole range"
        );
    }

    /// An unregistered caller must not abort anyone via `cancel_task`.
    ///
    /// Without the registration guard a caller deregistered by a shrink (but still
    /// running cooperatively) makes `is_self` vacuous -- `false` for every entry --
    /// and aborts *all* twins, including the worker meant to drain `waiting`.
    #[test]
    fn cancel_task_unregistered_caller_is_a_noop() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..1, 100..200].into_iter());
        q.set_threads(2, 1, Some(&ex)).unwrap();
        // id 999 is not in `running`, so the call must be a no-op.
        q.cancel_task(&ex.task_of(1), &999);
        assert!(
            ex.aborted().is_empty(),
            "unregistered caller must not abort anyone"
        );
        assert_eq!(q.inner.lock().running.len(), 2, "no worker deregistered");
    }

    /// `retain` at the top of `set_threads` removes every dead entry first, so the
    /// shrink path always operates on live survivors and needs no separate
    /// liveness guard.
    #[test]
    fn set_threads_shrink_guard_never_fires_after_retain() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new((0..5).map(|i| i * 10..(i + 1) * 10));
        q.set_threads(5, 1, Some(&ex)).unwrap();
        assert_eq!(q.inner.lock().running.len(), 5);

        // Kill the first 3 workers. A naive reading of the guard suggests
        // shrink-to-2 might early-return (first 2 are dead). But `retain`
        // removes them first, leaving [alive3, alive4], so shrink proceeds.
        ex.kill(0);
        ex.kill(1);
        ex.kill(2);

        q.set_threads(2, 1, Some(&ex)).unwrap();
        let guard = q.inner.lock();
        assert_eq!(
            guard.running.len(),
            2,
            "retain sweeps dead entries; shrink always proceeds"
        );
        // The 2 survivors' tasks were NOT aborted.
        assert!(ex.aborted().is_empty() || ex.aborted().iter().all(|&id| id < 3));
    }

    /// A worker deregistered by a failed `steal` (no work found) is rejected
    /// on all subsequent steal attempts — even after new work is added to
    /// `waiting`. Only `set_threads` can re-register a worker.
    #[test]
    fn steal_after_deregistration_is_permanently_rejected() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new(core::iter::once(0..1));
        q.set_threads(1, 1, Some(&ex)).unwrap();

        let mut t = ex.drained_task_of(0);
        // No work anywhere: steal reports `NoWork` and deregisters worker 0.
        assert_eq!(q.steal(&0, &mut t, 1, 1), StealOutcome::NoWork);
        assert_eq!(q.inner.lock().running.len(), 0, "worker deregistered");

        // Add fresh work to waiting.
        let _ = q.add(Task::new(100..200));
        assert_eq!(q.inner.lock().waiting.len(), 1);

        // The deregistered worker still cannot steal -- and now says so
        // explicitly instead of reporting the same `NoWork` as an empty queue.
        assert_eq!(q.steal(&0, &mut t, 1, 1), StealOutcome::Unregistered);
        assert_eq!(
            q.inner.lock().waiting.len(),
            1,
            "waiting undisturbed by rejected caller"
        );

        // Only set_threads can rescue the stranded work.
        q.set_threads(1, 1, Some(&ex)).unwrap();
        assert_eq!(q.inner.lock().running.len(), 1);
        assert_eq!(q.inner.lock().waiting.len(), 0);
    }

    /// Multiple registered workers calling `steal` concurrently from
    /// different OS threads: the mutex serialises access, so every task from
    /// `waiting` is handed out exactly once with no loss or duplication.
    #[test]
    fn concurrent_steal_from_waiting_no_loss_or_dup() {
        use std::thread;

        let ex = SyncExecutor::new();
        let q = TaskQueue::new((0..4).map(|i| i * 100..(i + 1) * 100));
        q.set_threads(4, 1, Some(&ex)).unwrap();

        // Add 4 fresh tasks to waiting.
        for i in 0..4u64 {
            let _ = q.add(Task::new(1000 + i * 100..1100 + i * 100));
        }

        let mut handles = Vec::new();
        for id in 0..4usize {
            let q = q.clone();
            let mut t = ex.drained_task_of(id);
            handles.push(thread::spawn(move || {
                let outcome = q.steal(&id, &mut t, 1, 1);
                (id, outcome, t.get())
            }));
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // All 4 succeed (4 waiting tasks available).
        let stolen: Vec<_> = results
            .iter()
            .filter(|(_, outcome, _)| *outcome == StealOutcome::Refilled)
            .map(|(_, _, r)| r.clone())
            .collect();
        assert_eq!(stolen.len(), 4, "all 4 workers got work from waiting");

        // Ranges are non-overlapping and cover exactly 1000..1400.
        let mut sorted = stolen;
        sorted.sort_by_key(|r| r.start);
        for w in sorted.windows(2) {
            assert!(
                w[0].end <= w[1].start,
                "overlap detected: {:?} and {:?}",
                w[0],
                w[1]
            );
        }
        assert_eq!(sorted[0].start, 1000);
        assert_eq!(sorted[3].end, 1400);
    }

    /// Multiple workers concurrently splitting the same fat peer: the CAS in
    /// `split_two` serialises splits so the resulting sub-ranges form a
    /// non-overlapping partition of the original.
    #[test]
    fn concurrent_steal_split_no_overlap() {
        use std::thread;

        let ex = SyncExecutor::new();
        // Worker 3 gets the fat task; workers 0-2 get crumbs.
        let q = TaskQueue::new([0..1, 1..2, 2..3, 0..1000].into_iter());
        q.set_threads(4, 1, Some(&ex)).unwrap();

        let mut handles = Vec::new();
        for id in 0..3usize {
            let q = q.clone();
            let mut t = ex.drained_task_of(id);
            // Each thread returns its task, not a range snapshot: the task keeps
            // the worker's liveness guard, and a worker that drops it while its
            // peers are still stealing looks dead to the queue -- whose sweep
            // would (correctly) hand its unclaimed range to someone else. That
            // would make the "stolen ranges" below the same range twice.
            handles.push(thread::spawn(move || {
                let outcome = q.steal(&id, &mut t, 1, 1);
                (id, outcome, t)
            }));
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // All 3 must succeed by splitting the fat peer.
        for (id, outcome, task) in &results {
            assert_eq!(
                *outcome,
                StealOutcome::Refilled,
                "worker {id} failed to steal via split"
            );
            let range = task.get();
            assert!(range.end - range.start > 0, "worker {id} got empty range");
        }

        // The workers' *final* holdings must not overlap: a later split can carve
        // a sub-range out of an earlier steal, so the final snapshot -- not the
        // range handed out at refill time -- is what partitions 0..1000.
        let mut ranges: Vec<_> = results.iter().map(|(_, _, task)| task.get()).collect();
        ranges.sort_by_key(|r| r.start);
        for w in ranges.windows(2) {
            assert!(
                w[0].end <= w[1].start,
                "split ranges overlap: {:?} and {:?}",
                w[0],
                w[1]
            );
        }
    }

    /// `cancel_task` is a no-op when the registered caller has no speculative
    /// twin: nothing is aborted and no worker is deregistered. This guards
    /// against the call spuriously dropping the caller from `running`.
    #[test]
    fn cancel_task_registered_caller_without_twin_is_noop() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..1, 100..200].into_iter());
        q.set_threads(2, 1, Some(&ex)).unwrap();
        // Worker 0 owns its own task and shares no cursor with anyone.
        q.cancel_task(&ex.task_of(0), &0);
        assert!(ex.aborted().is_empty(), "nothing to abort without a twin");
        assert_eq!(
            q.inner.lock().running.len(),
            2,
            "both workers must stay registered"
        );
    }

    /// Pins the *correct* handoff when a speculative sharer is reclaimed by a
    /// `set_threads` shrink.
    ///
    /// A speculative sharer aliases its twin's progress cursor (same `state`
    /// pointer). When the shrink aborts the sharer and pushes its task into
    /// `waiting`, that reclaimed task still points at the *surviving* twin's
    /// cursor. The next `steal` to drain it calls `take()` on the shared cursor:
    /// `take()` atomically claims the remaining range for the new worker and
    /// empties the shared cursor, so the survivor twin exits cleanly on its next
    /// `safe_add_start` and the range is executed exactly once.
    ///
    /// This guards against a regression where the handoff would be "fixed" by
    /// dropping the reclaimed task — which would instead LOSE the remaining work
    /// when the survivor is the only live worker left.
    #[test]
    fn shrink_reclaims_speculative_sharer_task_aliasing_live_cursor() {
        let ex = SyncExecutor::new();
        // `min_chunk = 10` makes the 5-element peers too small to split, forcing
        // the *share* branch of `steal` instead of `split_two`.
        let q = TaskQueue::new([0..5, 5..10].into_iter());
        q.set_threads(2, 10, Some(&ex)).unwrap();

        // Worker 0 speculatively aliases worker 1's cursor (5..10).
        let mut t0 = ex.drained_task_of(0);
        assert_eq!(q.steal(&0, &mut t0, 10, 2), StealOutcome::Refilled);
        ex.rebind(0, &t0);
        assert!(
            t0.same_cursor(&ex.task_of(1)),
            "worker 0 now aliases worker 1's cursor"
        );

        // Shrink to 1: aborts worker 1, reclaims its task into `waiting`.
        q.set_threads(1, 10, Some(&ex)).unwrap();
        let guard = q.inner.lock();
        assert_eq!(guard.running.len(), 1);
        let survivor = guard.running[0].0.clone();

        // The reclaimed waiting task aliases the *still-running* survivor's
        // cursor. Whoever steals it next re-executes the survivor's range.
        assert_eq!(
            guard.waiting.len(),
            1,
            "the aborted sharer's task was reclaimed"
        );
        assert!(
            guard.waiting[0].same_cursor(&survivor),
            "reclaimed task shares the survivor's cursor; steal's take() handshake transfers it safely"
        );
    }

    /// A worker that dies with work still unclaimed must not strand it: the next
    /// `steal` reaps the dead registration and hands its range to a survivor.
    ///
    /// Before `steal` swept, the fat-task scan merely *skipped* dead entries and
    /// only `set_threads` reclaimed them -- so a panicking worker's whole
    /// remaining range was unreachable until someone happened to resize the pool.
    #[test]
    fn steal_reaps_a_dead_peers_range() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..1, 100..200].into_iter());
        q.set_threads(2, 1, Some(&ex)).unwrap();
        // Worker 1 dies mid-task: its registration stays, its range unclaimed.
        ex.kill(1);

        // No `set_threads`: only the survivor's refill can rescue the range.
        let mut t0 = ex.drained_task_of(0);
        assert_eq!(q.steal(&0, &mut t0, 1, 1), StealOutcome::Refilled);
        assert_eq!(t0.get(), 100..200, "the dead peer's range was stranded");
        let guard = q.inner.lock();
        assert_eq!(guard.running.len(), 1, "the dead entry was reaped");
        assert_eq!(guard.waiting.len(), 0);
    }

    /// `add` reaps a dead registration too, so a task added after a worker
    /// panicked is not queued behind a range nobody can reach.
    #[test]
    fn add_reaps_a_dead_peers_range() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..1, 100..200].into_iter());
        q.set_threads(2, 1, Some(&ex)).unwrap();
        ex.kill(1);
        assert!(q.add(Task::new(500..510)), "worker 0 is still live");

        let mut t0 = ex.drained_task_of(0);
        // FIFO: the reaped range comes before the added one.
        assert_eq!(q.steal(&0, &mut t0, 1, 1), StealOutcome::Refilled);
        assert_eq!(t0.get(), 100..200);
        let guard = q.inner.lock();
        assert_eq!(guard.running.len(), 1);
        assert_eq!(guard.waiting.len(), 1, "the added task still waits");
        assert_eq!(guard.waiting[0].get(), 500..510);
    }

    /// A `usize::MAX` request is bounded by the work, not by a constant: growth
    /// stops once no range can be split under `min_chunk_size`. Eight one-byte
    /// pieces are all this queue can feed, so eight workers is all it spawns.
    #[test]
    fn set_threads_stops_at_the_splittable_work() {
        let ex = HoldExecutor {
            spawned: Arc::new(Mutex::new(0)),
            tasks: Arc::new(Mutex::new(Vec::new())),
        };
        let q = TaskQueue::new(core::iter::once(0..8));
        q.set_threads(usize::MAX, 1, Some(&ex)).unwrap();
        assert_eq!(
            q.inner.lock().running.len(),
            8,
            "the pool grows only as far as the splittable work feeds it"
        );
    }

    /// `None` does not roll back the sweep: dead registrations are dropped and
    /// the ranges they still hold are handed back to `waiting`, so the next call
    /// with an executor can still spend them.
    #[test]
    fn set_threads_none_still_reclaims_dead_work() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new(core::iter::once(0..10));
        q.set_threads(1, 1, Some(&ex)).unwrap();
        ex.kill(0);

        assert!(q.set_threads::<SyncExecutor>(2, 1, None).is_none());
        let guard = q.inner.lock();
        assert_eq!(guard.running.len(), 0, "the dead entry was swept");
        assert_eq!(guard.waiting.len(), 1, "its range went back to waiting");
        assert_eq!(guard.waiting[0].get(), 0..10);
    }

    /// `Debug` shows the pool sizes without requiring `H: Debug` (`SyncHandle`
    /// has no `Debug` impl at all) and without locking the queue.
    #[test]
    fn queue_debug_reports_the_pool_sizes() {
        use std::format;
        let q: TaskQueue<SyncHandle> = TaskQueue::new(core::iter::once(0..5));
        let text = format!("{q:?}");
        assert!(text.contains("waiting: 1"), "pool sizes missing: {text}");
        assert!(text.contains("running: 0"), "pool sizes missing: {text}");
    }

    /// Drives one full refill under whichever lock backend is compiled in. Under
    /// `--no-default-features --features spin` this is the test that pins the
    /// `spin` path, so it cannot be broken while every other test still passes on
    /// `std`.
    #[cfg(all(feature = "spin", not(feature = "std")))]
    #[test]
    fn spin_backend_serves_a_refill() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new(core::iter::once(0..10));
        q.set_threads(1, 1, Some(&ex)).unwrap();
        let _ = q.add(Task::new(100..110));

        let mut t = ex.drained_task_of(0);
        assert_eq!(q.steal(&0, &mut t, 1, 1), StealOutcome::Refilled);
        assert_eq!(t.get(), 100..110);
        assert!(q.inner.lock().running[0].0.is_alive());
    }
}
