//! The partial file paired with a `.fd` state file.

use crate::utils::path_key;
use std::path::{Path, PathBuf};

/// The `.part` file belonging to `fd_path`: the same path with its extension
/// swapped to `part`, so the pair always shares a stem and moves together.
pub fn part_path(fd_path: &Path) -> PathBuf {
    fd_path.with_extension("part")
}

/// Whether `fd_path`, `tmp_path` and `final_path` name three different files.
///
/// All three are compared after the same normalization the `.fd` claim uses
/// ([`path_key`]: `soft_canonicalize`, plus case folding on Windows and macOS),
/// so `./dl/a.fd`, `dl/a.fd` and `DL/A.FD` all name one pair on the platforms
/// they are one file on. All three pairs matter: an `.fd` equal to its `.part`
/// lets the sink overwrite the state, an output equal to the `.fd` makes
/// `finalize` replace the state with the finished file, and an output equal to
/// the `.part` turns its rename into a no-op that leaves no output behind.
pub fn paths_are_distinct(fd_path: &Path, tmp_path: &Path, final_path: &Path) -> bool {
    let fd = path_key(fd_path);
    let tmp = path_key(tmp_path);
    let output = path_key(final_path);
    fd != tmp && fd != output && tmp != output
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fd_part_path_{name}_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn dot_segments_name_the_same_file() {
        // `soft_canonicalize` resolves the `.` even when the rest does not exist
        // yet, so a leading `./` cannot make one `.fd` look distinct from itself.
        let dir = temp_dir("dot");
        let direct = dir.join("dl").join("a.fd");
        let dotty = dir.join(".").join("dl").join("a.fd");
        assert!(
            !paths_are_distinct(&direct, &part_path(&direct), &dotty),
            "./dl/a.fd and dl/a.fd name the same output"
        );
    }

    /// A verbatim (`\\?\`) spelling and a plain one name the same file, which is
    /// what the claim has always promised; plan validation compares through the
    /// same key, so it sees that too.
    #[cfg(windows)]
    #[test]
    fn a_verbatim_path_names_the_same_file() {
        let dir = temp_dir("verbatim");
        let plain = dir.join("a.fd");
        let verbatim = PathBuf::from(format!(r"\\?\{}", plain.display()));
        assert!(
            !paths_are_distinct(&plain, &part_path(&plain), &verbatim),
            "a verbatim spelling of the .fd names the same output"
        );
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn case_variants_name_the_same_file() {
        // Windows and macOS compare paths case-insensitively by default, so the
        // plan must see `A.FD` and `a.fd` as one file there.
        let dir = temp_dir("case");
        let lower = dir.join("dl").join("a.fd");
        let upper = PathBuf::from(lower.to_string_lossy().to_uppercase());
        assert!(
            !paths_are_distinct(&lower, &part_path(&lower), &upper),
            "case variants name one output on this platform"
        );
    }

    #[test]
    fn the_part_and_the_output_must_differ() {
        let dir = temp_dir("part_output");
        let fd = dir.join("a.fd");
        let tmp = part_path(&fd);
        assert!(
            !paths_are_distinct(&fd, &tmp, &tmp),
            "an output equal to the .part would be renamed onto itself"
        );
        assert!(
            paths_are_distinct(&fd, &tmp, &dir.join("a.bin")),
            "a normal .fd/.part/output triple is distinct"
        );
    }
}
