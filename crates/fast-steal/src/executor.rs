//! Traits that connect a [`TaskQueue`](crate::TaskQueue) to an async runtime.
//!
//! Implement [`Executor`] to spawn work onto your runtime, and [`Handle`] to give the
//! queue a way to identify and abort the tasks it spawned.

use crate::{Task, TaskQueue};

/// User-defined executor that runs tasks on a [`TaskQueue`].
///
/// Implement this trait to integrate with any async runtime (e.g. tokio, smol).
/// The associated [`Handle`] type is used for task cancellation and identification.
pub trait Executor {
    type Handle: Handle;
    /// Spawns a worker that processes `task` and, once exhausted, refills it via
    /// [`TaskQueue::steal`].
    ///
    /// # Contract
    ///
    /// - **Do not call back into the queue synchronously.** This method is
    ///   invoked by [`TaskQueue::set_threads`] *while the queue's internal lock
    ///   is held*; synchronously calling any `TaskQueue` method (`add`, `steal`,
    ///   `set_threads`, `cancel_task`, `handles`) on `task_queue` from inside
    ///   `execute` deadlocks. Defer all queue access to the spawned worker
    ///   (e.g. inside `tokio::spawn`), which runs after the lock is released.
    /// - **The returned handle must identify the spawned worker.** Its
    ///   [`Handle::is_self`] must return `true` for the id the worker passes to
    ///   [`TaskQueue::steal`] — that handle is the worker's *only* registration
    ///   key. If it fails to match, `steal` treats the worker as unregistered
    ///   and returns `false`, which the worker cannot distinguish from "no work
    ///   left": it exits while work may still be waiting.
    fn execute(&self, task: Task, task_queue: TaskQueue<Self::Handle>) -> Self::Handle;
}

/// Handle returned by [`Executor::execute`], used to abort or identify a running task.
pub trait Handle {
    type Id;
    /// Requests cancellation of the worker this handle was returned for.
    ///
    /// # Contract
    ///
    /// Invoked by [`TaskQueue`](crate::TaskQueue) *while its internal lock is
    /// held* — the same reentrancy prohibition as [`Executor::execute`] applies:
    /// do not synchronously call back into the queue. Cancellation may be
    /// cooperative (a signal the worker observes at its next await point);
    /// the queue does not require the worker to have stopped when this returns.
    fn abort(&mut self);
    /// Returns whether `id` identifies the worker this handle was returned for.
    ///
    /// # Contract
    ///
    /// This is the *sole* registration key [`TaskQueue::steal`](crate::TaskQueue::steal)
    /// uses to authenticate a worker (see [`Executor::execute`]). It must be a
    /// pure predicate: `true` iff `id` is the id of the worker spawned by the
    /// `execute` call that returned this handle, consistent across calls. An
    /// implementation that misidentifies its worker makes `steal` silently
    /// strand waiting work.
    fn is_self(&self, id: &Self::Id) -> bool;
}

#[cfg(test)]
mod tests {
    //! Contract tests: these pin the *requirements* [`TaskQueue`] places on
    //! implementors of [`Executor`] / [`Handle`], which the trait signatures
    //! alone cannot express.
    use crate::{Executor, Handle, Task, TaskQueue};
    use core::cell::Cell;

    /// A runtime-free stub: `execute` spawns nothing and just returns a handle
    /// carrying a sequential id. `honest` controls whether `is_self` upholds
    /// its contract or violates it (always `false`).
    struct StubExecutor {
        honest: bool,
        next_id: Cell<usize>,
    }
    struct StubHandle {
        honest: bool,
        id: usize,
    }
    impl Handle for StubHandle {
        type Id = usize;
        fn abort(&mut self) {}
        fn is_self(&self, id: &Self::Id) -> bool {
            self.honest && self.id == *id
        }
    }
    impl Executor for StubExecutor {
        type Handle = StubHandle;
        fn execute(&self, _task: Task, _queue: TaskQueue<Self::Handle>) -> Self::Handle {
            let id = self.next_id.get();
            self.next_id.set(id + 1);
            StubHandle {
                honest: self.honest,
                id,
            }
        }
    }

    fn queue_with_one_worker(honest: bool) -> TaskQueue<StubHandle> {
        let executor = StubExecutor {
            honest,
            next_id: Cell::new(0),
        };
        let queue = TaskQueue::new(core::iter::once(0..10));
        // Registers worker 0: its task is handed to `execute` and its handle is
        // stored in `running`. The stub spawns nothing, so nothing consumes the
        // `10..20` we enqueue next -- by construction it stays in `waiting`.
        queue
            .set_threads(1, 1, Some(&executor))
            .expect("executor provided");
        let _ = queue.add(Task::new(10..20));
        queue
    }

    /// CONTRACT (`is_self`): the handle returned by `execute` MUST report `true`
    /// for the id of the worker spawned in that same call. This is the *sole*
    /// registration key `steal` has.
    #[test]
    fn honest_is_self_lets_the_worker_steal_waiting_work() {
        let queue = queue_with_one_worker(true);
        let mut task = Task::new(0..0);
        assert!(queue.steal(&0, &mut task, 1, 1));
        assert_eq!(task.get(), 10..20);
    }

    /// AUDIT FINDING B-03: a broken `is_self` (always `false`) makes `steal`
    /// return `false` -- indistinguishable from "no work left" -- while the
    /// waiting queue still holds `10..20`. The worker exits and that work is
    /// silently stranded. Nothing in the trait signature or docs prevents this.
    #[test]
    fn broken_is_self_strands_waiting_work() {
        let queue = queue_with_one_worker(false);
        let mut task = Task::new(0..0);
        // Same queue state as the honest twin above; only `is_self` differs.
        assert!(!queue.steal(&0, &mut task, 1, 1));
        // The caller's task was not refilled, yet 10..20 was never handed out.
        assert_eq!(task.get(), 0..0);
    }

    /// CONTRACT (id pairing): an id that no registered handle recognises is
    /// treated as an unregistered worker -- `steal` refuses it outright, even
    /// with an honest `is_self` and work available.
    #[test]
    fn unregistered_id_cannot_steal() {
        let queue = queue_with_one_worker(true);
        let mut task = Task::new(0..0);
        // Worker ids handed out so far: only 0. Id 7 was never registered.
        assert!(!queue.steal(&7, &mut task, 1, 1));
        assert_eq!(task.get(), 0..0);
    }
}
