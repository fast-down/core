//! How a resolved download chooses between resuming and starting fresh.
//!
//! These are pure value types: the run loop ([`super::runner`]) resolves a
//! [`StartPolicy`] against the probed [`crate::ResumeOutcome`], but nothing here
//! depends on the runner, so the policy can be reasoned about on its own.

/// A shorthanded preset for how a resolved download should be started.
///
/// Each variant maps onto a [`StartPolicy`] (see [`From<StartMode>`]); use
/// [`StartPolicy`] directly when you need per-dimension control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartMode {
    /// Resume when possible, otherwise download the whole file.
    Auto,
    /// Resume only: refuse (via [`Event::ResumeError`](crate::Event::ResumeError))
    /// when the recorded state exists but no longer describes the remote.
    Resume,
    /// Ignore any recorded progress and download the whole file.
    Fresh,
    /// Continue from a mismatched state when the size still matches.
    ///
    /// Soundness of a forced resume is decided by
    /// [`StateError::force_safe`](crate::StateError::force_safe); when the
    /// mismatch is not force-safe, `start` reports the failure instead of
    /// continuing.
    Forced,
}

impl From<StartMode> for StartPolicy {
    fn from(mode: StartMode) -> Self {
        match mode {
            StartMode::Auto => Self::auto(),
            StartMode::Resume => Self::resume(),
            StartMode::Fresh => Self::fresh(),
            StartMode::Forced => Self::forced(),
        }
    }
}

/// What to do when recorded state exists but cannot be resumed cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Recovery {
    /// Ignore the recorded state and download the whole file.
    #[default]
    Fresh,
    /// Report [`Event::ResumeError`](crate::Event::ResumeError) and stop.
    Fail,
}

/// What to do when the remote's identity headers changed but its **size** still
/// matches, so the recorded byte ranges still describe the same bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IdentityRecovery {
    /// Ignore the recorded state and download the whole file.
    #[default]
    Fresh,
    /// Report [`Event::ResumeError`](crate::Event::ResumeError) and stop.
    Fail,
    /// Continue from the recorded state despite the identity change. Sound only
    /// because the size matches — the byte layout is unchanged.
    Force,
}

/// Per-dimension policy for how [`Ready::start`](crate::Ready::start) handles a
/// recorded `.fd`/`.part` that cannot be resumed cleanly.
///
/// Each field covers one independent reason a resume might not be possible; set
/// exactly the ones you care about and take the rest from [`Default`] (which is
/// [`StartPolicy::auto`]). The common combinations are available as presets via
/// [`StartMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartPolicy {
    /// Use recorded progress when it is valid. `false` ignores any recorded
    /// progress and downloads the whole file, reusing a pinned `.fd`/`.part`
    /// location when one is set.
    pub use_recorded: bool,
    /// No `.fd`, or a `.fd` whose `.part` is gone — nothing to continue.
    pub no_state: Recovery,
    /// The `.part` is shorter than the recorded progress.
    pub truncated: Recovery,
    /// The `.fd` exists but cannot be read or decoded.
    pub unreadable: Recovery,
    /// The remote file's size changed, so the recorded ranges describe
    /// different bytes. Only [`Recovery`] is offered: continuing across a size
    /// change would splice two different files together, so there is no safe
    /// "force" option (unlike [`StartPolicy::identity_changed`]).
    pub size_changed: Recovery,
    /// The server does not support ranged requests.
    pub not_resumable: Recovery,
    /// The remote's identity headers changed but the size still matches.
    pub identity_changed: IdentityRecovery,
}

impl Default for StartPolicy {
    fn default() -> Self {
        Self::auto()
    }
}

impl StartPolicy {
    /// Resume when possible, otherwise download the whole file.
    #[must_use]
    pub const fn auto() -> Self {
        Self {
            use_recorded: true,
            no_state: Recovery::Fresh,
            truncated: Recovery::Fresh,
            unreadable: Recovery::Fresh,
            size_changed: Recovery::Fresh,
            not_resumable: Recovery::Fresh,
            identity_changed: IdentityRecovery::Fresh,
        }
    }

    /// Resume this specific state, refusing (via
    /// [`Event::ResumeError`](crate::Event::ResumeError)) when it can no longer
    /// be continued; only "nothing safe to continue" falls back.
    #[must_use]
    pub const fn resume() -> Self {
        Self {
            use_recorded: true,
            no_state: Recovery::Fresh,
            truncated: Recovery::Fresh,
            unreadable: Recovery::Fail,
            size_changed: Recovery::Fail,
            not_resumable: Recovery::Fail,
            identity_changed: IdentityRecovery::Fail,
        }
    }

    /// Ignore any recorded progress and download the whole file.
    #[must_use]
    pub const fn fresh() -> Self {
        Self {
            use_recorded: false,
            no_state: Recovery::Fresh,
            truncated: Recovery::Fresh,
            unreadable: Recovery::Fresh,
            size_changed: Recovery::Fresh,
            not_resumable: Recovery::Fresh,
            identity_changed: IdentityRecovery::Fresh,
        }
    }

    /// Continue from a mismatched state when the size still matches.
    #[must_use]
    pub const fn forced() -> Self {
        Self {
            use_recorded: true,
            no_state: Recovery::Fresh,
            truncated: Recovery::Fail,
            unreadable: Recovery::Fail,
            size_changed: Recovery::Fail,
            not_resumable: Recovery::Fail,
            identity_changed: IdentityRecovery::Force,
        }
    }
}
