//! Aggregated progress reporting: turns the written byte ranges and the
//! accumulated active time into the [`ProgressSample`] carried by
//! [`Event::Progress`].
//!
//! This is deliberately pure: the run loop owns the progress and passes it in,
//! so no state is shared between tasks. Rate smoothing state
//! ([`RateEstimator`]) is owned by the run loop too.

use crate::ProgressSample;
use fast_down::Total;
use std::time::{Duration, Instant};

/// Smooths the instantaneous transfer rate with an exponential moving average.
///
/// Raw per-interval deltas are too jittery for display: TCP congestion-window
/// swings, kernel buffering bursts and disk flush stalls make adjacent windows
/// differ by multiples. The EMA uses the time-constant form
/// `alpha = 1 - exp(-dt / TAU)`, which stays correct for irregular sampling
/// intervals. With `TAU` ≈ 3s the value tracks genuine throughput changes
/// within a few seconds while filtering sub-second noise.
#[derive(Debug, Default)]
pub struct RateEstimator {
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

/// Compute one aggregate [`ProgressSample`].
///
/// `rate` is the smoothing state; pass `None` for a terminal sample, which
/// forces `bps = 0`. `elapsed` is the total active time (prior runs plus this
/// one).
#[must_use]
pub fn sample(
    progress: Vec<fast_down::ProgressEntry>,
    total: u64,
    elapsed: Duration,
    now: Instant,
    rate: Option<&mut RateEstimator>,
) -> ProgressSample {
    let downloaded = progress.total();
    let bps = rate.map_or(0, |r| r.observe(now, downloaded));

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
    // smoothed recent rate: the session-wide average is dragged by history and
    // lies for most of the run after a resume at a different speed. Fall back to
    // `avg_bps` only while the EMA is still warming up.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_estimator_smooths_and_lags() {
        let mut est = RateEstimator::default();
        let t0 = Instant::now();
        assert_eq!(est.observe(t0, 0), 0, "first observation has no interval");

        let e1 = est.observe(t0 + Duration::from_secs(1), 100);
        assert!(e1 > 0 && e1 < 100, "EMA must lag the raw rate, got {e1}");
    }

    #[test]
    fn rate_estimator_zero_dt_skips_update() {
        let mut est = RateEstimator::default();
        let t0 = Instant::now();
        assert_eq!(est.observe(t0, 0), 0);
        assert_eq!(
            est.observe(t0, 500),
            0,
            "zero dt must leave the EMA untouched"
        );
        let r = est.observe(t0 + Duration::from_secs(1), 600);
        assert!(
            r > 0 && r < 100,
            "next interval measures from the new baseline, got {r}"
        );
    }

    #[test]
    fn rate_estimator_decays_when_stalled() {
        let mut est = RateEstimator::default();
        let t0 = Instant::now();
        est.observe(t0, 0);
        let warm = est.observe(t0 + Duration::from_secs(1), 100);
        assert!(warm > 0);
        let decayed = est.observe(t0 + Duration::from_secs(2), 100);
        assert!(
            decayed < warm,
            "a stall must decay the EMA: {decayed} >= {warm}"
        );
    }

    #[test]
    fn sample_reports_zero_percent_when_total_is_zero() {
        let s = sample(Vec::new(), 0, Duration::ZERO, Instant::now(), None);
        assert_eq!(s.total, 0);
        assert!(s.percent.abs() < f64::EPSILON);
        assert_eq!(s.downloaded, 0);
        assert!(s.eta.is_none());
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn sample_derives_percent_and_eta() {
        let s = sample(
            vec![0u64..500],
            1000,
            Duration::from_secs(1),
            Instant::now(),
            None,
        );
        assert_eq!(s.downloaded, 500);
        assert!((s.percent - 50.0).abs() < f64::EPSILON);
        assert_eq!(s.bps, 0, "no rate estimator forces bps to 0");
        assert!(s.avg_bps > 0);
        assert!(s.eta.is_some());
    }
}
