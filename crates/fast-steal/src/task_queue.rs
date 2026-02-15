extern crate alloc;
use crate::{Executor, Handle, Task, WeakTask};
use alloc::{collections::vec_deque::VecDeque, sync::Arc, vec::Vec};
use core::ops::Range;
use parking_lot::Mutex;

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
    pub fn new<'a>(tasks: impl Iterator<Item = &'a Range<u64>>) -> Self {
        let waiting: VecDeque<_> = tasks.map(Task::from).collect();
        Self {
            inner: Arc::new(Mutex::new(TaskQueueInner {
                running: VecDeque::with_capacity(waiting.len()),
                waiting,
            })),
        }
    }
    pub fn add(&self, task: Task) {
        let mut guard = self.inner.lock();
        guard.waiting.push_back(task);
    }
    pub fn steal(&self, task: &mut Task, min_chunk_size: u64, speculative: bool) -> bool {
        let min_chunk_size = min_chunk_size.max(1);
        let mut guard = self.inner.lock();
        while let Some(new_task) = guard.waiting.pop_front() {
            if let Some(range) = new_task.take() {
                task.set(range);
                return true;
            }
        }
        if let Some(steal_task) = guard
            .running
            .iter()
            .filter_map(|w| w.0.upgrade())
            .filter(|w| w != task)
            .max_by_key(|w| w.remain())
        {
            if steal_task.remain() >= min_chunk_size * 2
                && let Ok(Some(range)) = steal_task.split_two()
            {
                task.set(range);
                true
            } else if speculative && steal_task.remain() > 0 {
                task.state = steal_task.state.clone();
                true
            } else {
                false
            }
        } else {
            false
        }
    }
    /// 当线程数需要增加时，但 executor 为空时，返回 None
    pub fn set_threads<E: Executor<Handle = H>>(
        &self,
        threads: usize,
        min_chunk_size: u64,
        executor: Option<&E>,
    ) -> Option<()> {
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
                    .max_by_key(|w| w.remain())
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
    pub fn handles<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut dyn Iterator<Item = &mut H>) -> R,
    {
        let mut guard = self.inner.lock();
        let mut iter = guard.running.iter_mut().map(|w| &mut w.1);
        f(&mut iter)
    }
    pub fn cancel_tasks(&self, task: &Task) {
        let mut guard = self.inner.lock();
        for (weak, handle) in &mut guard.running {
            if let Some(t) = weak.upgrade()
                && t == *task
            {
                handle.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use crate::{Executor, Handle, Task, TaskQueue};
    use std::{collections::HashMap, dbg, println};
    use tokio::{sync::mpsc, task::AbortHandle};

    pub struct TokioExecutor {
        tx: mpsc::UnboundedSender<(u64, u64)>,
        speculative: bool,
    }
    #[derive(Clone)]
    pub struct TokioHandle(AbortHandle);

    impl Handle for TokioHandle {
        type Output = ();
        fn abort(&mut self) -> Self::Output {
            self.0.abort();
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
                    if !task_queue.steal(&mut task, 1, speculative) {
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
        let executor = TokioExecutor {
            tx,
            speculative: false,
        };
        let pre_data = [1..20, 41..48];
        let task_queue = TaskQueue::new(pre_data.iter());
        task_queue.set_threads(8, 1, Some(&executor));
        drop(executor);
        let mut data = HashMap::new();
        while let Some((i, res)) = rx.recv().await {
            println!("main: {i} = {res}");
            if data.insert(i, res).is_some() {
                panic!("数字 {i}，值为 {res} 重复计算");
            }
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
        let executor = TokioExecutor {
            tx,
            speculative: true,
        };
        let pre_data = [1..20, 41..48];
        let task_queue = TaskQueue::new(pre_data.iter());
        task_queue.set_threads(8, 1, Some(&executor));
        drop(executor);
        let mut data = HashMap::new();
        while let Some((i, res)) = rx.recv().await {
            println!("main: {i} = {res}");
            if data.insert(i, res).is_some() {
                panic!("数字 {i}，值为 {res} 重复计算");
            }
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
