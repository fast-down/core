extern crate alloc;
extern crate std;

use alloc::sync::Arc;
use core::cell::UnsafeCell;
use std::io::Write;
use core::sync::atomic::AtomicBool;
use std::sync::Mutex;
use bytes::Bytes;
use color_eyre::eyre::Result;
use crossbeam_skiplist::SkipMap;
use memmap2::MmapMut;
use crate::Progress;
use super::write::DownloadWriter;
struct Inner {
    mmap: UnsafeCell<MmapMut>,
    map_size: u64,
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
        let map_size = file.metadata()?.len();
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
        let map_size = file.metadata()?.len();
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

impl DownloadWriter for FileWriter {
    //noinspection RsUnwrap
    fn write_part(&self, progress: Progress, bytes: Bytes) -> Result<()> {
        todo!();
        // progress.start * self.0.block_locks.len();
        // let _guard = self.0.lock().unwrap();
        // unsafe { self.write_part_unchecked(progress, bytes) }
    }

    unsafe fn write_part_unchecked(&self, progress: Progress, bytes: Bytes) -> Result<()> {
        todo!();
        // unsafe { &mut (*self.mmap.0.get())[progress] }.write_all(&*bytes)?;
        // Ok(())
    }
}

#[cfg(test)]
mod test {
    #[test]
    fn test_overlay_iter() {
        use std::iter::Iterator;

        struct OverlapIterator {
            range: std::ops::Range<u64>,
            n: u64,
            chunk_size: u64,
        }

        impl Iterator for OverlapIterator {
            type Item = u64;

            fn next(&mut self) -> Option<Self::Item> {
                // Calculate chunk size, checking for potential division by zero error which is handled when instantiating this struct
                // let chunk_size = self.count / self.n;  This is not needed because we already calculated it

                // Iterate through chunks and check for overlap
                let mut i = 0;
                while i * self.chunk_size < self.range.end {
                    // Check if the current chunk overlaps with the range
                    if self.range.start <= (i + 1) * self.chunk_size && self.range.end > i * self.chunk_size{
                        return Some(i);
                    }
                    i += 1;
                }
                None // No more overlaps
            }
        }


        fn overlapping_chunks(range: std::ops::Range<u64>, count: u64, n: u64) -> impl Iterator<Item = u64> {
            let chunk_size = count / n; //This might be zero
            OverlapIterator { range, n, chunk_size }
        }

        let range = 10..20; // Example range
        let count: u64 = 30; // Example count
        let n: u64 = 3;  // Example n

        for i in overlapping_chunks(range.clone(), count, n) {
            println!("Overlapping chunk index: {}", i);
        }


        let range = 0..10; // Example range
        let count: u64 = 10; // Example count
        let n: u64 = 2;  // Example n

        for i in overlapping_chunks(range.clone(), count, n) {
            println!("Overlapping chunk index: {}", i);
        }

        //test for empty range
        let range = 10..10;
        let count: u64 = 10;
        let n: u64 = 2;
        for i in overlapping_chunks(range,count,n){
            println!("Overlapping chunk index: {}", i);
        }

        //test for n=0
        let range = 10..20;
        let count: u64 = 10;
        let n: u64 = 0;
        for i in overlapping_chunks(range,count,n){
            println!("Overlapping chunk index: {}", i);
        }

        //test for chunk_size = 0
        let range = 10..20;
        let count: u64 = 0;
        let n: u64 = 2;
        for i in overlapping_chunks(range,count,n){
            println!("Overlapping chunk index: {}", i);
        }
    }
}
