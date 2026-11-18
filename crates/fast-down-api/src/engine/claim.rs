//! Claim guarding a `.fd`/`.part` pair against concurrent runs.
//!
//! Two runs writing the same pair would interleave writes to the `.part` and
//! overwrite each other's `.fd`, losing progress. The claim is one advisory OS
//! lock per normalized `.fd` path (`flock` on Unix, `LockFileEx` on Windows,
//! through `fs4`).
//!
//! Those locks are held per open file handle, not per process, so a second run
//! in the *same* process is refused just like another process would be; there
//! is no separate in-process registry to keep in step with it.
//!
//! Keys are normalized first (`soft_canonicalize`, plus case folding on Windows
//! and macOS), so `./out.fd`, `dir/../out.fd` and `OUT.FD` all name one pair.
//!
//! The locked file lives under `<temp>/fast-down-locks` and is named after a
//! hash of the key, so a download directory never gains a stray `.lock`. It is
//! left in place because deleting it would race a process that already opened
//! it; the lock itself disappears with the last handle. Because that file is in
//! `temp_dir()`, the lock's semantics do not depend on the filesystem the
//! download writes to.

use fs4::TryLockError;
use soft_canonicalize::soft_canonicalize;
use std::{
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::LazyLock,
};

/// Directory holding every lock file, created on first use.
static LOCK_DIR: LazyLock<PathBuf> = LazyLock::new(|| std::env::temp_dir().join("fast-down-locks"));

/// One run's exclusive hold on a `.fd` path, released on drop.
#[derive(Debug)]
pub struct Claim {
    /// The locked handle; closing it releases the OS lock.
    file: File,
}

/// Normalize `fd_path` into the key the lock file is derived from.
fn key_of(fd_path: &Path) -> PathBuf {
    let key = soft_canonicalize(fd_path).unwrap_or_else(|_| fd_path.to_path_buf());
    // Windows and macOS compare paths case-insensitively by default, so `OUT.FD`
    // and `out.fd` name the same pair and must share one lock.
    #[cfg(any(windows, target_os = "macos"))]
    let key = PathBuf::from(key.to_string_lossy().to_lowercase());
    key
}

/// The lock file paired with `key`.
///
/// The name is an FNV-1a hash of the key's bytes rather than the path itself:
/// paths can exceed the name limit and can contain separators. A 64-bit hash
/// collides for two different paths only with probability ~n²/2⁶⁵ — and a
/// collision is harmless in the safe direction, since it can only refuse a run
/// that would otherwise have been allowed.
fn lock_path(key: &Path) -> PathBuf {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in key.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    LOCK_DIR.join(format!("{hash:016x}.lock"))
}

/// Take the claim for `fd_path`.
///
/// `Ok(None)` means another run already holds it, in this process or another
/// one. `Err` means the lock file itself could not be created, opened, or
/// locked; the caller reports that rather than silently running unprotected.
pub fn claim(fd_path: &Path) -> io::Result<Option<Claim>> {
    let key = key_of(fd_path);
    std::fs::create_dir_all(&*LOCK_DIR)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path(&key))?;
    match fs4::FileExt::try_lock(&file) {
        Ok(()) => Ok(Some(Claim { file })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        // Closing the handle releases the lock; unlocking first makes that
        // explicit instead of relying on the file's drop order.
        let _ = fs4::FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn fd_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fd_claim_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("x.fd")
    }

    #[test]
    fn claim_is_exclusive_within_the_process() {
        let path = fd_path("inproc");
        let first = claim(&path).unwrap().expect("first claim");
        assert!(claim(&path).unwrap().is_none(), "second claim must fail");
        drop(first);
        assert!(
            claim(&path).unwrap().is_some(),
            "released claim is reusable"
        );
    }

    #[test]
    fn different_paths_do_not_conflict() {
        let a = claim(&fd_path("a")).unwrap().unwrap();
        let b = claim(&fd_path("b")).unwrap().unwrap();
        drop((a, b));
    }

    /// Equivalent spellings of one path must hit the same lock, or a caller
    /// could open the same pair twice by naming it differently.
    #[test]
    fn equivalent_spellings_share_one_claim() {
        let path = fd_path("spelling");
        let dir = path.parent().expect("temp dir");
        let roundabout = dir
            .join("..")
            .join(dir.file_name().expect("temp dir name"))
            .join(path.file_name().expect("file name"));
        let first = claim(&path).unwrap().expect("first claim");
        assert!(
            claim(&roundabout).unwrap().is_none(),
            "a different spelling of the same path must hit the same claim"
        );
        drop(first);
    }

    #[cfg(windows)]
    #[test]
    fn case_variants_share_one_claim_on_windows() {
        let path = fd_path("case");
        let upper = PathBuf::from(path.to_string_lossy().to_uppercase());
        let first = claim(&path).unwrap().expect("first claim");
        assert!(
            claim(&upper).unwrap().is_none(),
            "Windows paths are case-insensitive, so the pair is already claimed"
        );
        drop(first);
    }

    /// The default macOS filesystem (APFS) is case-insensitive too, and the
    /// lock keys follow that so two spellings cannot open one pair twice.
    #[cfg(target_os = "macos")]
    #[test]
    fn case_variants_share_one_claim_on_macos() {
        let path = fd_path("case");
        let upper = PathBuf::from(path.to_string_lossy().to_uppercase());
        let first = claim(&path).unwrap().expect("first claim");
        assert!(
            claim(&upper).unwrap().is_none(),
            "macOS paths are case-insensitive by default, so the pair is already claimed"
        );
        drop(first);
    }

    /// The OS lock itself — not an in-process registry — is what refuses a
    /// second handle in this same process. This is the invariant that lets the
    /// claim skip a second layer of state.
    #[test]
    fn the_os_lock_refuses_a_second_handle_in_this_process() {
        let path = fd_path("handle");
        let claim = claim(&path).unwrap().unwrap();
        let other = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path(&key_of(&path)))
            .unwrap();
        assert!(
            matches!(
                fs4::FileExt::try_lock(&other),
                Err(TryLockError::WouldBlock)
            ),
            "a second handle in the same process must not acquire the OS lock"
        );
        drop(claim);
        fs4::FileExt::try_lock(&other).expect("the lock is free after drop");
    }

    /// A `.lock` left by a process that died carries no lock: the OS released
    /// it with the handle, so the next run claims successfully.
    #[test]
    fn leftover_lock_file_is_harmless() {
        let path = fd_path("stale");
        std::fs::create_dir_all(&*LOCK_DIR).unwrap();
        std::fs::write(lock_path(&key_of(&path)), b"").unwrap();
        assert!(claim(&path).unwrap().is_some());
    }
}
