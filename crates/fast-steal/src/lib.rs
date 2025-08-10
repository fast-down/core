#![no_std]
//! # fast-steal 神偷
//!
//! ![GitHub last commit (branch)](https://img.shields.io/github/last-commit/fast-down/fast-steal/master)
//! [![Rust](https://github.com/fast-down/fast-steal/workflows/Test/badge.svg)](https://github.com/fast-down/fast-steal/actions)
//! [![Latest version](https://img.shields.io/crates/v/fast-steal.svg)](https://crates.io/crates/fast-steal)
//! [![Documentation](https://docs.rs/fast-steal/badge.svg)](https://docs.rs/fast-steal)
//! ![License](https://img.shields.io/crates/l/fast-steal.svg)
//!
//! `fast-steal` 是一个特别快的多线程库，支持超细颗粒度的任务窃取。
//!
//! ## 优势
//!
//! 1. no_std 支持，不依赖于标准库
//! 2. 超细颗粒度任务窃取，速度非常快
//! 3. 零依赖，不依赖任何第三方库
//! 4. 零拷贝，库中没有任何 clone 操作
//! 5. 安全的 Rust，库中没有使用任何 unsafe 的代码
//! 6. 无锁，库中没有使用任何锁（但是在任务重新分配时 `task.steal()`，记得手动加锁）
//! 7. 测试完全覆盖，保证库的稳定性和可靠性
//!
//! ```rust
//! use fast_steal::{SplitTask, StealTask, Task, TaskList};
//! use std::{
//!     collections::HashMap,
//!     sync::{Arc, Mutex, mpsc},
//!     thread,
//! };
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
//! fn main() {
//!     let (tx, rx) = mpsc::channel();
//!     let mutex = Arc::new(Mutex::new(()));
//!     // 任务数据列表
//!     let task_list = Arc::new(TaskList::from(&[1..20, 41..48][..]));
//!     // 分配 8 个任务
//!     let tasks = Arc::from_iter(
//!         Task::from(&*task_list)
//!             .split_task(8)
//!             .map(Arc::new)
//!     );
//!     let mut handles = Vec::with_capacity(tasks.len());
//!     for task in tasks.iter() {
//!         let task = task.clone();
//!         let tasks = tasks.clone();
//!         let task_list = task_list.clone();
//!         let mutex = mutex.clone();
//!         let tx = tx.clone();
//!         let handle = thread::spawn(move || {
//!             loop {
//!                 // 必须在每次循环开始判断 task.start() < task.end()，因为其他线程可能会修改 task
//!                 while task.start() < task.end() {
//!                     let i = task_list.get(task.start());
//!                     // 提前更新进度，防止其他线程重复计算
//!                     task.fetch_add_start(1);
//!                     // 计算
//!                     tx.send((i, fib(i))).unwrap();
//!                 }
//!                 // 检查是否还有任务
//!                 // ⚠️注意：这里需要加锁，防止多个线程同时检查任务列表
//!                 let _guard = mutex.lock().unwrap();
//!                 if !task.steal(&tasks, 2) {
//!                     return;
//!                 }
//!                 // 这里需要释放锁
//!             }
//!         });
//!         handles.push(handle);
//!     }
//!     // 汇总任务结果
//!     let mut data = HashMap::new();
//!     // ⚠️注意：这里要 drop(tx) 否则永远会卡在 for (i, res) in rx {} 这里
//!     drop(tx);
//!     for (i, res) in rx {
//!         // 如果重复计算就报错
//!         if data.insert(i, res).is_some() {
//!             panic!("数字 {i}，值为 {res} 重复计算");
//!         }
//!     }
//!     // 等待任务结束
//!     for handle in handles {
//!         handle.join().unwrap();
//!     }
//!     // 验证结果
//!     dbg!(&data);
//!     for i in 0..task_list.len {
//!         let index = task_list.get(i);
//!         assert_eq!((index, data.get(&index)), (index, Some(&fib_fast(index))));
//!     }
//! }
//! ```

mod split_task;
mod steal_task;
mod task;
mod task_list;

pub use split_task::SplitTask;
pub use steal_task::StealTask;
pub use task::Task;
pub use task_list::TaskList;
