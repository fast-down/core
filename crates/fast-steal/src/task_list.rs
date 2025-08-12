extern crate alloc;
extern crate spin;
use crate::{Executor, SplitTask, Task, executor::Handle};
use alloc::{sync::Arc, vec::Vec};
use core::ops::Range;
use spin::mutex::SpinMutex;

#[derive(Debug)]
pub struct TaskList<E: Executor> {
    running: SpinMutex<Vec<(Arc<Task>, E::Handle)>>,
    waiting: SpinMutex<Vec<Arc<Task>>>,
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
            running: SpinMutex::new(Vec::with_capacity(threads)),
            waiting: SpinMutex::new(tasks.iter().map(Task::from).map(Arc::new).collect()),
            executor: Arc::new(executor),
        });
        t.clone().set_threads(threads, min_chunk_size);
        t
    }

    pub fn remove(&self, task: &Task) -> usize {
        let mut running_guard = self.running.lock();
        let len = running_guard.len();
        running_guard.retain(|(t, _)| &**t != task);
        len - running_guard.len()
    }

    pub fn steal(&self, task: &Task, min_chunk_size: u64) -> bool {
        debug_assert!(min_chunk_size > 0, "min_chunk_size must be greater than 0");
        let running_guard = self.running.lock();
        let mut waiting_guard = self.waiting.lock();
        if let Some(new_task) = waiting_guard.pop() {
            task.set_end(new_task.end());
            task.set_start(new_task.start());
            return true;
        }
        if let Some(w) = running_guard.iter().max_by_key(|w| w.0.remain())
            && w.0.remain() >= min_chunk_size
        {
            let (start, end) = w.0.split_two();
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
        let mut running_guard = self.running.lock();
        let mut waiting_guard = self.waiting.lock();
        let len = running_guard.len();
        if len < threads {
            let need = (threads - len).min(waiting_guard.len());
            let iter = waiting_guard.drain(..need);
            for task in iter {
                let handle = self.executor.clone().execute(task.clone(), self.clone());
                running_guard.push((task.clone(), handle));
            }
            while threads > running_guard.len()
                && let Some(w) = running_guard.iter().max_by_key(|w| w.0.remain())
                && w.0.remain() >= min_chunk_size
            {
                let (start, end) = w.0.split_two();
                let task = Arc::new(Task::new(start, end));
                let handle = self.executor.clone().execute(task.clone(), self.clone());
                running_guard.push((task, handle));
            }
        } else if len > threads {
            let need_remove = len - threads;
            let iter = running_guard.drain(..need_remove);
            for (task, mut handle) in iter {
                handle.abort();
                waiting_guard.push(task.clone());
            }
        }
    }

    pub fn handles(&self) -> Arc<[E::Handle]> {
        self.running.lock().iter().map(|w| w.1.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use crate::{Executor, Handle, Task, TaskList};
    use std::{collections::HashMap, dbg, println, sync::Arc};
    use tokio::{
        sync::{Mutex, mpsc},
        task::{AbortHandle, JoinHandle},
    };

    pub struct TokioExecutor {
        tx: mpsc::UnboundedSender<(u64, u64)>,
    }
    #[derive(Clone)]
    pub struct TokioHandle(Arc<Mutex<Option<JoinHandle<()>>>>, AbortHandle);

    impl Handle for TokioHandle {
        type Output = ();
        fn abort(&mut self) -> Self::Output {
            self.1.abort();
        }
    }

    impl Executor for TokioExecutor {
        type Handle = TokioHandle;
        fn execute(
            self: Arc<Self>,
            task: Arc<Task>,
            task_list: Arc<TaskList<Self>>,
        ) -> Self::Handle {
            println!("execute");
            let handle = tokio::spawn(async move {
                loop {
                    while task.start() < task.end() {
                        let i = task.start();
                        task.fetch_add_start(1);
                        let res = fib(i);
                        println!("task: {i} = {res}");
                        self.tx.send((i, res)).unwrap();
                    }
                    if !task_list.steal(&task, 2) {
                        break;
                    }
                }
                assert_eq!(task_list.remove(&task), 1);
            });
            let abort_handle = handle.abort_handle();
            TokioHandle(Arc::new(Mutex::new(Some(handle))), abort_handle)
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
    async fn test_task_list() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let executor = TokioExecutor { tx };
        let pre_data = [1..20, 41..48];
        let task_list = TaskList::run(8, 2, &pre_data[..], executor);
        let handles = task_list.handles();
        drop(task_list);
        for handle in handles.iter() {
            handle.0.lock().await.take().unwrap().await.unwrap();
        }
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
