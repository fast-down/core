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

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines)]
    use super::{ProgressReporter, RateEstimator};
    use crate::PartialConfig;
    use crate::core::state::DownloadState;
    use fast_down::UrlInfo;
    use std::path::Path;
    use std::time::{Duration, Instant};
    use url::Url;

    #[test]
    fn compute_reports_zero_percent_when_total_is_zero() {
        // Covers progress_reporter.rs:133-134 (total == 0 short-circuit:
        // percent is forced to 0.0 instead of dividing by zero).
        let url = Url::parse("https://example.com/x").unwrap();
        let info = UrlInfo {
            size: 0,
            raw_name: "x".to_string(),
            supports_range: false,
            fast_download: false,
            final_url: url.clone(),
            file_id: fast_down::FileId::new(None, None),
            content_type: None,
        };
        let state = DownloadState::new(
            &url,
            &info,
            &PartialConfig::default(),
            Path::new("/tmp/_pr_zero_total.fd"),
        );
        let reporter = ProgressReporter::new(Duration::ZERO, 0, state.share_inner());
        let sample = reporter.compute(Instant::now(), None);
        assert_eq!(sample.total, 0);
        assert!(
            (sample.percent).abs() < f64::EPSILON,
            "percent must be 0.0 when total is 0, got {}",
            sample.percent
        );
        assert_eq!(sample.downloaded, 0);
        assert!(sample.eta.is_none());
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn compute_reports_half_progress_fields() {
        // compute (progress_reporter.rs lines 113-171) must derive percent/eta
        // from the recorded on-disk progress. 500/1000 bytes => 50%, and with a
        // non-zero loaded_elapsed the ETA is computable.
        let url = Url::parse("https://example.com/x").unwrap();
        let info = UrlInfo {
            size: 1000,
            raw_name: "x".to_string(),
            supports_range: true,
            fast_download: true,
            final_url: url.clone(),
            file_id: fast_down::FileId::new(None, None),
            content_type: None,
        };
        let state = DownloadState::new(
            &url,
            &info,
            &PartialConfig::default(),
            Path::new("/tmp/_pr_fields.fd"),
        );
        state.update(|inner| {
            inner.config.get_or_insert_default().downloaded_chunk = Some(vec![0u64..500]);
        });

        let reporter = ProgressReporter::new(Duration::from_secs(1), 1000, state.share_inner());
        let sample = reporter.compute(Instant::now(), None);

        assert_eq!(sample.total, 1000);
        assert_eq!(sample.downloaded, 500);
        assert!(
            (sample.percent - 50.0).abs() < f64::EPSILON,
            "percent must be 50.0 for half-downloaded file, got {}",
            sample.percent
        );
        assert_eq!(sample.bps, 0, "no rate estimator => bps forced to 0");
        assert!(
            sample.avg_bps > 0,
            "avg_bps must be positive with non-zero elapsed"
        );
        assert!(
            sample.eta.is_some(),
            "eta must be computable once bytes are downloaded"
        );
    }

    #[test]
    fn rate_estimator_smooths_and_lags() {
        // RateEstimator::observe (progress_reporter.rs lines 45-60): the first
        // observation has no interval and returns 0; subsequent steady 100 bps
        // input is tracked by an EMA that lags (stays below 100) but rises toward it.
        let mut est = RateEstimator::default();
        let t0 = Instant::now();
        assert_eq!(
            est.observe(t0, 0),
            0,
            "first observation has no interval => 0"
        );

        let e1 = est.observe(t0 + Duration::from_secs(1), 100);
        assert!(
            e1 > 0 && e1 < 100,
            "EMA must lag the 100 bps raw rate, got {e1}"
        );

        let e2 = est.observe(t0 + Duration::from_secs(2), 200);
        assert!(
            e2 > e1,
            "EMA must increase toward the steady rate, got {e2}"
        );
        assert!(
            e2 <= 100,
            "EMA must stay at/below the steady 100 bps, got {e2}"
        );
    }

    #[test]
    fn rate_estimator_zero_dt_skips_update_and_advances_last() {
        // observe with a zero interval must not touch the EMA (returns the
        // unchanged smoothed value), but still advances `last` — so bytes
        // written within the same instant are dropped from rate accounting
        // and the next interval measures from the new baseline.
        let mut est = RateEstimator::default();
        let t0 = Instant::now();
        assert_eq!(est.observe(t0, 0), 0);
        assert_eq!(
            est.observe(t0, 500),
            0,
            "zero dt must leave the EMA (still 0) untouched"
        );
        // `last` advanced to (t0, 500): the 500 bytes are dropped; the next
        // interval only sees the 100 bytes written after t0.
        let r = est.observe(t0 + Duration::from_secs(1), 600);
        assert!(
            r > 0 && r < 100,
            "next interval must measure from the advanced baseline, got {r}"
        );
    }

    #[test]
    fn rate_estimator_converges_toward_steady_rate() {
        // A steady 100 bps feed must pull the EMA (started at 0) close to the
        // true rate within a few time constants.
        let mut est = RateEstimator::default();
        let t0 = Instant::now();
        est.observe(t0, 0);
        let mut last = 0u64;
        for i in 1u64..=10 {
            last = est.observe(t0 + Duration::from_secs(i), i * 100);
        }
        assert!(
            (90..=100).contains(&last),
            "EMA must converge to the steady 100 bps, got {last}"
        );
    }

    #[test]
    fn rate_estimator_decays_when_download_stalls() {
        // A stalled transfer (no new bytes) yields a raw rate of 0, so the EMA
        // must decay toward 0 instead of holding the previous speed.
        let mut est = RateEstimator::default();
        let t0 = Instant::now();
        est.observe(t0, 0);
        let warm = est.observe(t0 + Duration::from_secs(1), 100);
        assert!(warm > 0, "first interval must heat the EMA up");
        let decayed = est.observe(t0 + Duration::from_secs(2), 100);
        assert!(
            decayed < warm,
            "a stalled download must decay the EMA: {decayed} >= {warm}"
        );
    }

    #[test]
    fn elapsed_now_includes_loaded_elapsed() {
        // elapsed_now (progress_reporter.rs lines 102-105) sums the prior runs'
        // loaded_elapsed with this run's wall-clock.
        let url = Url::parse("https://example.com/x").unwrap();
        let info = UrlInfo {
            size: 1,
            raw_name: "x".to_string(),
            supports_range: false,
            fast_download: false,
            final_url: url.clone(),
            file_id: fast_down::FileId::new(None, None),
            content_type: None,
        };
        let state = DownloadState::new(
            &url,
            &info,
            &PartialConfig::default(),
            Path::new("/tmp/_pr_elapsed.fd"),
        );
        let loaded = Duration::from_secs(5);
        let reporter = ProgressReporter::new(loaded, 1, state.share_inner());
        let now = Instant::now();
        let elapsed = reporter.elapsed_now(now);
        assert!(
            elapsed >= loaded,
            "elapsed_now must include loaded_elapsed, got {elapsed:?}"
        );
        assert!(
            elapsed < loaded + Duration::from_secs(1),
            "elapsed must be ~loaded + tiny run time, got {elapsed:?}"
        );
    }
}
