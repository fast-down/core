use std::{collections::HashMap, sync::Arc};

use fast_steal::{Executor, Handle, Task, TaskList};
use tokio::{
    sync::{Mutex, mpsc},
    task::JoinHandle,
};

pub struct TokioExecutor {
    pub tx: mpsc::UnboundedSender<(u64, u64)>,
    pub mutex: Arc<Mutex<()>>,
}
pub struct TokioHandle(JoinHandle<()>);

impl Handle for TokioHandle {
    type Output = ();
    fn abort(&mut self) -> Self::Output {
        self.0.abort();
    }
}

impl Executor for TokioExecutor {
    type Handle = TokioHandle;
    fn execute(self: Arc<Self>, task: Arc<Task>, task_list: Arc<TaskList<Self>>) -> Self::Handle {
        println!("start");
        TokioHandle(tokio::spawn(async move {
            loop {
                while task.start() < task.end() {
                    let i = task.start();
                    // 提前更新进度，防止其他线程重复计算
                    task.fetch_add_start(1);
                    // 计算
                    self.tx.send((i, fib(i))).unwrap();
                }
                // 检查是否还有任务
                // ⚠️注意：这里需要加锁，防止多个线程同时检查任务列表
                let _guard = self.mutex.lock().await;
                if !task_list.steal(&task, 2) {
                    return;
                }
                // 这里需要释放锁
            }
        }))
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
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mutex = Arc::new(Mutex::new(()));
    let executor = TokioExecutor { tx, mutex };
    let pre_data = [1..20, 41..48];
    TaskList::run(8, 1, &pre_data[..], executor);
    let mut data = HashMap::new();
    while let Some((i, res)) = rx.recv().await {
        println!("{i} = {res}");
        if data.insert(i, res).is_some() {
            panic!("数字 {i}，值为 {res} 重复计算");
        }
    }
    dbg!(&data);
    for range in pre_data {
        for i in range {
            assert_eq!((i, data.get(&i)), (i, Some(&fib_fast(i))));
        }
    }
}
