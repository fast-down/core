macro_rules! poll_ok {
    (
        $expr:expr,
        $tx:expr => $err:ident,
        $retry_gap:expr
    ) => {
        loop {
            match $expr {
                Ok(value) => break value,
                Err(err) => {
                    let _ = $tx.send($crate::Event::$err(err));
                }
            }
            ::tokio::time::sleep($retry_gap).await;
        }
    };
    (
        $expr:expr,
        $id:ident @ $tx:expr => $err:ident,
        $retry_gap:expr
    ) => {
        loop {
            match $expr {
                Ok(value) => break value,
                Err(err) => {
                    let _ = $tx.send($crate::Event::$err($id, err));
                }
            }
            ::tokio::time::sleep($retry_gap).await;
        }
    };
}

pub(crate) use poll_ok;
