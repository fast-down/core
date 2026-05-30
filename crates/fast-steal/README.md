# fast-steal

[![GitHub last commit](https://img.shields.io/github/last-commit/fast-down/core/main)](https://github.com/fast-down/core/commits/main)
[![Test](https://github.com/fast-down/core/workflows/Test/badge.svg)](https://github.com/fast-down/core/actions)
[![Latest version](https://img.shields.io/crates/v/fast-steal.svg)](https://crates.io/crates/fast-steal)
[![Documentation](https://docs.rs/fast-steal/badge.svg)](https://docs.rs/fast-steal)
[![License](https://img.shields.io/crates/l/fast-steal.svg)](https://github.com/fast-down/core/blob/main/LICENSE)

`fast-steal` is an ultra-fast multi-threaded library with fine-grained work stealing.

## Highlights

1. `no_std` support
2. Ultra-fine-grained work stealing for maximum throughput
3. Safe Rust — no `unsafe` code
4. Full test coverage for stability and reliability

```rust
use fast_steal::{Executor, Handle, Task, TaskQueue};
use std::{collections::HashMap};
use tokio::{
    sync::mpsc,
    task::AbortHandle,
};

pub struct TokioExecutor {
    tx: mpsc::UnboundedSender<(u64, u64)>,
    speculative: usize,
}
#[derive(Clone)]
pub struct TokioHandle(AbortHandle);

impl Handle for TokioHandle {
    type Output = ();
    type Id = ();
    fn abort(&mut self) -> Self::Output {
        self.0.abort();
    }
    fn is_self(&mut self, _: &Self::Id) -> bool {
        false
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
                if !task_queue.steal(&(), &mut task, 1, speculative) {
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

#[tokio::main]
async fn main() {
    {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let executor = TokioExecutor {
            tx,
            speculative: 1,
        };
        let pre_data = [1..20, 41..48];
        let task_queue = TaskQueue::new(pre_data.iter().cloned());
        task_queue.set_threads(8, 1, Some(&executor)).unwrap();
        drop(executor);
        let mut data = HashMap::new();
        while let Some((i, res)) = rx.recv().await {
            println!("main: {i} = {res}");
            assert!(
                data.insert(i, res).is_none(),
                "number {i} with value {res} was computed twice"
            );
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

    {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let executor = TokioExecutor {
            tx,
            speculative: 2,
        };
        let pre_data = [1..20, 41..48];
        let task_queue = TaskQueue::new(pre_data.iter().cloned());
        task_queue.set_threads(8, 1, Some(&executor)).unwrap();
        drop(executor);
        let mut data = HashMap::new();
        while let Some((i, res)) = rx.recv().await {
            println!("main: {i} = {res}");
            assert!(
                data.insert(i, res).is_none(),
                "number {i} with value {res} was computed twice"
            );
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
```
