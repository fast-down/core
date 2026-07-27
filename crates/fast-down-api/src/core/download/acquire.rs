use crate::{Config, DownloadState, Event, PartialConfig, ResumeError, Tx};
use fast_down::UrlInfo;
use inherit_config::ConfigLayer;
use std::path::Path;
use tokio::fs::{self, File, OpenOptions};

/// Outcome of trying to acquire a writable `.part` file + the matching resume state.
#[allow(clippy::large_enum_variant)]
pub(super) enum Acquire {
    /// A file is ready to write, optionally carrying a previously-saved resume state.
    ///
    /// The large fields are boxed so the variant stays small (`large_enum_variant`).
    Ready {
        file: File,
        effective: Config,
        parsed: PartialConfig,
        resume_state: Option<DownloadState>,
    },
    /// Unique-name collision: the caller should regenerate the stem and retry.
    CollisionRetry,
    /// An unrecoverable error was already reported via `tx`; the caller should stop.
    Abort,
}

/// Outcome of probing an existing `.part`/`.fd` pair for resume eligibility.
///
/// The classification is pure (see [`classify_resume`]); this enum only names
/// the three possible results so the resume *contract* lives in one place.
#[allow(clippy::large_enum_variant)]
enum ResumeProbe {
    /// File + state both present and the state still matches the remote file.
    Valid { file: File, state: DownloadState },
    /// An explicit resume was requested but the pair is unusable (stale or
    /// missing); the caller must report and stop.
    GiveUp(ResumeError),
    /// Stale or missing state on a plain download; the caller drops the partial
    /// files and opens fresh.
    Discard,
}

/// Borrow the shared `OpenOptions` presets used to open the `.part` file.
fn open_existing() -> OpenOptions {
    let mut o = OpenOptions::new();
    o.read(true).write(true).truncate(false).create(false);
    o
}
fn open_create() -> OpenOptions {
    let mut o = OpenOptions::new();
    o.read(true).write(true).truncate(false).create(true);
    o
}
fn open_create_new() -> OpenOptions {
    let mut o = OpenOptions::new();
    o.read(true).write(true).truncate(false).create_new(true);
    o
}

/// Classify the result of probing an existing `.part`/`.fd` pair for resume.
///
/// Pure: no I/O, no event emission. The two probe results (`open` the `.part`,
/// `load` the `.fd`) map onto one of three outcomes so the resume *contract*
/// lives in a single, unit-testable place. The outcomes are
/// [`ResumeProbe::Valid`] (file + state present and still match the remote
/// file; resume from it), [`ResumeProbe::GiveUp`] (an explicit resume was
/// requested but the pair is unusable; report and stop), and
/// [`ResumeProbe::Discard`] (stale or missing state on a plain download; drop
/// the partial files and open fresh).
fn classify_resume<E>(
    open_res: std::io::Result<File>,
    load_res: Result<DownloadState, E>,
    explicit_resume: bool,
    info: &UrlInfo,
) -> ResumeProbe {
    match (open_res, load_res) {
        (Ok(file), Ok(state)) if state.validate(info) => ResumeProbe::Valid { file, state },
        // Only an *explicit* resume (`resume()`) treats a stale/missing state as
        // a hard error (the resume contract). A plain `download()` that happens
        // to find a leftover `.part`/`.fd` discards them and re-downloads
        // silently — so `explicit_resume` (true only for `resume()`) gates the
        // `GiveUp` arms and everything else falls through to `Discard`.
        (Ok(_), Ok(state)) if explicit_resume => ResumeProbe::GiveUp(ResumeError::FileChanged {
            local_file_id: state.file_id(),
            local_file_size: state.size.unwrap_or(0),
            remote_file_id: info.file_id.clone(),
            remote_file_size: info.size,
        }),
        (Ok(_), Err(_)) if explicit_resume => ResumeProbe::GiveUp(ResumeError::NoStateFile),
        _ => ResumeProbe::Discard,
    }
}

/// Try to resume from an existing `.part`/`.fd` pair, or open a fresh `.part`
/// file. Returns:
///
/// - [`Acquire::Ready`] with a writable `file` + the effective config + the
///   resume state (if any);
/// - [`Acquire::CollisionRetry`] in unique mode when `create_new` failed
///   (treats the failure as a name collision and asks the caller to retry);
/// - [`Acquire::Abort`] when an unrecoverable error has already been reported
///   through `tx` (e.g. a `resume()` contract violation, or a non-unique open
///   failure), and the caller should stop.
#[allow(clippy::too_many_arguments)]
pub(super) async fn try_acquire_target(
    tx: &Tx,
    can_resume: bool,
    explicit_resume: bool,
    unique: bool,
    info: &UrlInfo,
    partial_config: &PartialConfig,
    tmp: &Path,
    cfg: &Path,
) -> Acquire {
    // ---- 1. Try to resume from an existing `.part`/`.fd` pair ----
    if can_resume {
        let opener = open_existing();
        let (open_res, load_res) = tokio::join!(opener.open(tmp), DownloadState::load(cfg));
        match classify_resume(open_res, load_res, explicit_resume, info) {
            ResumeProbe::Valid { file, state } => {
                let mut pc = partial_config.clone();
                if let Some(c) = &state.config {
                    pc.inherit_from(c);
                }
                // Reconstruct `effective` from the resume state in one step: the
                // `config` half comes from `inherit_from(state.config)` above, and
                // the saved-progress half is folded into `downloaded_chunk` inside
                // `build_seeded`. So `effective` is a pure function of `pc` + the
                // saved progress (no post-build mutation), and `parsed`/`effective`
                // stay in sync by construction — `pc` carries no progress at all.
                let effective = pc.build_seeded(state.progress.as_deref());
                let parsed = pc;
                return Acquire::Ready {
                    file,
                    effective,
                    parsed,
                    resume_state: Some(state),
                };
            }
            ResumeProbe::GiveUp(err) => {
                // Explicit resume target but the `.part`/`.fd` pair is unusable
                // (stale or missing): report and stop rather than silently
                // re-downloading (resume contract).
                let _ = tx.send(Event::ResumeError(err));
                return Acquire::Abort;
            }
            ResumeProbe::Discard => {
                // Stale or missing state on a plain download: drop the partial
                // files and fall through to a fresh open below.
                let _ = fs::remove_file(tmp).await;
                let _ = fs::remove_file(cfg).await;
            }
        }
    }

    // ---- 2. Fresh open (also the fall-through after discarding a stale state) ----
    let f = if unique {
        open_create_new().open(tmp).await.ok()
    } else {
        match open_create().open(tmp).await {
            Ok(f) => Some(f),
            Err(e) => {
                let _ = tx.send(Event::BuildPusherError(e));
                return Acquire::Abort;
            }
        }
    };
    f.map_or_else(
        || Acquire::CollisionRetry,
        |file| {
            let pc = partial_config.clone();
            let effective = pc.build_seeded(None);
            Acquire::Ready {
                file,
                effective,
                parsed: pc,
                resume_state: None,
            }
        },
    )
}
