use std::thread;
use fast_down::curl::worker::{multi, Op};

pub fn main() {
    let (tx_ops, rx_ops) = kanal::unbounded();
    let handle = thread::spawn(move || {
        multi(rx_ops).unwrap();
    });
    let (tx_ret, ret) = oneshot::channel();
    let (tx_data, rx_data) = kanal::bounded(1);
    tx_ops.send(Op::New(tx_data, curl::easy::List::new(), "https://1.1.1.1/cdn-cgi/trace".to_string(), tx_ret)).unwrap();
    for data in rx_data.into_iter() {
        dbg!(String::from_utf8_lossy(&*data));
    }
    handle.join().unwrap();
}