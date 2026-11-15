//! The public event types a [`Queue`](super::Queue) publishes.
//!
//! These are the *descriptions* of what happened to a task, kept apart from the
//! scheduling machinery in [`super`]: a [`QueueEvent`] says what the queue did.

use crate::{Event, PlanError, TerminationReason, Tx};

/// The events a [`Queue`](super::Queue) publishes, delivered on the channel
/// returned by [`create_queue_channel`](super::create_queue_channel).
///
/// A task contributes events only once it has started: `Started` opens its run
/// and exactly one [`Event::Terminated`] closes it. A task still waiting when
/// the queue is abandoned emits only its [`Queued`](QueueEvent::Queued) notification, never runs, and
/// leaves no files on disk, so a download manager can keep its own manifest and
/// re-adopt that task on the next start. A waiting task removed with
/// [`Queue::cancel`](super::Queue::cancel) emits
/// [`Cancelled`](QueueEvent::Cancelled) instead of `Started`/`Terminated`.
#[derive(Debug)]
#[non_exhaustive]
pub enum QueueEvent<K> {
    /// A task was accepted and is waiting for a free slot (or already started).
    Queued(K),
    /// A task acquired a slot and began resolving the remote. Emitted before any
    /// of its [`Task`](QueueEvent::Task) events, and only for tasks that started.
    Started(K),
    /// A waiting task was cancelled before it started.
    ///
    /// Emitted instead of `Started`/`Terminated`: the task never ran and left no
    /// files on disk, so a manager can drop it from its manifest.
    Cancelled(K),
    /// An engine [`Event`] from one task's run.
    Task(K, Event),
}

/// Emit the terminal event for a task whose resolve failed, so a task that never
/// reached a run still reports why (mirroring what an engine run emits) while
/// keeping the "exactly one `Terminated` per task" contract.
pub(super) fn emit_resolve_failed(tx: &Tx, error: PlanError) {
    error.emit(tx);
    let _ = tx.send(Event::Terminated(TerminationReason::Failed));
}
