use bytes::Bytes;
use curl::easy::{Easy, Handler, WriteError};
use curl::easy::{Easy2, HttpVersion, List};
use curl::multi::Multi;
use kanal::{ReceiveError, Receiver, SendError, Sender};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default, Clone)]
struct BufferedDataCollector {
    waker: Option<core::task::Waker>,
    ring: VecDeque<u8>,
}

impl Handler for BufferedDataCollector {
    fn write(&mut self, data: &[u8]) -> Result<usize, WriteError> {
        let size = self.ring.capacity() - self.ring.len();
        if size == 0 {
            return Err(WriteError::Pause);
        }
        self.ring
            .extend(data[0..core::cmp::min(data.len(), size)].iter());
        if let Some(waker) = &self.waker {
            waker.wake_by_ref();
        }
        Ok(data.len())
    }
}
pub struct ChannelDataCollector(Arc<DataSignal>, Sender<Bytes>);

impl Handler for ChannelDataCollector {
    fn header(&mut self, header: &[u8]) -> bool {
        dbg!(std::str::from_utf8(header));
        true
    }

    fn write(&mut self, data: &[u8]) -> Result<usize, WriteError> {
        let bytes = Bytes::copy_from_slice(data);
        match self.1.try_send(bytes) {
            Ok(true) => {
                self.0.notify(false);
                Ok(data.len())
            }
            Ok(false) => {
                self.0.notify(true);
                Err(WriteError::Pause)
            }
            e @ Err(SendError::ReceiveClosed | SendError::Closed) => {
                e.unwrap();
                unreachable!()
            }
        }
    }
}

enum SignalWaker {
    Waker(Arc<Mutex<Option<core::task::Waker>>>),
    Callback(Arc<Mutex<Box<dyn FnMut(u32) + Send + Sync>>>),
    None,
}

pub struct DataSignal {
    waker: SignalWaker,
    state: AtomicU32,
    signal: AtomicU32,
}

pub const STATE_SEND_FAILED: u32 = 2;
pub const STATE_SENT: u32 = 1;
pub const STATE_NONE: u32 = 0;

pub const SIG_NONE: u32 = 2;
pub const SIG_EVENT: u32 = 1;

impl Default for DataSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl DataSignal {
    pub fn new_with_waker(maybe_waker_mutex: Arc<Mutex<Option<core::task::Waker>>>) -> Self {
        Self {
            waker: SignalWaker::Waker(maybe_waker_mutex),
            state: AtomicU32::new(STATE_NONE),
            signal: AtomicU32::new(SIG_NONE),
        }
    }
    pub fn new_with_callback(
        maybe_fn_mutex: Arc<Mutex<Box<dyn FnMut(u32) + Send + Sync>>>,
    ) -> Self {
        Self {
            waker: SignalWaker::Callback(maybe_fn_mutex),
            state: AtomicU32::new(STATE_NONE),
            signal: AtomicU32::new(SIG_NONE),
        }
    }
    pub fn new() -> Self {
        Self {
            waker: SignalWaker::None,
            state: AtomicU32::new(STATE_NONE),
            signal: AtomicU32::new(SIG_NONE),
        }
    }

    pub fn state(&self) -> u32 {
        self.state.load(Ordering::Acquire)
    }

    pub fn signal(&self) -> &AtomicU32 {
        &self.signal
    }

    fn reset_from(&self, old_state: Option<u32>, old_signal: Option<u32>) -> bool {
        if let Some(state) = old_state {
            match self.state.compare_exchange(
                state,
                STATE_NONE,
                Ordering::Acquire,
                Ordering::Acquire,
            ) {
                Ok(_) => {}
                Err(_) => return false,
            }
        } else {
            self.state.store(STATE_NONE, Ordering::Release);
        }
        if let Some(signal) = old_signal {
            match self.signal.compare_exchange(
                signal,
                SIG_NONE,
                Ordering::Acquire,
                Ordering::Acquire,
            ) {
                Ok(_) => {}
                Err(_) => return false,
            }
        } else {
            self.signal.store(SIG_NONE, Ordering::Release);
        }
        true
    }

    pub fn is_send_failed(&self) -> bool {
        match self.state.load(Ordering::Acquire) {
            STATE_SEND_FAILED => true,
            STATE_NONE | STATE_SENT => false,
            _ => unreachable!("invalid state"),
        }
    }

    pub fn is_sent(&self) -> bool {
        match self.state.load(Ordering::Acquire) {
            STATE_SENT => true,
            STATE_NONE | STATE_SEND_FAILED => false,
            _ => unreachable!("invalid state"),
        }
    }

    pub fn is_none(&self) -> bool {
        match self.state.load(Ordering::Acquire) {
            STATE_NONE => true,
            STATE_SENT | STATE_SEND_FAILED => false,
            _ => unreachable!("invalid state"),
        }
    }

    fn notify(&self, send_fail: bool) {
        let val = if send_fail { 2 } else { 1 };
        self.state.store(val, Ordering::Release);
        self.signal.store(1, Ordering::Release);
        atomic_wait::wake_all(&self.signal);
        self.signal.store(STATE_NONE, Ordering::Release);
        match &self.waker {
            SignalWaker::Waker(waker) => {
                if let Some(a) = waker.lock().unwrap().take() {
                    core::task::Waker::wake(a)
                }
            }
            SignalWaker::Callback(f) => f.lock().unwrap()(val),
            SignalWaker::None => {}
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct TaskHandle(usize);
pub mod options {
    use super::*;
    pub struct New {
        pub headers: List,
        pub url: String,
        pub signal: Arc<DataSignal>,
        pub extra: Option<
            Box<dyn FnOnce(Easy2<ChannelDataCollector>) -> Easy2<ChannelDataCollector> + Send>,
        >,
    }
}

pub enum Op {
    New(Sender<Bytes>, options::New, oneshot::Sender<TaskHandle>),
    Kill(TaskHandle),
    UnpauseData(TaskHandle),
}

pub fn multi(rx_ops: Receiver<Op>) -> Result<(), anyhow::Error> {
    let multi = Multi::new();
    let mut handles = slab::Slab::with_capacity(32);
    loop {
        let maybe_task = match rx_ops.try_recv() {
            Ok(maybe_task) => maybe_task,
            Err(ReceiveError::Closed) => break Ok(()),
            Err(ReceiveError::SendClosed) => unreachable!(),
        };
        match maybe_task {
            None => {}
            Some(Op::New(
                tx,
                options::New {
                    headers,
                    url,
                    signal,
                    extra,
                },
                ret,
            )) => {
                let mut easy2 = Easy2::new(ChannelDataCollector(signal, tx));
                easy2.url(&url)?;
                easy2.http_headers(headers)?;
                let easy2 = if let Some(f) = extra { f(easy2) } else { easy2 };
                ret.send(TaskHandle(handles.insert(multi.add2(easy2)?)))
                    .expect("cannot send back handle");
            }
            Some(Op::Kill(TaskHandle(key))) => {
                let handle = handles
                    .try_remove(key)
                    .ok_or_else(|| format!("no such handle: {key}"))
                    .unwrap();
                multi.remove2(handle)?;
            }
            Some(Op::UnpauseData(TaskHandle(key))) => {
                let handle = handles
                    .get(key)
                    .ok_or_else(|| format!("no such handle: {key}"))
                    .unwrap();
                handle.get_ref().0.reset_from(None, None);
                handle.unpause_write()?;
            }
        }
        match multi.perform() {
            Ok(0) => break Ok(()),
            Ok(_) => continue,
            Err(e) => return Err(anyhow::anyhow!(e)),
        }
    }
}
