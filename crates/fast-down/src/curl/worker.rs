use bytes::Bytes;
use curl::easy::{Easy, Handler, SeekResult, WriteError};
use curl::easy::{Easy2, HttpVersion, List};
use curl::multi::{Easy2Handle, Multi};
use kanal::{ReceiveError, Receiver, SendError, Sender};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt::{Debug, Formatter};
use std::io::SeekFrom;
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

#[derive(Debug)]
pub enum ChannelData {
    Data(Bytes),
    Error(i32)
}

#[derive(Debug)]
pub struct ChannelDataCollector(Arc<DataSignal>, Sender<ChannelData>);

impl Handler for ChannelDataCollector {
    fn header(&mut self, header: &[u8]) -> bool {
        true
    }

    fn write(&mut self, data: &[u8]) -> Result<usize, WriteError> {
        let bytes = Bytes::copy_from_slice(data);
        match self.1.try_send(ChannelData::Data(bytes)) {
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

impl Debug for DataSignal {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "signal%<>")
    }
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

    impl Debug for New {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("options::New")
                .field("headers", &self.headers)
                .field("url", &self.url)
                .field("signal", &self.signal)
                .finish()
        }
    }
}

#[derive(Debug)]
pub enum Op {
    New(Sender<ChannelData>, options::New, oneshot::Sender<TaskHandle>),
    Kill(TaskHandle),
    UnpauseData(TaskHandle),
}

fn handle_op(
    op: Op,
    multi: &mut Multi,
    handles: &mut slab::Slab<Easy2Handle<ChannelDataCollector>>,
) -> Result<(), anyhow::Error> {
    match op {
        Op::New(
            tx,
            options::New {
                headers,
                url,
                signal,
                extra,
            },
            ret,
        ) => {
            let mut easy2 = Easy2::new(ChannelDataCollector(signal, tx));
            easy2.url(&url)?;
            easy2.http_headers(headers)?;
            let easy2 = if let Some(f) = extra { f(easy2) } else { easy2 };
            ret.send(TaskHandle(handles.insert(multi.add2(easy2)?)))
                .expect("cannot send back handle");
        }
        Op::Kill(TaskHandle(key)) => {
            if let Some(handle) = handles
                .try_remove(key) {
                multi.remove2(handle)?;
            }
        }
        Op::UnpauseData(TaskHandle(key)) => {
            if let Some(handle) = handles
                .get(key) {
                handle.get_ref().0.reset_from(None, None);
                handle.unpause_write()?;
            }
        }
    }

    Ok(())
}

pub fn multi(rx_ops: Receiver<Op>) -> Result<(), anyhow::Error> {
    let mut multi = Multi::new();
    let mut handles = slab::Slab::with_capacity(32);
    let mut performed = 0;
    let mut pending_removals = Vec::new();
    loop {
        let maybe_task = match rx_ops.try_recv() {
            Ok(maybe_task) => maybe_task,
            Err(_) => break Ok(()),
        };
        if let Some(task) = maybe_task {
            handle_op(task, &mut multi, &mut handles)?;
        } else if performed == 0 {
            let task = match rx_ops.recv() {
                Ok(maybe_task) => maybe_task,
                Err(_) => break Ok(()),
            };
            handle_op(task, &mut multi, &mut handles)?;
        }
        match multi.perform() {
            Ok(num) => performed = num,
            Err(e) => return Err(anyhow::anyhow!(e)),
        };
        for (code, handle) in &mut handles {
            if let Ok(errno) = handle.os_errno() && errno != 0 {
                handle.get_ref().1.send(ChannelData::Error(errno)).expect("failed to send back error");
                pending_removals.push(code);
            }
        }
        for removal in pending_removals.drain(..) {
            handles.remove(removal);
        }
    }
}
