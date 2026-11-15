//! In-process claim guarding a `.fd`/`.part` pair against concurrent runs.
//!
//! Two runs of the same state file in one process would interleave writes to
//! the `.part` and overwrite each other's `.fd`, losing progress. The claim is
//! process-local: another process can still open the same pair.

use parking_lot::Mutex;
use soft_canonicalize::soft_canonicalize;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::LazyLock,
};

static ACTIVE: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// One run's exclusive hold on a `.fd` path, released on drop.
#[derive(Debug)]
pub struct Claim(PathBuf);

/// Take the claim for `fd_path`, or `None` when another run holds it.
pub fn claim(fd_path: &Path) -> Option<Claim> {
    // Normalize so different spellings of the same path (`./out.fd`,
    // `dir/../out.fd`, a junction to the same directory) share one key.
    let mut key = soft_canonicalize(fd_path).unwrap_or_else(|_| fd_path.to_path_buf());
    #[cfg(windows)]
    {
        // Windows compares paths case-insensitively, so `OUT.FD` and `out.fd`
        // name the same pair and must share one claim.
        key = PathBuf::from(key.to_string_lossy().to_lowercase());
    }
    let mut active = ACTIVE.lock();
    if active.insert(key.clone()) {
        Some(Claim(key))
    } else {
        None
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        ACTIVE.lock().remove(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_fd(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("fd-claim-{tag}-{}.fd", std::process::id()))
    }

    #[test]
    fn second_claim_is_refused_until_the_first_is_dropped() {
        let path = temp_fd("exclusive");
        let first = claim(&path).expect("first claim");
        assert!(claim(&path).is_none(), "a second run must be refused");
        drop(first);
        assert!(claim(&path).is_some(), "dropping the run releases the pair");
    }

    #[test]
    fn equivalent_spellings_share_one_claim() {
        let path = temp_fd("spelling");
        let dir = path.parent().expect("temp dir");
        let roundabout = dir
            .join("..")
            .join(dir.file_name().expect("temp dir name"))
            .join(path.file_name().expect("file name"));
        let first = claim(&path).expect("first claim");
        assert!(
            claim(&roundabout).is_none(),
            "a different spelling of the same path must hit the same claim"
        );
        drop(first);
    }

    #[cfg(windows)]
    #[test]
    fn case_variants_share_one_claim_on_windows() {
        let path = temp_fd("case");
        let upper = PathBuf::from(path.to_string_lossy().to_uppercase());
        let first = claim(&path).expect("first claim");
        assert!(
            claim(&upper).is_none(),
            "Windows paths are case-insensitive, so the pair is already claimed"
        );
        drop(first);
    }
}
