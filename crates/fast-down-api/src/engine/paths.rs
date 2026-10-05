//! Filesystem preparation for a run: creating the output directory and claiming
//! a free `.fd`/`.part` scratch pair.
//!
//! These are the only places [`crate::engine`] touches the filesystem before the
//! engine starts, kept apart from [`super::runner`]'s run loop.

use super::open_create_new;
use crate::{Event, TerminationReason, Tx};
use path_helper::IterStemExt;
use std::{
    io,
    path::{Path, PathBuf},
};
use tokio::fs;

/// Create `final_path`'s parent directory, reporting a failure as
/// [`Event::BuildPusherError`].
pub async fn ensure_parent(final_path: &Path, tx: &Tx) -> bool {
    if let Some(parent) = final_path.parent()
        && let Err(e) = fs::create_dir_all(parent).await
    {
        let _ = tx.send(Event::BuildPusherError(e));
        return false;
    }
    true
}

/// Atomically claim a free `.fd`/`.part` pair next to `final_path`.
///
/// Walks `name`, `name (1)`, `name (2)` … and takes the first stem where both
/// files can be created exclusively. `overwrite` plays no part here: the scratch
/// pair is always unique, and uniquification of the *final* name happens at
/// rename time.
pub async fn claim_pair(
    final_path: &Path,
    tx: &Tx,
) -> Result<(PathBuf, PathBuf), TerminationReason> {
    for base in final_path.iter_stem() {
        let fd_path = base.with_added_extension("fd");
        let tmp_path = fd_path.with_extension("part");
        match open_create_new().open(&tmp_path).await {
            Ok(_) => match open_create_new().open(&fd_path).await {
                Ok(_) => return Ok((fd_path, tmp_path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let _ = fs::remove_file(&tmp_path).await;
                }
                Err(e) => {
                    let _ = fs::remove_file(&tmp_path).await;
                    let _ = tx.send(Event::BuildPusherError(e));
                    return Err(TerminationReason::Failed);
                }
            },
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                let _ = tx.send(Event::BuildPusherError(e));
                return Err(TerminationReason::Failed);
            }
        }
    }
    unreachable!("iter_stem is infinite")
}
