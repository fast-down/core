#![no_std]
//! # fast-steal 神偷
//!
//! [![GitHub last commit](https://img.shields.io/github/last-commit/fast-down/core/stable)](https://github.com/fast-down/core/commits/stable)
//! [![Test](https://github.com/fast-down/core/workflows/Test/badge.svg)](https://github.com/fast-down/core/actions)
//! [![Latest version](https://img.shields.io/crates/v/fast-steal.svg)](https://crates.io/crates/fast-steal)
//! [![Documentation](https://docs.rs/fast-steal/badge.svg)](https://docs.rs/fast-steal)
//! [![License](https://img.shields.io/crates/l/fast-steal.svg)](https://github.com/fast-down/core/blob/stable/crates/fast-steal/LICENSE)
//!
//! `fast-steal` 是一个特别快的多线程库，支持超细颗粒度的任务窃取。
//!
//! ## 优势
//!
//! 1. no_std 支持，不依赖于标准库
//! 2. 超细颗粒度任务窃取，速度非常快
//! 3. 安全的 Rust，库中没有使用任何 unsafe 的代码
//! 4. 测试完全覆盖，保证库的稳定性和可靠性
//!
//! ```rust
//! use fast_steal::{Executor, Handle, Task, TaskList};
//! use std::{collections::HashMap, sync::Arc, num::NonZero};
//! use tokio::{
//!     sync::{Mutex, mpsc},
//!     task::{AbortHandle, JoinHandle},
//! };
//!
//! pub struct TokioExecutor {
//!     tx: mpsc::UnboundedSender<(u64, u64)>,
//! }
//! #[derive(Clone)]
//! pub struct TokioHandle(Arc<Mutex<Option<JoinHandle<()>>>>, AbortHandle);
//!
//! impl Handle for TokioHandle {
//!     type Output = ();
//!     fn abort(&mut self) -> Self::Output {
//!         self.1.abort();
//!     }
//! }
//!
//! impl Executor for TokioExecutor {
//!     type Handle = TokioHandle;
//!     fn execute(self: Arc<Self>, task: Arc<Task>, task_list: Arc<TaskList<Self>>) -> Self::Handle {
//!         println!("execute");
//!         let handle = tokio::spawn(async move {
//!             loop {
//!                 while task.start() < task.end() {
//!                     let i = task.start();
//!                     task.fetch_add_start(1);
//!                     let res = fib(i);
//!                     println!("task: {i} = {res}");
//!                     self.tx.send((i, res)).unwrap();
//!                 }
//!                 if !task_list.steal(&task, NonZero::new(2).unwrap()) {
//!                     break;
//!                 }
//!             }
//!             assert_eq!(task_list.remove(&task), 1);
//!         });
//!         let abort_handle = handle.abort_handle();
//!         TokioHandle(Arc::new(Mutex::new(Some(handle))), abort_handle)
//!     }
//! }
//!
//! fn fib(n: u64) -> u64 {
//!     match n {
//!         0 => 0,
//!         1 => 1,
//!         _ => fib(n - 1) + fib(n - 2),
//!     }
//! }
//! fn fib_fast(n: u64) -> u64 {
//!     let mut a = 0;
//!     let mut b = 1;
//!     for _ in 0..n {
//!         (a, b) = (b, a + b);
//!     }
//!     a
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     let (tx, mut rx) = mpsc::unbounded_channel();
//!     let executor = Arc::new(TokioExecutor { tx });
//!     let pre_data = [1..20, 41..48];
//!     let task_list = Arc::new(TaskList::run(&pre_data[..], executor));
//!     task_list
//!         .clone()
//!         .set_threads(NonZero::new(8).unwrap(), NonZero::new(2).unwrap());
//!     let handles: Arc<[_]> = task_list.handles(|it| it.collect());
//!     drop(task_list);
//!     for handle in handles.iter() {
//!         handle.0.lock().await.take().unwrap().await.unwrap();
//!     }
//!     let mut data = HashMap::new();
//!     while let Some((i, res)) = rx.recv().await {
//!         println!("main: {i} = {res}");
//!         if data.insert(i, res).is_some() {
//!             panic!("数字 {i}，值为 {res} 重复计算");
//!         }
//!     }
//!     dbg!(&data);
//!     for range in pre_data {
//!         for i in range {
//!             assert_eq!((i, data.get(&i)), (i, Some(&fib_fast(i))));
//!             data.remove(&i);
//!         }
//!     }
//!     assert_eq!(data.len(), 0);
//! }
//! ```

mod executor;
mod task;
mod task_list;

pub use executor::{Executor, Handle};
pub use task::Task;
pub use task_list::TaskList;
