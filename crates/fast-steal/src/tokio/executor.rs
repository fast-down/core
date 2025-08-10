extern crate alloc;
extern crate spin;
use crate::{SplitTask, Task, task_list::TaskList, tokio::TokioRunner};
use alloc::{sync::Arc, vec::Vec};
use spin::mutex::SpinMutex;
use tokio::task::AbortHandle;

pub struct TokioExecutor<Runner: TokioRunner> {
    task_list: TaskList,
    tasks: Arc<SpinMutex<Vec<Task>>>,
    runner: Runner,
    abort_handles: Vec<AbortHandle>,
    min_chunk_size: u64,
    threads: u64,
}

impl<Runner: TokioRunner + 'static> TokioExecutor<Runner> {
    pub fn new(task_list: TaskList, threads: u64, min_chunk_size: u64, runner: Runner) -> Self {
        Self {
            tasks: Arc::new(spin::mutex::SpinMutex::new(
                Task::from(&task_list).split_task(threads).collect(),
            )),
            task_list,
            runner,
            abort_handles: Vec::with_capacity(threads as usize),
            min_chunk_size,
            threads,
        }
    }

    pub async fn run(&mut self) -> Result<(), Runner::Error> {
        for (id, task) in self.tasks.lock().iter().cloned().enumerate() {
            let mut runner = self.runner.clone();
            let tasks = self.tasks.clone();
            let task_list = self.task_list.clone();
            let handle = tokio::spawn(async move { runner.run(id, task, tasks, task_list).await });
            self.abort_handles.push(handle.abort_handle());
        }
        Ok(())
    }

    pub fn abort(&mut self) {
        for handle in self.abort_handles.drain(..) {
            handle.abort();
        }
    }

    pub fn set_threads(&mut self, threads: u64) {
        let mut tasks = self.tasks.lock();
        while threads > self.threads {
            let (max_pos, max_remain) = tasks
                .iter()
                .enumerate()
                .map(|(i, w)| (i, w.remain()))
                .max_by_key(|(_, remain)| *remain)
                .unwrap_or((0, 0));
            if max_remain < self.min_chunk_size {
                break;
            }
            self.threads += 1;
            let (start, end) = tasks[max_pos].split_two();
            tasks.push(Task::new(start, end));
        }
        todo!("实现减少线程数");
        // while threads < self.threads {
        // }
    }
}
