extern crate alloc;
extern crate spin;
use crate::{Task, task_list::TaskList};
use alloc::{sync::Arc, vec::Vec};
use core::future::Future;
use spin::mutex::SpinMutex;

pub trait TokioRunner: Send + Clone {
    type Error: Send;
    fn run(
        &mut self,
        id: usize,
        task: Task,
        tasks: Arc<SpinMutex<Vec<Task>>>,
        task_list: TaskList,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}
