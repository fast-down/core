//! Aggregated progress reporting, decoupled from the download engine.
//!
//! [`ProgressReporter`] reads the already-written byte ranges from the shared
//! [`DownloadState`](crate::DownloadState) (handed in via
//! [`DownloadState::share_inner`](crate::DownloadState::share_inner)) and computes
//! the [`ProgressSample`] (rate, percentage, ETA, …) carried by
//! [`crate::Event::Progress`]. It does **not** own a separate snapshot and never
//! mutates progress itself — the engine loop is the single writer
//! ([`DownloadState::merge_progress`](crate::DownloadState::merge_progress)), which
//! also marks the state dirty so progress is persisted for resume. The reporter
//! runs as an independent task ([`ProgressReporter::spawn`]) so its emit cadence is
//! driven purely by `Config::progress_emit_gap` and is never delayed by flushing,
//! state saving, event forwarding, or a slow consumer (the channel is unbounded).
use crate::{Event, ProgressSample, Tx, core::state::PartialDownloadStateInner};
use fast_down::Total;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::spawn;
use tokio::time::sleep;

/// Smooths the instantaneous transfer rate with an exponential moving average.
///
/// Raw per-interval deltas (default window: 500ms) are too jittery for display:
/// TCP congestion-window swings, kernel buffering bursts and disk flush stalls
/// make adjacent windows differ by multiples. The EMA uses the time-constant
/// form `alpha = 1 - exp(-dt / TAU)`, which stays correct for irregular
/// sampling intervals. With `TAU` ≈ 3s the value tracks genuine throughput
/// changes within a few seconds while filtering sub-second noise.
#[derive(Default)]
pub(super) struct RateEstimator {
    /// Previous observation: `(instant, downloaded)`.
    last: Option<(Instant, u64)>,
    /// Current smoothed rate in bytes/second.
    ema_bps: f64,
}

impl RateEstimator {
    /// EMA time constant: how far back the smoothed rate "remembers".
    const TAU: Duration = Duration::from_secs(3);

    /// Feed one `(now, downloaded)` observation; returns the smoothed rate.
    ///
    /// Returns `0` on the first observation (no interval to measure yet).
    pub fn observe(&mut self, now: Instant, downloaded: u64) -> u64 {
        if let Some((t, b)) = self.last {
            let dt = now.duration_since(t);
            if !dt.is_zero() {
                #[allow(clippy::cast_precision_loss)]
                let raw = downloaded.saturating_sub(b) as f64 / dt.as_secs_f64();
                let alpha = 1.0 - (-dt.as_secs_f64() / Self::TAU.as_secs_f64()).exp();
                self.ema_bps = alpha.mul_add(raw - self.ema_bps, self.ema_bps);
            }
        }
        self.last = Some((now, downloaded));
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            self.ema_bps as u64
        }
    }
}

/// Drives [`crate::Event::Progress`] emission on a fixed cadence.
///
/// Cheap to clone: `state` is an `Arc`, and every other field is a small
/// read-only `Copy` value captured at construction time.
#[derive(Clone)]
pub(super) struct ProgressReporter {
    /// The single source of truth: already-written byte ranges from `DownloadState`.
    state: Arc<Mutex<PartialDownloadStateInner>>,
    /// Remote file size, used to derive percentage and remaining bytes.
    total: u64,
    /// Wall-clock moment the current run began (after pipeline setup).
    start: Instant,
    /// Active time already spent on this download in prior resume runs.
    loaded_elapsed: Duration,
}

impl ProgressReporter {
    /// Capture the starting point of this run.
    ///
    /// `loaded_elapsed` is the time persisted across prior resume runs (so the
    /// session-wide average speed continues rather than resetting). `state` is
    /// the shared inner of the [`DownloadState`](crate::DownloadState); it already
    /// holds the progress written to disk in prior runs, which this reporter reads
    /// directly in [`ProgressReporter::compute`]. The reporter never writes to it.
    pub fn new(
        loaded_elapsed: Duration,
        total: u64,
        state: Arc<Mutex<PartialDownloadStateInner>>,
    ) -> Self {
        Self {
            state,
            total,
            start: Instant::now(),
            loaded_elapsed,
        }
    }

    /// Total active time so far: prior runs plus this run's wall-clock.
    #[must_use]
    pub fn elapsed_now(&self, now: Instant) -> Duration {
        self.loaded_elapsed
            .saturating_add(now.duration_since(self.start))
    }

    /// Compute the current aggregate sample.
    ///
    /// `rate` is the smoothing state for the recent transfer rate; it observes
    /// the same `downloaded` value carried by the returned sample. Pass `None`
    /// for a terminal sample (forces `bps = 0`).
    #[must_use]
    pub fn compute(&self, now: Instant, rate: Option<&mut RateEstimator>) -> ProgressSample {
        let progress = self
            .state
            .lock()
            .config
            .as_ref()
            .and_then(|c| c.downloaded_chunk.clone())
            .unwrap_or_default();
        let downloaded = progress.total();
        let total = self.total;

        let bps = rate.map_or(0, |r| r.observe(now, downloaded));

        let elapsed = self.elapsed_now(now);
        let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        let avg_bps = downloaded
            .saturating_mul(1000)
            .checked_div(elapsed_ms)
            .unwrap_or(0);

        let percent = if total == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            {
                downloaded as f64 / total as f64 * 100.0
            }
        };

        // Remaining time = (total - downloaded) / effective rate. Prefer the
        // smoothed recent rate (`bps`): the session-wide `avg_bps` is dragged
        // by history — after a resume at a different speed, or a slow first
        // half, it converges far too slowly and the ETA lies for most of the
        // run. Fall back to `avg_bps` only while the EMA is still warming up.
        // `None` until any rate can be measured.
        let eta = {
            let remaining = total.saturating_sub(downloaded);
            let rate = if bps > 0 { bps } else { avg_bps };
            if rate == 0 {
                None
            } else {
                remaining
                    .saturating_mul(1000)
                    .checked_div(rate)
                    .map(Duration::from_millis)
            }
        };

        ProgressSample {
            progress,
            bps,
            avg_bps,
            downloaded,
            percent,
            total,
            elapsed,
            eta,
        }
    }

    /// Spawn the cadence-driven reporter task.
    ///
    /// The task emits [`crate::Event::Progress`] every `gap` until the caller
    /// [`abort`](tokio::task::JoinHandle::abort)s the returned handle. The loop
    /// holds no state that needs cleanup and its only await point is the
    /// `sleep`, so aborting is safe: a `compute` + `send` pair is synchronous
    /// and can never be torn in half. Callers must still `await` the aborted
    /// handle before emitting a terminal sample, so no in-flight tick can land
    /// after the terminal progress event. Consumes `self` (a clone is typically
    /// handed to the caller beforehand for the terminal
    /// [`ProgressReporter::compute`]).
    #[must_use]
    pub fn spawn(self, tx: &Tx, gap: Duration) -> tokio::task::JoinHandle<()> {
        let tx = tx.clone();
        spawn(async move {
            let mut rate = RateEstimator::default();
            loop {
                let sample = self.compute(Instant::now(), Some(&mut rate));
                let _ = tx.send(Event::Progress(sample));
                sleep(gap).await;
            }
        })
    }
}
