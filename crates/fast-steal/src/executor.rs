use crate::{Task, TaskQueue};

pub trait Executor {
    type Handle: Handle;
    fn execute(&self, task: Task, task_queue: TaskQueue<Self::Handle>) -> Self::Handle;
}

pub trait Handle {
    type Output;
    fn abort(&mut self) -> Self::Output;
}
