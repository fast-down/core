extern crate alloc;
use crate::{Executor, Handle, Task};
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
    running: VecDeque<(Task, H)>,
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
    /// 仅供 Worker 线程在任务完成后调用
    pub fn finish_work(&self, task: &Task) -> usize {
        let mut guard = self.inner.lock();
        let len = guard.running.len();
        guard.running.retain(|(t, _)| t != task);
        len - guard.running.len()
    }
    /// 用于从外部取消任务
    pub fn cancel(&self, task: &Task) -> usize {
        let mut guard = self.inner.lock();
        let len = guard.running.len() + guard.waiting.len();
        guard.running.retain(|(t, _)| t != task);
        guard.waiting.retain(|t| t != task);
        len - guard.running.len() - guard.waiting.len()
    }
    pub fn add(&self, task: Task) {
        let mut guard = self.inner.lock();
        guard.waiting.push_back(task);
    }
    pub fn steal(&self, task: &Task, min_chunk_size: u64) -> bool {
        let min_chunk_size = min_chunk_size.max(1);
        let mut guard = self.inner.lock();
        while let Some(new_task) = guard.waiting.pop_front() {
            if let Some(range) = new_task.take() {
                task.set(range);
                return true;
            }
        }
        if let Some(steal_task) = guard.running.iter().map(|w| &w.0).max()
            && steal_task.remain() >= min_chunk_size * 2
            && let Ok(Some(range)) = steal_task.split_two()
        {
            task.set(range);
            true
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
        let threads = threads.max(1);
        let min_chunk_size = min_chunk_size.max(1);
        let mut guard = self.inner.lock();
        let len = guard.running.len();
        if len < threads {
            let executor = executor?;
            let need = (threads - len).min(guard.waiting.len());
            let mut temp = Vec::with_capacity(need);
            let iter = guard.waiting.drain(..need);
            for task in iter {
                let handle = executor.execute(task.clone(), self.clone());
                temp.push((task, handle));
            }
            guard.running.extend(temp);
            while guard.running.len() < threads
                && let Some(steal_task) = guard.running.iter().map(|w| &w.0).max()
                && steal_task.remain() >= min_chunk_size * 2
                && let Ok(Some(range)) = steal_task.split_two()
            {
                let task = Task::new(range);
                let handle = executor.execute(task.clone(), self.clone());
                guard.running.push_back((task, handle));
            }
        } else if len > threads {
            let mut temp = Vec::with_capacity(len - threads);
            let iter = guard.running.drain(threads..);
            for (task, mut handle) in iter {
                handle.abort();
                temp.push(task);
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
}

#[cfg(test)]
mod tests {
    extern crate std;
    use crate::{Executor, Handle, Task, TaskQueue};
    use std::{collections::HashMap, dbg, println};
    use tokio::{sync::mpsc, task::AbortHandle};

    pub struct TokioExecutor {
        tx: mpsc::UnboundedSender<(u64, u64)>,
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
        fn execute(&self, task: Task, task_queue: TaskQueue<Self::Handle>) -> Self::Handle {
            println!("execute");
            let tx = self.tx.clone();
            let handle = tokio::spawn(async move {
                loop {
                    while task.start() < task.end() {
                        let i = task.start();
                        task.fetch_add_start(1).unwrap();
                        let res = fib(i);
                        println!("task: {i} = {res}");
                        tx.send((i, res)).unwrap();
                    }
                    if !task_queue.steal(&task, 1) {
                        break;
                    }
                }
                assert_eq!(task_queue.finish_work(&task), 1);
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

    #[tokio::test]
    async fn test_task_queue() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let executor = TokioExecutor { tx };
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
