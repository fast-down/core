//! A shareable, multi-consumer handle to a tokio task.

use std::sync::Arc;
use tokio::{
    sync::watch,
    task::{JoinError, JoinHandle},
};

/// A shareable handle to a tokio task that can be awaited from multiple consumers.
///
/// Unlike a raw [`JoinHandle`], [`SharedHandle`] can be cloned and awaited
/// concurrently without consuming the result. The first awaiter gets the result,
/// subsequent awaiters will see the same cached result.
#[derive(Debug, Clone)]
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
                    .expect("SharedHandle: watch channel closed without a result — the inner JoinHandle observer panicked or was dropped");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Every cloned handle must observe the *same* cached result, not a
    // recomputed or distinct value (lines 36-51).
    #[tokio::test]
    async fn shared_handle_returns_same_value_to_all_clones() {
        let payload: Arc<[u8]> = Arc::from(vec![7u8, 8, 9]);
        let handle = SharedHandle::new(tokio::spawn({
            let payload = payload.clone();
            async move { payload }
        }));

        let h2 = handle.clone();
        let h3 = handle.clone();
        let r1 = handle.join().await.unwrap();
        let r2 = h2.join().await.unwrap();
        let r3 = h3.join().await.unwrap();

        assert!(Arc::ptr_eq(&r1, &r2));
        assert!(Arc::ptr_eq(&r2, &r3));
        assert_eq!(&*r1, &[7u8, 8, 9]);
    }

    // A panicking worker must surface as `Err(JoinError)` to the awaiter, never
    // propagate the panic into `join()` itself (the inner `JoinError` is mapped
    // to `Some(Err(..))` in `new`, so the cached result is an error, not a panic).
    #[tokio::test]
    async fn shared_handle_propagates_inner_panic_as_err() {
        let handle = SharedHandle::new(tokio::spawn(async { panic!("boom") }));
        let result = handle.join().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().is_panic());
    }

    // Re-awaiting after the result is cached must not re-run the task.
    #[tokio::test]
    async fn shared_handle_join_is_idempotent_after_completion() {
        let counter = Arc::new(AtomicUsize::new(0));
        let handle = SharedHandle::new(tokio::spawn({
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                42u32
            }
        }));
        assert_eq!(handle.join().await.unwrap(), 42);
        assert_eq!(handle.join().await.unwrap(), 42);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
}
