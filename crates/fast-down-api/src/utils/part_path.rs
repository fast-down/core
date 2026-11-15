//! The partial file paired with a `.fd` state file.

use std::path::{Path, PathBuf};

/// The `.part` file belonging to `fd_path`: the same path with its extension
/// swapped to `part`, so the pair always shares a stem and moves together.
pub fn part_path(fd_path: &Path) -> PathBuf {
    fd_path.with_extension("part")
}
