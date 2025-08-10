macro_rules! poll_ok {
    (
        $prelude:block,
        $expr:expr,
        $tx:expr => $err:ident,
        $retry_gap:expr
    ) => {
        loop {
            $prelude;
            match $expr {
                Ok(value) => break value,
                Err(err) => {
                    $tx.send($crate::Event::$err(err)).await.unwrap();
                }
            }
            ::tokio::time::sleep($retry_gap).await;
        }
    };
    (
        $prelude:block,
        $expr:expr,
        $id:ident @ $tx:expr => $err:ident,
        $retry_gap:expr
    ) => {
        loop {
            $prelude;
            match $expr {
                Ok(value) => break value,
                Err(err) => {
                    $tx.send($crate::Event::$err($id, err)).await.unwrap();
                }
            }
            ::tokio::time::sleep($retry_gap).await;
        }
    };
}

pub(crate) use poll_ok;
