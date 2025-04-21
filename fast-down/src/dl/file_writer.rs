extern crate alloc;
extern crate std;

use alloc::sync::Arc;
use core::cell::UnsafeCell;
use std::io::Write;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::Ordering;
use bytes::Bytes;
use color_eyre::eyre::Result;
use crossbeam_skiplist::SkipMap;
use memmap2::MmapMut;
use crate::dl::block_lock::overlaps;
use crate::Progress;
use super::write::DownloadWriter;

struct Inner {
    mmap: UnsafeCell<MmapMut>,
    map_size: usize,
    block_locks: SkipMap<usize, AtomicBool>,
}

unsafe impl Sync for Inner {}

#[cfg(feature = "file")]
#[derive(Clone)]
pub struct FileWriter(Arc<Inner>);

#[cfg(feature = "file")]
impl FileWriter {
    pub fn new<const LOCK_COUNT: usize>(file: impl Into<std::fs::File>) -> Result<FileWriter> {
        let file = file.into();
        let map_size = file.metadata()?.len() as usize;
        let locks = SkipMap::new();
        for i in 0..LOCK_COUNT {
            locks.insert(i, AtomicBool::new(false));
        }
        let mmap = unsafe { MmapMut::map_mut(&file)? };
        Ok(FileWriter(Arc::new(Inner {
            mmap: UnsafeCell::new(mmap),
            block_locks: locks,
            map_size
        })))
    }

    pub fn with_lock_size(file: impl Into<std::fs::File>, lock_size: usize) -> Result<FileWriter> {
        let file = file.into();
        let map_size = file.metadata()?.len() as usize;
        let locks = SkipMap::new();
        for i in 0..lock_size {
            locks.insert(i, AtomicBool::new(false));
        }
        let mmap = unsafe { MmapMut::map_mut(&file)? };
        Ok(FileWriter(Arc::new(Inner {
            mmap: UnsafeCell::new(mmap),
            block_locks: locks,
            map_size,
        })))
    }
}

impl Drop for FileWriter {
    fn drop(&mut self) {
        unsafe { (*self.0.mmap.get()).flush().unwrap() }
    }
}

const SPIN_TIME: usize = 100;

fn spin(lock: &AtomicBool, current: bool, new: bool) {
    let mut spin: usize = 0;
    loop {
        // Attempt to set the flag to true only if it's currently false.
        if lock.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            // The compare-and-swap operation succeeded, the flag is now true.
            break;
        } else {
            #[cfg(any(target_arch = "x86", target_arch = "x86_64", target_arch = "arm", target_arch = "aarch64", target_arch = "riscv32", target_arch = "riscv64"))]
            unsafe {
                core::arch::asm!("nop");
            }
            spin += 1;
            if spin > SPIN_TIME {
                std::thread::yield_now();
                spin = 0;
            }
        }
    }
}

impl DownloadWriter for FileWriter {
    //noinspection RsUnwrap
    fn write_part(&self, progress: Progress, bytes: Bytes) -> Result<()> {
        let blk_sz = self.0.map_size.div_ceil(self.0.block_locks.len());

        for lock_id in overlaps(progress.start..progress.end, blk_sz, self.0.map_size) { // spin set lock
            match self.0.block_locks.get(&(lock_id as usize)) {
                Some(entry) => {
                    let lock = entry.value();
                    spin(lock, false, true);
                },
                None => continue
            };
        }

        let result = unsafe {
            self.write_part_unchecked(progress.clone(), bytes)
        };

        for lock_id in overlaps(progress.start..progress.end, blk_sz, self.0.map_size) { // spin to unset the lock
            match self.0.block_locks.get(&(lock_id)) {
                Some(entry) => {
                    let lock = entry.value();

                    spin(lock, true, false);
                },
                None => continue
            };
        }

        result
    }

    unsafe fn write_part_unchecked(&self, progress: Progress, bytes: Bytes) -> Result<()> {
        unsafe { &mut (*self.0.mmap.get())[progress.start..progress.end] }.write_all(&*bytes)?;
        Ok(())
    }
}

