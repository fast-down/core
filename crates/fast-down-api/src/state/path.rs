use std::path::{Path, PathBuf};

/// The `.part` file paired with a `.fd` state file: the same path with its
/// extension swapped. `video.mp4.fd` → `video.mp4.part`.
///
/// This is the single place the `.fd`/`.part` naming rule lives, so a `.fd`
/// and its partial file always move together and no path has to be recorded in
/// the state file itself.
#[must_use]
pub fn tmp_path_for(fd_path: &Path) -> PathBuf {
    fd_path.with_extension("part")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swaps_extension() {
        assert_eq!(
            tmp_path_for(Path::new("/dl/video.mp4.fd")),
            Path::new("/dl/video.mp4.part")
        );
        assert_eq!(
            tmp_path_for(Path::new("/dl/video.fd")),
            Path::new("/dl/video.part")
        );
        assert_eq!(
            tmp_path_for(Path::new("/dl/README.fd")),
            Path::new("/dl/README.part")
        );
    }
}
