use dashmap::DashMap;
use fast_steal::{Executor, Handle, Task, TaskList};
use std::sync::Arc;
use tokio::{
    sync::Mutex,
    task::{AbortHandle, JoinHandle},
};

pub struct TokioExecutor {
    pub mutex: Mutex<()>,
    pub data: Arc<DashMap<u64, u64>>,
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
    fn execute(self: Arc<Self>, task: Arc<Task>, task_list: Arc<TaskList<Self>>) -> Self::Handle {
        let handle = tokio::spawn(async move {
            loop {
                while task.start() < task.end() {
                    let i = task.start();
                    task.fetch_add_start(1);
                    let res = fib(i);
                    println!("{i} = {res}");
                    if self.data.insert(i, res).is_some() {
                        panic!("数字 {i}，值为 {res} 重复计算");
                    }
                }
                let _guard = self.mutex.lock().await;
                if !task_list.steal(&task, 2) {
                    break;
                }
            }
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

#[tokio::main]
async fn main() {
    let mutex = Mutex::new(());
    let data = Arc::new(DashMap::new());
    let executor = TokioExecutor {
        mutex,
        data: data.clone(),
    };
    let pre_data = [1..20, 41..48];
    let task_list = TaskList::run(8, 1, &pre_data[..], executor);
    let handles = task_list.handles();
    for handle in handles.iter() {
        handle.0.lock().await.take().unwrap().await.unwrap();
    }
    dbg!(&data);
    for range in pre_data {
        for i in range {
            assert_eq!((i, data.get(&i).as_deref()), (i, Some(&fib_fast(i))));
        }
    }
}
