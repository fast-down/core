#[macro_export]
#[doc(hidden)]
macro_rules! tx_err {
    ($x: expr, $tx: expr, $event: ident) => {
        match $x {
            Ok(r) => r,
            Err(e) => {
                let _ = $tx.send(Event::$event(e));
                return;
            }
        }
    };
}
