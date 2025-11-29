extern crate alloc;
use alloc::sync::Arc;
use tokio::{
    sync::watch,
    task::{JoinError, JoinHandle},
};

#[derive(Debug)]
pub struct SharedHandle<T> {
    rx: watch::Receiver<Option<Result<T, Arc<JoinError>>>>,
}

impl<T> SharedHandle<T>
where
    T: Clone + Send + Sync + 'static,
{
    pub fn new(handle: JoinHandle<T>) -> Self {
        let (tx, rx) = watch::channel(None);
        tokio::spawn(async move {
            let _ = tx.send(Some(handle.await.map_err(Arc::from)));
        });
        Self { rx }
    }

    pub async fn join(&self) -> Result<T, Arc<JoinError>> {
        let mut rx = self.rx.clone();
        loop {
            if let Some(res) = rx.borrow_and_update().clone() {
                return res;
            }
            if rx.changed().await.is_err() {
                return match rx.borrow().clone() {
                    Some(res) => res,
                    None => panic!(
                        "SharedHandle background task panicked or was cancelled unexpectedly"
                    ),
                };
            }
        }
    }
}
