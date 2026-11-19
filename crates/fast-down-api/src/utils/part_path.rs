//! The partial file paired with a `.fd` state file.

use path_helper::comparable;
use std::path::{Path, PathBuf};

/// The `.part` file belonging to `fd_path`: the same path with its extension
/// swapped to `part`, so the pair always shares a stem and moves together.
pub fn part_path(fd_path: &Path) -> PathBuf {
    fd_path.with_extension("part")
}

/// Whether `fd_path`, `tmp_path` and `final_path` name three different files.
///
/// The paths are compared through [`comparable`], so a verbatim (`\\?\`) and a
/// plain spelling of one location still count as the same file, and the
/// comparison itself stays free of I/O.
pub fn paths_are_distinct(fd_path: &Path, tmp_path: &Path, final_path: &Path) -> bool {
    comparable(fd_path) != comparable(tmp_path) && comparable(fd_path) != comparable(final_path)
}
