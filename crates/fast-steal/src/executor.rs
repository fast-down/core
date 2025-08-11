extern crate alloc;
use crate::{Task, TaskList};
use alloc::sync::Arc;

pub trait Executor: Sized {
    type Handle: Handle;
    fn execute(self: Arc<Self>, task: Arc<Task>, task_list: Arc<TaskList<Self>>) -> Self::Handle;
}

pub trait Handle: Clone {
    type Output;
    fn abort(&mut self) -> Self::Output;
}
