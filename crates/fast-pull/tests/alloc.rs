//! Regression tests pinning the handle's heap cost.
//!
//! The `ForceSend` wrapper around the session state is transparent, so it must
//! stay allocation- and dispatch-neutral:
//!
//! * constructing a handle allocates **exactly once** — the same single
//!   `Arc<DownloadResultInner>` allocation as before, with no extra `Box`;
//! * cloning a handle allocates **nothing** — it is an `Arc` refcount bump.
//!
//! The allocation counter is process-global, so this file must contain exactly
//! one test (it does) and nothing else may allocate on other threads while it
//! runs. `DownloadResult::new` spawns no tasks, so its body is fully
//! synchronous and the measurement is deterministic.

use crossfire::mpmc;
use fast_pull::mock::MockPuller;
use fast_pull::multi::TokioExecutor;
use fast_pull::{DownloadResult, Event};
use std::alloc::{GlobalAlloc, Layout, System};
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingAlloc;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAlloc = CountingAlloc;

type Handle = DownloadResult<TokioExecutor<MockPuller, Infallible>, Infallible, Infallible>;

#[test]
fn handle_construction_allocates_once_and_clone_allocates_none() {
    // Build everything that could allocate *before* the measured region.
    let (tx, rx) = mpmc::unbounded_async::<Event<Infallible, Infallible>>();
    let token = tokio_util::sync::CancellationToken::new();

    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let handle: Handle = DownloadResult::new(rx, None, token);
    let after = ALLOCATIONS.load(Ordering::Relaxed);
    assert_eq!(
        after - before,
        1,
        "constructing a handle must cost exactly one allocation"
    );

    let before_clone = ALLOCATIONS.load(Ordering::Relaxed);
    let clone = handle.clone();
    let after_clone = ALLOCATIONS.load(Ordering::Relaxed);
    assert_eq!(
        after_clone - before_clone,
        0,
        "cloning a handle must not allocate"
    );

    drop(clone);
    drop(handle);
    drop(tx);
}
