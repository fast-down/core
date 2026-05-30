use std::sync::Arc;
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

    /// # Panics
    /// Panics if the background task awaiting the handle exits unexpectedly
    ///
    /// # Errors
    /// Returns `Arc<JoinError>` if the task itself returns a `JoinError`
    pub async fn join(&self) -> Result<T, Arc<JoinError>> {
        let mut rx = self.rx.clone();
        loop {
            let res = rx.borrow_and_update().clone();
            if let Some(res) = res {
                return res;
            }
            if rx.changed().await.is_err() {
                #[allow(clippy::expect_used)]
                return rx
                    .borrow()
                    .clone()
                    .expect("SharedHandle background task panicked or was cancelled unexpectedly");
            }
        }
    }
}
