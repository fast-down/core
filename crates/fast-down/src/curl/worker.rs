use bytes::Bytes;
use curl::easy::{Easy2, HttpVersion, List};
use curl::easy::{Easy, Handler, WriteError};
use curl::multi::Multi;
use std::collections::VecDeque;
use kanal::{ReceiveError, Receiver, SendError, Sender};

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
struct ChannelDataCollector(Sender<Bytes>);

impl Handler for ChannelDataCollector {
    fn header(&mut self, header: &[u8]) -> bool {
        dbg!(std::str::from_utf8(header));
        true
    }

    fn write(&mut self, data: &[u8]) -> Result<usize, WriteError> {
        let bytes = Bytes::copy_from_slice(data);
        match self.0.try_send(bytes) {
            Ok(true) => Ok(data.len()),
            Ok(false) =>  Err(WriteError::Pause),
            e@ Err(SendError::ReceiveClosed | SendError::Closed) => {
                e.unwrap();
                unreachable!()
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct TaskHandle(usize);

pub enum Op {
    New(Sender<Bytes>, List, String, oneshot::Sender<TaskHandle>),
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
            Err(ReceiveError::SendClosed) => unreachable!()
        };
        match maybe_task {
            None => {}
            Some(Op::New(tx, headers, url, ret)) => {
                let mut easy2 = Easy2::new(ChannelDataCollector(tx));
                easy2.url(&url)?;
                easy2.ssl_verify_host(false)?; // todo(CyanChanges): temporary workaround, might be changed later
                easy2.ssl_verify_peer(false)?; // todo(CyanChanges): temporary workaround, might be changed later
                easy2.http_headers(headers)?;
                easy2.http_version(HttpVersion::V3)?;
                ret.send(TaskHandle(handles.insert(multi.add2(easy2)?))).expect("cannot send back handle");
            }
            Some(Op::Kill(TaskHandle(key))) => {
                let handle = handles.try_remove(key)
                    .ok_or_else(|| format!("no such handle: {key}"))
                    .unwrap();
                multi.remove2(handle)?;
            },
            Some(Op::UnpauseData(TaskHandle(key))) => {
                let handle = handles.get(key)
                    .ok_or_else(|| format!("no such handle: {key}"))
                    .unwrap();
                handle.unpause_write()?;
            }
        }
        match multi.perform() {
            Ok(0) => break Ok(()),
            Ok(_) => continue,
            Err(e) => return Err(anyhow::anyhow!(e))
        }
    }
}
