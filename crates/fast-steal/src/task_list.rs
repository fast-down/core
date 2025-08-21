extern crate alloc;
extern crate spin;
use crate::{Executor, Task, executor::Handle};
use alloc::{collections::vec_deque::VecDeque, sync::Arc, vec::Vec};
use core::{
    num::{NonZeroU64, NonZeroUsize},
    ops::Range,
};
use spin::mutex::SpinMutex;

pub struct TaskList<E: Executor> {
    queue: SpinMutex<TaskQueue<E>>,
    executor: Arc<E>,
}

struct TaskQueue<E: Executor> {
    running: VecDeque<(Arc<Task>, E::Handle)>,
    waiting: VecDeque<Arc<Task>>,
}

impl<E: Executor> TaskList<E> {
    pub fn run(tasks: &[Range<u64>], executor: Arc<E>) -> Self {
        Self {
            queue: SpinMutex::new(TaskQueue {
                running: VecDeque::with_capacity(tasks.len()),
                waiting: tasks.iter().map(Task::from).map(Arc::new).collect(),
            }),
            executor,
        }
    }

    pub fn remove(&self, task: &Task) -> usize {
        let mut guard = self.queue.lock();
        let len = guard.running.len() + guard.waiting.len();
        guard.running.retain(|(t, _)| &**t != task);
        guard.waiting.retain(|t| &**t != task);
        len - guard.running.len() - guard.waiting.len()
    }

    pub fn add(self: Arc<Self>, task: Arc<Task>) {
        let mut guard = self.queue.lock();
        guard.waiting.push_back(task);
    }

    pub fn steal(&self, task: &Task, min_chunk_size: NonZeroU64) -> bool {
        let min_chunk_size = min_chunk_size.get();
        let mut guard = self.queue.lock();
        if let Some(new_task) = guard.waiting.pop_front() {
            task.set_end(new_task.end());
            task.set_start(new_task.start());
            return true;
        }
        if let Some(steal_task) = guard.running.iter().map(|w| w.0.clone()).max()
            && steal_task.remain() >= min_chunk_size
        {
            let (start, end) = steal_task.split_two();
            task.set_end(end);
            task.set_start(start);
            true
        } else {
            false
        }
    }

    pub fn set_threads(self: Arc<Self>, threads: NonZeroUsize, min_chunk_size: NonZeroU64) {
        let threads = threads.get();
        let min_chunk_size = min_chunk_size.get();
        let mut guard = self.queue.lock();
        let len = guard.running.len();
        if len < threads {
            let need = (threads - len).min(guard.waiting.len());
            let mut temp = Vec::with_capacity(need);
            let iter = guard.waiting.drain(..need);
            for task in iter {
                let handle = self.executor.clone().execute(task.clone(), self.clone());
                temp.push((task.clone(), handle));
            }
            guard.running.extend(temp);
            while guard.running.len() < threads
                && let Some(steal_task) = guard.running.iter().map(|w| w.0.clone()).max()
                && steal_task.remain() >= min_chunk_size
            {
                let (start, end) = steal_task.split_two();
                let task = Arc::new(Task::new(start, end));
                let handle = self.executor.clone().execute(task.clone(), self.clone());
                guard.running.push_back((task, handle));
            }
        } else if len > threads {
            let need_remove = len - threads;
            let mut temp = Vec::with_capacity(need_remove);
            let iter = guard.running.drain(..need_remove);
            for (task, mut handle) in iter {
                handle.abort();
                temp.push(task.clone());
            }
            guard.waiting.extend(temp);
        }
    }

    pub fn handles<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut dyn Iterator<Item = E::Handle>) -> R,
    {
        let guard = self.queue.lock();
        let mut iter = guard.running.iter().map(|w| w.1.clone());
        f(&mut iter)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use crate::{Executor, Handle, Task, TaskList};
    use core::num::NonZero;
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
                    if !task_list.steal(&task, NonZero::new(2).unwrap()) {
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
        let executor = Arc::new(TokioExecutor { tx });
        let pre_data = [1..20, 41..48];
        let task_list = Arc::new(TaskList::run(&pre_data[..], executor));
        task_list
            .clone()
            .set_threads(NonZero::new(8).unwrap(), NonZero::new(2).unwrap());
        let handles: Arc<[_]> = task_list.handles(|it| it.collect());
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
