//! A concurrency-limited download queue.
//!
//! [`Queue`] runs many [`State`] downloads while keeping at most a configured
//! number in flight. A task is a [`State`] plus a [`StartPolicy`]: the queue
//! resolves ([`State::build`]) and runs it, so a slot covers prefetching, path
//! probing, and the download itself.
//!
//! Tasks are identified by a caller-chosen key `K` (a download manager's own
//! task id, say), which every event carries, so the queue needs no id mapping of
//! its own. The queue only schedules: progress, speed and outcome come from each
//! task's own [`Event`]s, tagged with its key on the same stream.
//!
//! ```no_run
//! use fast_down_api::{
//!     Event, PartialConfig, Queue, QueueEvent, StartMode, State, create_queue_channel,
//! };
//! use url::Url;
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let (tx, rx) = create_queue_channel::<i32>();
//! let queue = Queue::<i32>::new(2, tx); // at most two downloads at a time
//!
//! for (gid, url) in ["https://example.com/a.bin", "https://example.com/b.bin"]
//!     .into_iter()
//!     .enumerate()
//! {
//!     queue.push(
//!         gid as i32,
//!         State::new(Url::parse(url)?, PartialConfig::default()),
//!         StartMode::Auto,
//!     )?;
//! }
//!
//! // One stream carries every task's events, tagged with its key.
//! drop(queue); // the stream closes once every task has ended
//! while let Ok(event) = rx.recv().await {
//!     if let QueueEvent::Task(_, Event::Terminated(reason)) = event {
//!         println!("task: {reason:?}");
//!     }
//! }
//! # Ok(())
//! # }
//! ```

// The queue's critical sections are short and synchronous, and tasks are spawned
// while the lock is held so a freed slot is filled before any other operation can
// observe it. This mirrors `fast-steal`'s task queue.
#![allow(clippy::significant_drop_tightening)]

mod event;

pub use event::QueueEvent;

use crate::{CancellationToken, Ctx, Event, StartPolicy, State, TerminationReason};
use crossfire::{MAsyncRx, MTx, mpmc::List};
use parking_lot::Mutex;
use std::{
    collections::{HashMap, VecDeque},
    hash::Hash,
    sync::Arc,
};
use tokio::runtime::Handle;

/// Sending half of the queue event channel, for a queue keyed by `K`.
pub type QueueTx<K> = MTx<List<QueueEvent<K>>>;
/// Receiving half of the queue event channel, for a queue keyed by `K`.
pub type QueueRx<K> = MAsyncRx<List<QueueEvent<K>>>;

/// Create an unbounded async channel for a queue's [`QueueEvent`]s, keyed by `K`.
#[must_use]
pub fn create_queue_channel<K: 'static>() -> (QueueTx<K>, QueueRx<K>) {
    crossfire::mpmc::unbounded_async()
}

/// Why a [`Queue`] operation could not be carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum QueueError {
    /// The operation needs to spawn task work and was called from outside a
    /// tokio runtime. Call it from within a runtime (`#[tokio::main]`, or inside
    /// `Handle::enter`).
    #[error("the queue needs an active tokio runtime for this operation")]
    NoRuntime,
    /// A task with this key is still waiting or running. Keys must be unique
    /// among live tasks, so a re-used key cannot clobber one.
    #[error("a task with this key is still queued")]
    AlreadyQueued,
}

/// A queue that runs downloads with a bounded concurrency, keyed by `K`.
///
/// Construct one with [`Queue::new`], feed it [`State`]s with [`Queue::push`],
/// and watch the matching [`QueueRx`]. [`Queue::cancel`] stops a queued or
/// running task by key. Cloning a `Queue` shares the same tasks.
#[derive(Clone)]
pub struct Queue<K: 'static> {
    state: Arc<Mutex<QueueState<K>>>,
    tx: QueueTx<K>,
}

struct QueueState<K> {
    /// How many tasks may download at once.
    limit: usize,
    /// Tasks waiting for a slot, in arrival order.
    pending: VecDeque<(K, PendingRun)>,
    /// Keys holding a slot, with the token that cancels the running task.
    running: HashMap<K, CancellationToken>,
}

/// Everything needed to run a task, kept as plain data so a waiting task holds
/// no reference back to the queue.
struct PendingRun {
    ctx: Ctx,
    state: State,
    policy: StartPolicy,
}

/// Is there a tokio runtime to spawn onto?
fn has_runtime() -> bool {
    Handle::try_current().is_ok()
}

impl<K> Queue<K>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
{
    /// Create a queue running at most `concurrency` tasks at once, reporting on
    /// `tx`.
    ///
    /// `concurrency` is clamped to at least `1`.
    #[must_use]
    pub fn new(concurrency: usize, tx: QueueTx<K>) -> Self {
        Self {
            state: Arc::new(Mutex::new(QueueState {
                limit: concurrency.max(1),
                pending: VecDeque::new(),
                running: HashMap::new(),
            })),
            tx,
        }
    }

    /// Add `state` to the queue under the caller's key `id`, to be resolved and
    /// run once a slot is free.
    ///
    /// `policy` decides how the probed state is started, exactly as in
    /// [`Ready::start`](crate::Ready::start).
    ///
    /// # Errors
    /// Returns [`QueueError::NoRuntime`] when called outside a tokio runtime,
    /// and [`QueueError::AlreadyQueued`] when `id` names a task that is still
    /// waiting or running.
    pub fn push(
        &self,
        id: K,
        state: State,
        policy: impl Into<StartPolicy>,
    ) -> Result<(), QueueError> {
        if !has_runtime() {
            return Err(QueueError::NoRuntime);
        }
        let run = PendingRun {
            ctx: Ctx::new(),
            state,
            policy: policy.into(),
        };
        let mut guard = self.state.lock();
        if guard.running.contains_key(&id) || guard.pending.iter().any(|(key, _)| *key == id) {
            return Err(QueueError::AlreadyQueued);
        }
        let _ = self.tx.send(QueueEvent::Queued(id.clone()));
        guard.pending.push_back((id, run));
        self.spawn_ready(&mut guard);
        Ok(())
    }

    /// Start as many waiting tasks as the limit allows.
    fn spawn_ready(&self, state: &mut QueueState<K>) {
        while state.running.len() < state.limit {
            let Some((id, run)) = state.pending.pop_front() else {
                break;
            };
            state.running.insert(id.clone(), run.ctx.token().clone());
            tokio::spawn(self.clone().drive(id, run));
        }
    }

    /// Cancel a task by key.
    ///
    /// A task still waiting for a slot is removed and reported as
    /// [`QueueEvent::Cancelled`] without ever running; a running task has its
    /// cancellation token triggered, and its own run reports the outcome.
    /// Returns whether `id` named a queued or running task. A run's terminal
    /// event and the removal of its key happen under the same lock, so `true`
    /// always means the cancellation reached a task that had not ended yet. A
    /// running task keeps its key until that point, so re-pushing the key before
    /// its terminal event still returns [`QueueError::AlreadyQueued`].
    pub fn cancel(&self, id: &K) -> bool {
        let mut guard = self.state.lock();
        if let Some(token) = guard.running.get(id) {
            token.cancel();
            return true;
        }
        if let Some(pos) = guard.pending.iter().position(|(key, _)| key == id) {
            guard.pending.remove(pos);
            // A task that never started gets `Cancelled`, not `Terminated`: the
            // "one Terminated per started task" contract stays intact.
            let _ = self.tx.send(QueueEvent::Cancelled(id.clone()));
            return true;
        }
        false
    }

    /// Run one task from slot acquisition to release.
    async fn drive(self, id: K, run: PendingRun) {
        // Free the slot even if the driver panics; a leaked key would block that
        // key forever and permanently cost one concurrency slot.
        let slot = SlotGuard {
            queue: self.clone(),
            id: id.clone(),
            armed: true,
        };
        let PendingRun { ctx, state, policy } = run;
        let _ = self.tx.send(QueueEvent::Started(id.clone()));
        // A task's own events flow on a private channel; forwarding them tags
        // each with `id` onto the queue channel.
        let (tx, rx) = crate::create_channel();
        // Resolve and run. Either way the sending half must end here so the drain
        // below terminates:
        //   - success: `start` takes `tx` and drops it when the run finishes,
        //   - failure: the failure events are still ours to publish, so emit them
        //     and drop `tx` ourselves.
        //
        // Resolving (prefetch) is watched by the cancel token too: dropping the
        // future abandons the request, so cancelling a task that is still probing
        // the remote does not wait out its retries.
        let built = tokio::select! {
            biased;
            () = ctx.token().cancelled() => None,
            built = state.build(&tx) => Some(built),
        };
        match built {
            None => drop(tx),
            Some(Ok(ready)) => {
                // `start` returns as soon as the run is spawned; its events
                // arrive on `tx` while `rx` is drained below.
                drop(ready.start(policy, tx, &ctx));
            }
            Some(Err(e)) => {
                // Keep the "one Terminated per task" contract even when the
                // resolve fails and no run is ever spawned.
                event::emit_resolve_failed(&tx, e);
                drop(tx);
            }
        }
        // Hold the terminal event back so it can be published together with the
        // slot release: a caller that re-queues the same key on `Terminated` must
        // never observe the key as still running.
        let mut terminal = None;
        while let Ok(event) = rx.recv().await {
            if matches!(event, Event::Terminated(_)) {
                terminal = Some(event);
            } else {
                let _ = self.tx.send(QueueEvent::Task(id.clone(), event));
            }
        }
        // A panicking run drops its sender before reporting; keep the
        // "exactly one `Terminated` per started task" contract.
        let terminal = terminal.unwrap_or(if ctx.token().is_cancelled() {
            Event::Terminated(TerminationReason::Cancelled)
        } else {
            Event::Terminated(TerminationReason::Failed)
        });
        slot.finish(terminal);
    }

    /// Publish `terminal` and free the slot as one step, then admit the next
    /// waiting task.
    fn finish(&self, id: &K, terminal: Event) {
        let mut guard = self.state.lock();
        let _ = self.tx.send(QueueEvent::Task(id.clone(), terminal));
        guard.running.remove(id);
        self.spawn_ready(&mut guard);
    }
}

/// Releases a task's slot on normal completion and on panic.
struct SlotGuard<K: Clone + Eq + Hash + Send + Sync + 'static> {
    queue: Queue<K>,
    id: K,
    armed: bool,
}

impl<K> SlotGuard<K>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn finish(mut self, terminal: Event) {
        self.armed = false;
        self.queue.finish(&self.id, terminal);
    }
}

impl<K> Drop for SlotGuard<K>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn drop(&mut self) {
        if self.armed {
            self.queue
                .finish(&self.id, Event::Terminated(TerminationReason::Failed));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PartialConfig, create_queue_channel};
    use url::Url;

    fn waiting(id: u8) -> (u8, PendingRun) {
        (
            id,
            PendingRun {
                ctx: Ctx::new(),
                state: State::new(
                    Url::parse("http://example.com/f.bin").unwrap(),
                    PartialConfig::default(),
                ),
                policy: StartPolicy::auto(),
            },
        )
    }

    #[tokio::test]
    async fn cancel_removes_a_waiting_task_and_reports_it() {
        let (tx, rx) = create_queue_channel::<u8>();
        let queue = Queue::new(1, tx);
        // Fill the only slot with a task that never runs here, then queue one
        // more so `cancel` sees it as waiting.
        queue
            .state
            .lock()
            .running
            .insert(0, Ctx::new().token().clone());
        queue.state.lock().pending.push_back(waiting(7));

        assert!(queue.cancel(&7));
        assert!(queue.state.lock().pending.is_empty());
        assert!(
            matches!(rx.recv().await, Ok(QueueEvent::Cancelled(7))),
            "a waiting task reports Cancelled, never Started/Terminated"
        );
    }

    #[tokio::test]
    async fn cancel_of_an_unknown_key_is_false() {
        let (tx, _rx) = create_queue_channel::<u8>();
        let queue = Queue::new(1, tx);
        assert!(!queue.cancel(&9));
    }
}
