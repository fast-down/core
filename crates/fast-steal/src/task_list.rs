extern crate alloc;
use crate::{Executor, SplitTask, Task, executor::Handle};
use alloc::sync::Arc;
use core::ops::Range;
use dashmap::{DashMap, DashSet};

#[derive(Debug)]
pub struct TaskList<E: Executor> {
    running: DashMap<Arc<Task>, E::Handle>,
    waiting: DashSet<Arc<Task>>,
    executor: Arc<E>,
}

impl<E: Executor> TaskList<E> {
    pub fn run(
        threads: usize,
        min_chunk_size: u64,
        tasks: &[Range<u64>],
        executor: E,
    ) -> Arc<Self> {
        debug_assert!(min_chunk_size > 0, "min_chunk_size must be greater than 0");
        let t = Arc::new(Self {
            running: DashMap::with_capacity(threads),
            waiting: tasks.iter().map(Task::from).map(Arc::new).collect(),
            executor: Arc::new(executor),
        });
        t.clone().set_threads(threads, min_chunk_size);
        t
    }

    pub fn steal(&self, task: &Task, min_chunk_size: u64) -> bool {
        debug_assert!(min_chunk_size > 1, "min_chunk_size must be greater than 1");
        if let Some(new_task) = self.waiting.iter().next() {
            let new_task_arc = new_task.key().clone();
            drop(new_task);
            self.waiting.remove(&new_task_arc);
            task.set_end(new_task_arc.end());
            task.set_start(new_task_arc.start());
            return true;
        }
        if let Some(w) = self.running.iter().max_by_key(|w| w.key().remain())
            && w.key().remain() >= min_chunk_size
        {
            let (start, end) = w.key().split_two();
            task.set_end(end);
            task.set_start(start);
            true
        } else {
            false
        }
    }

    pub fn set_threads(self: Arc<Self>, threads: usize, min_chunk_size: u64) {
        debug_assert!(threads > 0, "threads must be greater than 0");
        debug_assert!(min_chunk_size > 0, "min_chunk_size must be greater than 0");
        let len = self.running.len();
        if len < threads {
            let mut need = threads - len;
            self.waiting.retain(|task| {
                if need > 0 {
                    let handle = self.executor.clone().execute(task.clone(), self.clone());
                    self.running.insert(task.clone(), handle);
                    need -= 1;
                    false
                } else {
                    true
                }
            });
            while threads > self.running.len()
                && let Some(w) = self.running.iter().max_by_key(|w| w.key().remain())
                && w.key().remain() >= min_chunk_size
            {
                let (start, end) = w.key().split_two();
                let task = Arc::new(Task::new(start, end));
                let handle = self.executor.clone().execute(task.clone(), self.clone());
                self.running.insert(task, handle);
            }
        } else if len > threads {
            let mut need_remove = len - threads;
            self.running.retain(|task, handle| {
                if need_remove > 0 {
                    handle.abort();
                    self.waiting.insert(task.clone());
                    need_remove -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }

    pub fn handles(&self) -> Arc<[E::Handle]> {
        self.running.iter().map(|w| w.value().clone()).collect()
    }
}
