use crate::{Task, TaskQueue};

/// User-defined executor that runs tasks on a [`TaskQueue`].
///
/// Implement this trait to integrate with any async runtime (e.g. tokio, smol).
/// The associated [`Handle`] type is used for task cancellation and identification.
pub trait Executor {
    type Handle: Handle;
    fn execute(&self, task: Task, task_queue: TaskQueue<Self::Handle>) -> Self::Handle;
}

/// Handle returned by [`Executor::execute`], used to abort or identify a running task.
pub trait Handle {
    type Output;
    type Id;
    fn abort(&mut self) -> Self::Output;
    fn is_self(&mut self, id: &Self::Id) -> bool;
}
