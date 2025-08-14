use fast_down::curl::worker::{DataSignal, Op, multi, options, SIG_EVENT, STATE_SEND_FAILED};
use kanal::ReceiveError;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub fn main() {
    let (tx_ops, rx_ops) = kanal::unbounded();
    let handle = thread::spawn(move || {
        multi(rx_ops).unwrap();
    });
    let (tx_ret, ret) = oneshot::channel();
    let (tx_data, rx_data) = kanal::bounded(1);
    let signal: Arc<DataSignal> = Default::default();
    tx_ops
        .send(Op::New(
            tx_data,
            options::New {
                signal: signal.clone(),
                headers: curl::easy::List::new(),
                url: "https://github.com/".to_string(),
                extra: None,
            },
            tx_ret,
        ))
        .unwrap();
    let th = ret.recv().unwrap();
    loop {
        if signal.is_send_failed() {
            tx_ops.send(Op::UnpauseData(th)).unwrap();
        }

        match rx_data.try_recv() {
            Ok(Some(data)) => {
                eprint!("{}", std::str::from_utf8(&data).unwrap());
                thread::sleep(Duration::from_millis(100));
            }
            Ok(None) => {
                if signal.is_send_failed() {
                    tx_ops.send(Op::UnpauseData(th)).unwrap();
                }
                atomic_wait::wait(signal.signal(), SIG_EVENT);
            }
            Err(ReceiveError::SendClosed) => break,
            Err(ReceiveError::Closed) => unreachable!(),
        }
    }
    handle.join().unwrap();
}
