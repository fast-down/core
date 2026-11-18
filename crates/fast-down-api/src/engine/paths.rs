//! Filesystem preparation for a run: creating the output directory and claiming
//! a free `.fd`/`.part` scratch pair.
//!
//! These are the only places [`crate::engine`] touches the filesystem before the
//! engine starts, kept apart from [`super::runner`]'s run loop.

use super::{
    claim::{self, Claim},
    open_create_new,
};
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
/// Walks `name`, `name (1)`, `name (2)` … and takes the first stem whose claim it
/// can hold and whose two files it can then create exclusively. `overwrite` plays
/// no part here: the scratch pair is always unique, and uniquification of the
/// *final* name happens at rename time.
///
/// The claim comes first because a claim is keyed by the `.fd` path and enforced
/// by an OS lock, so it works while the pair is still absent. Losing it means
/// another run owns that stem — with or without files on disk yet — and this run
/// must leave the stem untouched and move on: the winner may be about to write
/// its state there, and a freshly created pair is not this run's to delete.
pub async fn claim_pair(
    final_path: &Path,
    tx: &Tx,
) -> Result<(Claim, PathBuf, PathBuf), TerminationReason> {
    for base in final_path.iter_stem() {
        let fd_path = base.with_added_extension("fd");
        let tmp_path = crate::utils::part_path(&fd_path);
        let claim = match claim::claim(&fd_path) {
            Ok(Some(claim)) => claim,
            Ok(None) => continue,
            Err(e) => {
                let _ = tx.send(Event::BuildPusherError(e));
                return Err(TerminationReason::Failed);
            }
        };
        match open_create_new().open(&tmp_path).await {
            Ok(_) => match open_create_new().open(&fd_path).await {
                Ok(_) => return Ok((claim, fd_path, tmp_path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    // Only the `.part` is this run's to remove; a `.fd` already
                    // on disk belongs to whoever created it. The claim is held
                    // until this arm ends on purpose: releasing it first would
                    // let another run take the stem before the cleanup is done.
                    let _ = fs::remove_file(&tmp_path).await;
                }
                Err(e) => {
                    let _ = fs::remove_file(&tmp_path).await;
                    let _ = tx.send(Event::BuildPusherError(e));
                    return Err(TerminationReason::Failed);
                }
            },
            // This stem already has a `.part`; let the claim go and try the
            // next one.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                let _ = tx.send(Event::BuildPusherError(e));
                return Err(TerminationReason::Failed);
            }
        }
    }
    unreachable!("iter_stem is infinite")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Losing the claim for a stem must create nothing there: the winner may be
    /// about to persist its state under that very name, and deleting a pair this
    /// run never owned would take that state with it.
    #[tokio::test]
    async fn a_lost_claim_leaves_no_pair_and_moves_to_the_next_stem() {
        let dir = std::env::temp_dir().join(format!("fd_claim_pair_lost_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir).await;
        fs::create_dir_all(&dir).await.unwrap();
        let final_path = dir.join("out.bin");

        // Another run already owns the first stem, without any file on disk.
        let held_path = final_path.with_added_extension("fd");
        let held = claim::claim(&held_path)
            .unwrap()
            .expect("hold the first stem");

        let (tx, _rx) = crate::create_channel();
        let (_claim, fd_path, tmp_path) = claim_pair(&final_path, &tx).await.unwrap();
        assert_eq!(
            fd_path,
            dir.join("out (1).bin").with_added_extension("fd"),
            "the losing run must take the next stem"
        );
        assert_eq!(tmp_path, crate::utils::part_path(&fd_path));
        assert!(fd_path.exists(), "the pair it won must be created");
        assert!(tmp_path.exists(), "the pair it won must be created");
        assert!(
            !held_path.exists(),
            "the contested stem must not gain a `.fd`"
        );
        assert!(
            !crate::utils::part_path(&held_path).exists(),
            "the contested stem must not gain a `.part`"
        );

        drop(held);
        let _ = fs::remove_dir_all(&dir).await;
    }
}
