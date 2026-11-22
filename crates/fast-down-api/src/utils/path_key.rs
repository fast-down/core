//! One normalization behind every "is this the same file?" question.
//!
//! Two places need to decide it: the `.fd` claim (does another run already own
//! this pair?) and plan validation (do the `.fd`, its `.part` and the output
//! name three different files?). They must agree, so they share one key.

use path_helper::comparable;
use std::path::{Path, PathBuf};

/// Normalize `path` into the key that every spelling of one location shares.
///
/// `soft_canonicalize` folds `.`/`..` and resolves symlinks in the part of the
/// path that exists, without requiring the whole path to exist. Windows and
/// macOS compare paths case-insensitively by default, so the key is lowercased
/// there too: `./out.fd`, `dir/../out.fd` and `OUT.FD` name one pair and share
/// one lock.
///
/// Its answer still has two Windows spellings: verbatim (`\\?\C:\…`), or
/// simplified without the prefix when the `dunce` feature is on — and that
/// feature is not ours to hold still, since a crate that depends on us can switch
/// it on for the whole build. `comparable` folds both spellings onto one, so the
/// key, and the lock name hashed from it, does not depend on how the dependency
/// happened to be built.
pub fn path_key(path: &Path) -> PathBuf {
    let resolved =
        soft_canonicalize::soft_canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let key = comparable(&resolved);
    // Windows and macOS compare paths case-insensitively by default, so `OUT.FD`
    // and `out.fd` name the same pair and must share one lock.
    #[cfg(any(windows, target_os = "macos"))]
    let key = PathBuf::from(key.to_string_lossy().to_lowercase());
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key must not carry the extended-length prefix: with the `dunce`
    /// feature on, `soft_canonicalize` answers `C:\…` instead, and a caller can
    /// turn that feature on for us.
    #[cfg(windows)]
    #[test]
    fn verbatim_and_plain_spellings_share_one_key() {
        assert_eq!(
            path_key(Path::new(r"\\?\C:\dl\a.fd")),
            path_key(Path::new(r"C:\dl\a.fd"))
        );
    }

    /// Dot segments are folded before the key is formed, so a roundabout spelling
    /// cannot open a second pair.
    #[test]
    fn dot_segments_share_one_key() {
        let dir = std::env::temp_dir().join("fd_path_key_dot");
        assert_eq!(
            path_key(&dir.join(".").join("a.fd")),
            path_key(&dir.join("a.fd"))
        );
    }
}
