#[macro_export]
macro_rules! send_err {
    ($expr:expr, $tx:expr, $event:expr) => {
        match $expr {
            Ok(r) => r,
            Err(e) => {
                let _ = $tx.send($event(Err(e))).await;
                return;
            }
        }
    };
}
#[macro_export]
macro_rules! send_err2 {
    ($expr:expr, $tx:expr, $event:expr) => {
        match $expr {
            Ok(r) => r,
            Err(e) => {
                let _ = $tx.send($event(e)).await;
                return;
            }
        }
    };
}
