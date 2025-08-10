use std::time::Duration;
use futures::future::LocalBoxFuture;

enum DurationTransform {
    Boxed(Box<dyn FnMut(Duration) -> Duration>),
    Local(fn(Duration) -> Duration),
}

enum RetryStrategy {
    Fixed(Duration),
    Transform(Duration, DurationTransform),
    Fn(Box<dyn FnMut() -> LocalBoxFuture<'static, ()>>)
}

pub struct PullOptions {
    write_cap: usize,
    retry_strategy: RetryStrategy,
}
