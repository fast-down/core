//! A concurrent work-stealing queue.
//!
//! [`TaskQueue`] holds pending and running [`Task`](crate::Task)s and lets worker
//! threads pull fresh work or steal a sub-range from a busy peer via
//! [`steal`](TaskQueue::steal). The number of running workers can be adjusted at
//! runtime with [`set_threads`](TaskQueue::set_threads).

extern crate alloc;
use crate::{Executor, Handle, Task, WeakTask};
use alloc::{collections::vec_deque::VecDeque, sync::Arc, vec::Vec};
use core::ops::Range;
use parking_lot::Mutex;

/// A concurrent work-stealing queue that manages a set of [`Task`]s.
///
/// Workers created by [`Executor::execute`] call [`steal`](TaskQueue::steal) to obtain
/// new work when their current task is exhausted. The queue supports splitting,
/// speculative execution, and dynamic thread adjustment.
#[derive(Debug)]
pub struct TaskQueue<H: Handle> {
    inner: Arc<Mutex<TaskQueueInner<H>>>,
}
impl<H: Handle> Clone for TaskQueue<H> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}
#[derive(Debug)]
struct TaskQueueInner<H: Handle> {
    running: VecDeque<(WeakTask, H)>,
    waiting: VecDeque<Task>,
}
impl<H: Handle> TaskQueue<H> {
    /// Creates a queue from an iterator of `start..end` ranges, each wrapped in its
    /// own [`Task`].
    pub fn new(tasks: impl Iterator<Item = Range<u64>>) -> Self {
        let waiting: VecDeque<_> = tasks.map(Task::from).collect();
        Self {
            inner: Arc::new(Mutex::new(TaskQueueInner {
                running: VecDeque::with_capacity(waiting.len()),
                waiting,
            })),
        }
    }
    /// Appends a [`Task`] to the waiting queue so a future
    /// [`steal`](TaskQueue::steal) or [`set_threads`](TaskQueue::set_threads) can
    /// pick it up.
    ///
    /// Returns `true` if at least one worker is currently registered and live
    /// (so the task will be picked up on that worker's next [`steal`](TaskQueue::steal)), or
    /// `false` if no live worker exists — in which case the task stays stranded
    /// in `waiting` until a [`set_threads`](TaskQueue::set_threads) call spawns a
    /// worker to rescue it.
    #[must_use]
    pub fn add(&self, task: Task) -> bool {
        let mut guard = self.inner.lock();
        let live = guard.running.iter().any(|w| w.0.strong_count() > 0);
        guard.waiting.push_back(task);
        live
    }
    /// Tries to refill `task` with more work for the worker identified by `id`.
    ///
    /// The caller must pass its own currently-held [`Task`] plus `id` (compared via
    /// [`Handle::is_self`](crate::Handle::is_self)). The function first hands out a
    /// pending task from the waiting queue; if none is available it steals a half
    /// range from the busiest running task via [`Task::split_two`](crate::Task::split_two)
    /// (when at least `min_chunk_size * 2` work remains), or, if `max_speculative > 1`
    /// and the stolen task has few enough strong references, shares that same task
    /// speculatively.
    ///
    /// Returns `true` if `task` was refilled, or `false` if the worker is not
    /// registered or no work could be found.
    pub fn steal(
        &self,
        id: &H::Id,
        task: &mut Task,
        min_chunk_size: u64,
        max_speculative: usize,
    ) -> bool {
        let min_chunk_size = min_chunk_size.max(1);
        let mut guard = self.inner.lock();
        let mut worker_idx = None;
        for (i, (_, handle)) in guard.running.iter().enumerate() {
            if handle.is_self(id) {
                worker_idx = Some(i);
                break;
            }
        }
        let Some(worker_idx) = worker_idx else {
            return false;
        };
        let mut found = false;
        while let Some(new_task) = guard.waiting.pop_front() {
            // A task whose range invariant is broken (`start > end`) yields
            // `Err` and is skipped, never handed to a worker. This keeps steal's
            // policy toward corrupted tasks uniform with the speculative branch
            // below, which likewise discards `split_two`'s `Err`. Whether steal
            // should instead surface such corruption is deliberately left open
            // until the queue-level audit revisits it.
            if let Ok(Some(range)) = new_task.take() {
                *task = Task::new(range);
                found = true;
                break;
            }
        }
        if !found
            && let Some(steal_task) = guard
                .running
                .iter()
                .filter_map(|w| w.0.upgrade())
                .filter(|w| w != task)
                .max_by_key(Task::remain)
        {
            if steal_task.remain() >= min_chunk_size.saturating_mul(2)
                && let Ok(Some(range)) = steal_task.split_two()
            {
                *task = Task::new(range);
                found = true;
            } else if max_speculative > 1
                && steal_task.remain() > 0
                && steal_task.sharer_count() < max_speculative
            {
                task.share_state(&steal_task);
                found = true;
            }
        }
        if found {
            guard.running[worker_idx].0 = task.downgrade();
        }
        found
    }
    /// Returns `None` when threads need to be increased but the executor is `None`
    #[must_use]
    pub fn set_threads<E: Executor<Handle = H>>(
        &self,
        threads: usize,
        min_chunk_size: u64,
        executor: Option<&E>,
    ) -> Option<()> {
        #![allow(clippy::significant_drop_tightening)]
        let min_chunk_size = min_chunk_size.max(1);
        let mut guard = self.inner.lock();
        guard.running.retain(|t| t.0.is_alive());
        let len = guard.running.len();
        if len < threads {
            let executor = executor?;
            let need = (threads - len).min(guard.waiting.len());
            let mut temp = Vec::with_capacity(need);
            let iter = guard.waiting.drain(..need);
            for task in iter {
                let weak = task.downgrade();
                let handle = executor.execute(task, self.clone());
                temp.push((weak, handle));
            }
            guard.running.extend(temp);
            while guard.running.len() < threads
                && let Some(steal_task) = guard
                    .running
                    .iter()
                    .filter_map(|w| w.0.upgrade())
                    .max_by_key(Task::remain)
                && steal_task.remain() >= min_chunk_size.saturating_mul(2)
                && let Ok(Some(range)) = steal_task.split_two()
            {
                let task = Task::new(range);
                let weak = task.downgrade();
                let handle = executor.execute(task, self.clone());
                guard.running.push_back((weak, handle));
            }
        } else if len > threads {
            let mut temp = Vec::with_capacity(len - threads);
            let iter = guard.running.drain(threads..);
            for (task, mut handle) in iter {
                if let Some(task) = task.upgrade() {
                    temp.push(task);
                }
                handle.abort();
            }
            guard.waiting.extend(temp);
        }
        Some(())
    }
    /// Provides mutable access to the handles of all running tasks, e.g. to abort
    /// or inspect them.
    ///
    /// # Liveness / deadlock contract
    /// The closure `f` is invoked *while the queue lock is held*. It must **not**
    /// re-enter `TaskQueue` (e.g. call [`steal`](TaskQueue::steal),
    /// [`add`](TaskQueue::add), [`set_threads`](TaskQueue::set_threads), or
    /// [`handles`](TaskQueue::handles) again) — doing so deadlocks. Keep `f`
    /// short: it blocks every other queue operation until it returns.
    pub fn handles<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut dyn Iterator<Item = &mut H>) -> R,
    {
        #![allow(clippy::significant_drop_tightening)]
        let mut guard = self.inner.lock();
        let mut iter = guard.running.iter_mut().map(|w| &mut w.1);
        f(&mut iter)
    }

    /// Aborts every running task equal to `task` that does not belong to the
    /// worker `id`.
    ///
    /// Aborted twins are *deregistered* (removed from `running`) so a later
    /// [`set_threads`](TaskQueue::set_threads) liveness sweep does not mistake
    /// them for live workers. Their remaining work is **not** reclaimed into
    /// `waiting`: reclaiming would race with the caller's own still-live `task`
    /// over the same range and cause duplicate execution. Callers must therefore
    /// only invoke this after the shared range has finished (as the sole
    /// production caller `fast-pull` does). See audit finding C-05.
    pub fn cancel_task(&self, task: &Task, id: &H::Id) {
        let mut guard = self.inner.lock();
        // Abort every twin whose task matches but is not the caller's own, then
        // *deregister* it (drop it from `running`). We rebuild `running` from the
        // survivors because removing in place would require mutating through a
        // shared `&` handed to `retain`'s closure.
        let mut kept: VecDeque<(WeakTask, H)> = VecDeque::with_capacity(guard.running.len());
        for (weak, mut handle) in guard.running.drain(..) {
            let is_twin = weak
                .upgrade()
                .is_some_and(|t| t == *task && !handle.is_self(id));
            if is_twin {
                handle.abort();
            } else {
                kept.push_back((weak, handle));
            }
        }
        guard.running = kept;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    extern crate std;
    use crate::{Executor, Handle, Task, TaskQueue};
    use std::{
        collections::{HashMap, HashSet},
        dbg, println,
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
    }
    #[derive(Clone)]
    struct TokioHandle(AbortHandle);

    impl Handle for TokioHandle {
        type Id = ();
        fn abort(&mut self) {
            self.0.abort();
        }
        fn is_self(&self, (): &Self::Id) -> bool {
            false
        }
    }

    impl Executor for TokioExecutor {
        type Handle = TokioHandle;
        fn execute(&self, mut task: Task, task_queue: TaskQueue<Self::Handle>) -> Self::Handle {
            println!("execute");
            let tx = self.tx.clone();
            let speculative = self.speculative;
            let handle = tokio::spawn(async move {
                loop {
                    // Keep the worker alive briefly so the shrink-mid-run test can
                    // observe in-flight work without paying the recursive-fib cost.
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    while task.start() < task.end() {
                        let i = task.start();
                        let res = fib_fast(i);
                        let Ok(_) = task.safe_add_start(i, 1) else {
                            println!("task-failed: {i} = {res}");
                            continue;
                        };
                        println!("task: {i} = {res}");
                        tx.send((i, res)).unwrap();
                    }
                    if !task_queue.steal(&(), &mut task, 1, speculative) {
                        break;
                    }
                }
            });
            let abort_handle = handle.abort_handle();
            TokioHandle(abort_handle)
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
        let executor = TokioExecutor { tx, speculative: 1 };
        let pre_data = [1..20, 41..48];
        let task_queue = TaskQueue::new(pre_data.iter().cloned());
        task_queue.set_threads(8, 1, Some(&executor)).unwrap();
        drop(executor);
        let mut data = HashMap::new();
        while let Some((i, res)) = rx.recv().await {
            println!("main: {i} = {res}");
            assert!(
                data.insert(i, res).is_none(),
                "number {i} with value {res} was computed twice"
            );
        }
        dbg!(&data);
        for range in pre_data {
            for i in range {
                assert_eq!((i, data.get(&i)), (i, Some(&fib_fast(i))));
                data.remove(&i);
            }
        }
        assert_eq!(data.len(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_task_queue2() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let executor = TokioExecutor { tx, speculative: 2 };
        let pre_data = [1..20, 41..48];
        let task_queue = TaskQueue::new(pre_data.iter().cloned());
        task_queue.set_threads(8, 1, Some(&executor)).unwrap();
        drop(executor);
        let mut data = HashMap::new();
        while let Some((i, res)) = rx.recv().await {
            println!("main: {i} = {res}");
            assert!(
                data.insert(i, res).is_none(),
                "number {i} with value {res} was computed twice"
            );
        }
        dbg!(&data);
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
                    if !q.steal(&id, &mut task, 1, speculative) {
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
        // the big peer and end up doing far more than their initial 1-number
        // crumb. A broken `is_self` makes `steal` a no-op, leaving *exactly one*
        // worker (the big one) above 1.
        //
        // The threshold is deliberately loose. How many crumb workers get a bite
        // depends on scheduling: a crumb worker that wakes late may find the big
        // range already drained by its peers and legitimately finish with 1. So
        // we assert "several workers shared the big range", not "all of them" —
        // that still fails hard (1 < 3) when steal is broken, without being
        // flaky under an unlucky interleaving.
        let multi = per_worker.values().filter(|&&c| c > 1).count();
        assert!(
            multi >= 3,
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

    /// Q-02 regression: the README work-stealing example is `no_run`, so its
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
        // The discriminating Q-02 check: steals must have actually occurred.
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
            // Keep the `Task` alive with a strong reference so `Weak::upgrade`
            // during `set_threads` reclaim always succeeds. No real work is done
            // and no background task is spawned, so the test runtime exits cleanly.
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
    /// failed `Weak::upgrade` during reclaim, or a double-drain on increase) would
    /// break one of these assertions immediately.
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
        // Collapse to zero: every running task must be reclaimed into `waiting`.
        q.set_threads(0, 1, Some(&ex)).unwrap();
        let guard = q.inner.lock();
        assert_eq!(guard.running.len(), 0);
        assert_eq!(guard.waiting.len(), total);
        drop(guard);
    }

    /// `add` pushes onto the waiting queue (`task_queue.rs` 50-53) and a live worker's
    /// `steal` then pulls that freshly-added task off `waiting` (the `found = true;
    /// break` branch, `task_queue.rs` line 91) before falling back to stealing from a
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
        // `add` lands a brand-new task in `waiting` (covers 50-53). A live worker
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
        // ...and the `add`ed task was pulled from `waiting` via `steal` (line 91).
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
        Task::from_raw_state(Arc::new(portable_atomic::AtomicU128::new(
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

    /// `TaskQueue::new` funnels every range through `Task::from`, so it inherits
    /// that function's panic contract -- yet its own doc comment never mentions
    /// it, and clippy's `missing_panics_doc` cannot see across the call.
    #[test]
    #[should_panic(expected = "range.start <= range.end")]
    fn new_inherits_the_reversed_range_panic() {
        // Struct literal on purpose: an inline `10..5` trips
        // `clippy::reversed_empty_ranges`.
        let bad = core::ops::Range {
            start: 10u64,
            end: 5u64,
        };
        let _: TaskQueue<SyncHandle> = TaskQueue::new(core::iter::once(bad));
    }

    /// AUDIT FINDING (C-01): `add` only appends to `waiting`. It wakes nobody and
    /// returns `()`, so once every worker has exited (each one's `steal` returned
    /// `false` and it left its loop) an added task is stranded forever with no
    /// signal to the caller.
    ///
    /// The same test pins a related surprise: `set_threads` reports success even
    /// when it spawned nothing at all because there was no work to hand out.
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

    /// The `true` branch of C-01's `add`: when at least one live worker exists,
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

    /// An unregistered caller gets exactly the same `false` a worker gets when the
    /// queue is drained -- even with work sitting in `waiting`.
    #[test]
    fn steal_rejects_an_unregistered_worker() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new(core::iter::once(0..100));
        q.set_threads(1, 1, Some(&ex)).unwrap();
        let _ = q.add(Task::new(500..510));
        let mut t = Task::new(0..0);
        assert!(!q.steal(&999, &mut t, 1, 1));
        assert_eq!(t.get(), 0..0);
        assert_eq!(
            q.inner.lock().waiting.len(),
            1,
            "waiting must not be disturbed by an unknown caller"
        );
    }

    #[test]
    fn steal_prefers_waiting_over_robbing_a_peer() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..2, 100..200].into_iter());
        q.set_threads(2, 1, Some(&ex)).unwrap();
        let _ = q.add(Task::new(500..510));
        let mut t = ex.task_of(0);
        assert!(q.steal(&0, &mut t, 1, 1));
        assert_eq!(t.get(), 500..510);
        assert_eq!(
            ex.task_of(1).get(),
            100..200,
            "the fat peer keeps its range: waiting wins over split_two"
        );
        // The worker is re-registered against its brand-new task.
        assert_eq!(q.inner.lock().running[0].0.upgrade().unwrap(), t);
    }

    /// The waiting drain loop pops until it finds *usable* work: an exhausted task
    /// yields `Ok(None)` and a corrupted one yields `Err`; both are discarded
    /// rather than handed to a worker.
    ///
    /// Note this path only became safe with the `take` hardening: while `take`
    /// still returned `Some(20..10)` for a corrupted task, `Task::new(range)`
    /// below would have tripped its `start <= end` assertion instead.
    #[test]
    fn steal_skips_exhausted_and_corrupted_waiting_tasks() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new(core::iter::once(0..2));
        q.set_threads(1, 1, Some(&ex)).unwrap();
        let _ = q.add(Task::new(7..7));
        let _ = q.add(corrupted_task());
        let _ = q.add(Task::new(500..510));
        let mut t = ex.task_of(0);
        assert!(q.steal(&0, &mut t, 1, 1));
        assert_eq!(t.get(), 500..510);
        assert_eq!(
            q.inner.lock().waiting.len(),
            0,
            "all three were popped; the two unusable ones are dropped"
        );
    }

    /// A lone worker cannot steal from itself: the victim scan filters out any
    /// running task pointer-equal to the caller's own.
    #[test]
    fn steal_excludes_the_caller_as_a_victim() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new(core::iter::once(0..1000));
        q.set_threads(1, 1, Some(&ex)).unwrap();
        let mut t = ex.task_of(0);
        assert!(!q.steal(&0, &mut t, 1, 2));
        assert_eq!(t.get(), 0..1000, "the caller's own range must be untouched");
    }

    #[test]
    fn steal_splits_the_busiest_peer() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..2, 10..14, 100..200].into_iter());
        q.set_threads(3, 1, Some(&ex)).unwrap();
        let mut t = ex.task_of(0);
        assert!(q.steal(&0, &mut t, 1, 1));
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
    #[test]
    fn steal_shares_speculatively_only_when_allowed() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..1, 100..101].into_iter());
        q.set_threads(2, 1, Some(&ex)).unwrap();
        let mut t = ex.task_of(0);
        // Speculation disabled -> the crumb is left alone and the caller starves.
        assert!(!q.steal(&0, &mut t, 1, 1));
        assert_eq!(t.get(), 0..1);
        // Speculation enabled -> the caller now aliases the peer's state.
        assert!(q.steal(&0, &mut t, 1, 2));
        assert_eq!(t.get(), 100..101);
        assert_eq!(t, ex.task_of(1), "speculation must alias, not copy");
    }

    /// The speculation cap limits how many workers share one cursor. Each sharer
    /// holds its own strong ref to the cursor (via `share_state`), and the cap
    /// `sharer_count() < max_speculative` admits a new sharer only while fewer
    /// than `max_speculative` workers already alias that cursor — keeping the
    /// total at `max_speculative`.
    #[test]
    fn steal_caps_the_number_of_speculative_sharers() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..1, 1..2, 2..3].into_iter());
        q.set_threads(3, 1, Some(&ex)).unwrap();
        // All crumbs tie on `remain`, and `max_by_key` documents that ties resolve
        // to the *last* element, so worker 0 joins worker 2.
        let mut t0 = ex.task_of(0);
        assert!(q.steal(&0, &mut t0, 1, 2));
        ex.rebind(0, &t0);
        assert_eq!(t0, ex.task_of(2));
        // Worker 1 tries to become the third sharer of that same crumb and is
        // refused: the sharer count now exceeds the cap.
        let mut t1 = ex.task_of(1);
        assert!(!q.steal(&1, &mut t1, 1, 2));
        assert_eq!(t1.get(), 1..2, "the refused worker keeps its own range");
    }

    /// AUDIT FINDING (C-02, fixed): `min_chunk_size * 2` used to be an unchecked
    /// multiplication on a caller-supplied `u64` — `fast-pull` forwards a
    /// user-configurable `options.min_chunk_size` straight into it, so a large
    /// value panicked in debug and silently wrapped (disabling split) in release.
    /// It now uses `saturating_mul`, so an overflowing value simply caps at
    /// `u64::MAX` and the split branch is skipped gracefully instead of panicking.
    #[test]
    fn steal_skips_split_on_a_huge_min_chunk_size() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..2, 100..200].into_iter());
        q.set_threads(2, 1, Some(&ex)).unwrap();
        let mut t = ex.task_of(0);
        // No panic, no silent disable: the caller keeps its own tiny range because
        // the split branch is simply never taken (`remain >= u64::MAX` is false).
        assert!(!q.steal(&0, &mut t, u64::MAX, 1));
        assert_eq!(
            t.get(),
            0..2,
            "the caller is untouched when split is skipped"
        );
    }

    /// The identical `min_chunk_size * 2` in `set_threads`'s split-to-grow loop is
    /// now `saturating_mul` too (C-02, fixed): an overflowing value no longer
    /// panics, the split-to-grow branch is just skipped.
    #[test]
    fn set_threads_skips_split_on_a_huge_min_chunk_size() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new(core::iter::once(0..100));
        assert!(q.set_threads(2, u64::MAX, Some(&ex)).is_some());
        // The single waiting task is still spawned, but no extra split worker is
        // created because `remain >= u64::MAX` is always false.
        assert_eq!(q.inner.lock().running.len(), 1);
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
        let _ = q.set_threads(2, 1, Some(&ex));
        assert_eq!(q.inner.lock().running.len(), 1, "the dead slot is swept");
    }

    /// AUDIT FINDING (C-03, fixed): the liveness sweep used to key off the *cursor*
    /// refcount, which speculative sharing defeats — a dead worker's slot stayed
    /// propped up by its surviving twin and `set_threads` never refilled the pool.
    /// `WeakTask` now points at the worker's own identity (`TaskInner`), so the
    /// sweep reclaims a dead worker's slot regardless of how many twins share its
    /// cursor.
    #[test]
    fn speculative_sharing_no_longer_defeats_liveness_sweep() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..1, 100..101].into_iter());
        q.set_threads(2, 1, Some(&ex)).unwrap();

        let mut t0 = ex.task_of(0);
        assert!(q.steal(&0, &mut t0, 1, 2));
        ex.rebind(0, &t0);
        // Both `running` entries now weak-point at one and the same cursor.
        let guard = q.inner.lock();
        assert_eq!(
            guard.running[0].0.upgrade().unwrap(),
            guard.running[1].0.upgrade().unwrap()
        );
        drop(guard);

        // Worker 0 exits, leaving worker 1 as the only live worker.
        ex.kill(0);
        drop(t0);
        assert_eq!(ex.live_workers(), 1);

        // The liveness sweep alone must reclaim the dead worker's slot, even
        // though its speculative twin still references the shared cursor. Passing
        // `None` for the executor runs the sweep without spawning replacements.
        let _ = q.set_threads::<SyncExecutor>(2, 1, None);
        assert_eq!(
            q.inner.lock().running.len(),
            1,
            "dead worker reclaimed despite its speculative twin (C-03 fixed)"
        );
    }

    /// `handles` had no coverage inside `fast-steal` at all -- its only consumer
    /// lives in `fast-pull`. It must expose every running handle by mutable
    /// reference and hand the closure's return value back out.
    #[test]
    fn handles_exposes_every_running_worker() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..1, 1..2, 2..3].into_iter());
        q.set_threads(3, 1, Some(&ex)).unwrap();
        let ids = q.handles(|iter| iter.map(|h| h.id).collect::<Vec<_>>());
        assert_eq!(ids, [0, 1, 2]);
        q.handles(|iter| {
            for h in iter {
                h.abort();
            }
        });
        assert_eq!(ex.aborted(), [0, 1, 2]);
    }

    /// `cancel_task` is the speculation cleanup path: when one sharer finishes the
    /// shared range it aborts the others. It had no coverage in `fast-steal`.
    #[test]
    fn cancel_task_aborts_twins_but_spares_the_caller() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..1, 100..101].into_iter());
        q.set_threads(2, 1, Some(&ex)).unwrap();
        let mut t0 = ex.task_of(0);
        assert!(q.steal(&0, &mut t0, 1, 2));
        ex.rebind(0, &t0);

        // Worker 1 finished the shared range and cancels its twins.
        q.cancel_task(&t0, &1);
        assert_eq!(ex.aborted(), [0], "only the peer sharer is aborted");
        // C-05 fix: the aborted twin is now deregistered, leaving only the
        // caller's own entry. (Previously it lingered in `running`.)
        assert_eq!(
            q.inner.lock().running.len(),
            1,
            "aborted twin is deregistered"
        );
    }

    /// AUDIT FINDING (C-05): `cancel_task` aborts and deregisters the twin, but
    /// does **not** reclaim the remaining range into `waiting`. That is
    /// deliberate: reclaiming would race with the caller's own still-live `task`
    /// over the same range and cause duplicate execution. The soundness therefore
    /// depends on the caller invoking this only after the shared range is
    /// finished (as `fast-pull` does). Aimed at live work it silently strands the
    /// remainder — by design, not by accident.
    #[test]
    fn cancel_task_does_not_reclaim_unfinished_work() {
        let ex = SyncExecutor::new();
        let q = TaskQueue::new([0..1, 100..200].into_iter());
        q.set_threads(2, 1, Some(&ex)).unwrap();
        let victim = ex.task_of(1);
        assert_eq!(victim.remain(), 100);
        q.cancel_task(&victim, &0);
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
    }
}
