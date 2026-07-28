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
    pub fn add(&self, task: Task) {
        let mut guard = self.inner.lock();
        guard.waiting.push_back(task);
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
        for (i, (_, handle)) in guard.running.iter_mut().enumerate() {
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
            if let Some(range) = new_task.take() {
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
            if steal_task.remain() >= min_chunk_size * 2
                && let Ok(Some(range)) = steal_task.split_two()
            {
                *task = Task::new(range);
                found = true;
            } else if max_speculative > 1
                && steal_task.remain() > 0
                && steal_task.strong_count() <= max_speculative
            {
                task.state = steal_task.state;
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
        guard.running.retain(|t| t.0.strong_count() > 0);
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
                && steal_task.remain() >= min_chunk_size * 2
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
    /// worker `id`, reclaiming it back into the waiting queue.
    pub fn cancel_task(&self, task: &Task, id: &H::Id) {
        let mut guard = self.inner.lock();
        for (weak, handle) in &mut guard.running {
            if let Some(t) = weak.upgrade()
                && t == *task
                && !handle.is_self(id)
            {
                handle.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    extern crate std;
    use crate::{Executor, Handle, Task, TaskQueue};
    use std::{collections::HashMap, dbg, println};
    use tokio::{sync::mpsc, task::AbortHandle};

    struct TokioExecutor {
        tx: mpsc::UnboundedSender<(u64, u64)>,
        speculative: usize,
    }
    #[derive(Clone)]
    struct TokioHandle(AbortHandle);

    impl Handle for TokioHandle {
        type Output = ();
        type Id = ();
        fn abort(&mut self) -> Self::Output {
            self.0.abort();
        }
        fn is_self(&mut self, (): &Self::Id) -> bool {
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
                    while task.start() < task.end() {
                        let i = task.start();
                        let res = fib(i);
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

    fn fib(n: u64) -> u64 {
        match n {
            0 => 0,
            1 => 1,
            _ => fib(n - 1) + fib(n - 2),
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
}
