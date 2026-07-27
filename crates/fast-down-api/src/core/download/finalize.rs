use crate::{Event, Tx};
use path_helper::tokio::gen_unique_path;
use std::path::Path;
use tokio::fs;

/// Rename the finished `.part` into place and drop the `.fd` state file.
///
/// In unique mode, a fresh unique destination is reserved right before rename
/// via `gen_unique_path` (atomic `create_new`), closing the TOCTOU gap where
/// the final file could have been created by someone else during the download.
/// On any failure the relevant error event is sent through `tx`.
pub(super) async fn finalize(tx: &Tx, unique: bool, tmp: &Path, cfg: &Path, final_path: &Path) {
    let dest = if unique {
        match gen_unique_path(final_path).await {
            Ok(p) => p,
            Err(e) => {
                let _ = tx.send(Event::GenPathError(e));
                return;
            }
        }
    } else {
        final_path.to_path_buf()
    };
    if let Err(e) = fs::rename(tmp, &dest).await {
        // Best-effort: drop the empty placeholder we just reserved so a failed
        // rename doesn't leave an orphan `xxx (1).mp4` behind.
        if unique {
            let _ = fs::remove_file(&dest).await;
        }
        let _ = tx.send(Event::RenameFailed(e));
        return;
    }
    let _ = tx.send(Event::Renamed(dest));
    // Success: the download is complete and renamed, so the state file is no
    // longer needed. Best-effort cleanup only (kept on cancel).
    let _ = fs::remove_file(cfg).await;
}
