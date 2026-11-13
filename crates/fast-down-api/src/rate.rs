//! Byte counters and smoothed transfer rates shared by the engine.

use crate::TransferStats;
use std::time::{Duration, Instant};

/// A smoothed transfer rate: an exponential moving average over a monotone byte
/// counter. The time-constant form accounts for irregular sampling intervals.
#[derive(Debug, Default)]
struct Speed {
    last_sample: Option<(Instant, u64)>,
    smoothed_bps: f64,
}

impl Speed {
    /// EMA time constant: how far back the smoothed rate "remembers".
    const TAU: Duration = Duration::from_secs(3);

    /// Feed one `(now, bytes)` observation; returns the smoothed rate. Returns
    /// zero until a second observation gives an interval to measure.
    fn sample(&mut self, now: Instant, bytes: u64) -> u64 {
        if let Some((last_time, last_bytes)) = self.last_sample {
            let since_last = now - last_time;
            if !since_last.is_zero() {
                #[allow(clippy::cast_precision_loss)]
                let observed_bps =
                    bytes.saturating_sub(last_bytes) as f64 / since_last.as_secs_f64();
                let weight = 1.0 - (-since_last.as_secs_f64() / Self::TAU.as_secs_f64()).exp();
                self.smoothed_bps =
                    weight.mul_add(observed_bps - self.smoothed_bps, self.smoothed_bps);
            }
        }
        self.last_sample = Some((now, bytes));
        self.bps()
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    const fn bps(&self) -> u64 {
        self.smoothed_bps as u64
    }
}

/// A byte counter and its smoothed rate.
///
/// Counters are updated as bytes are observed; [`sample`](Self::sample) takes
/// the rate once per report, so a stalled transfer's rate decays instead of
/// freezing at its last value.
#[derive(Debug, Default)]
pub struct Meter {
    bytes: u64,
    speed: Speed,
}

impl Meter {
    /// Replace the counter with an absolute value (deduplicated coverage).
    pub(crate) const fn set(&mut self, bytes: u64) {
        self.bytes = bytes;
    }

    /// Add bytes observed since the last call (raw, re-fetches included).
    pub(crate) const fn add(&mut self, bytes: u64) {
        self.bytes = self.bytes.saturating_add(bytes);
    }

    pub(crate) const fn bytes(&self) -> u64 {
        self.bytes
    }

    pub(crate) const fn bps(&self) -> u64 {
        self.speed.bps()
    }

    /// Sample the rate against the counter's current value.
    pub(crate) fn sample(&mut self, now: Instant) -> u64 {
        self.speed.sample(now, self.bytes)
    }

    /// A live snapshot of this counter and its rate.
    pub(crate) fn stats(&self, total: u64, elapsed: Duration) -> TransferStats {
        TransferStats::new(self.bytes, self.bps(), total, elapsed)
    }

    /// A terminal snapshot: no further bytes arrive, so the live rate is zero.
    pub(crate) fn terminal_stats(&self, total: u64, elapsed: Duration) -> TransferStats {
        TransferStats::new(self.bytes, 0, total, elapsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_estimator_smooths_and_lags() {
        let mut est = Speed::default();
        let t0 = Instant::now();
        assert_eq!(est.sample(t0, 0), 0, "first observation has no interval");
        let e1 = est.sample(t0 + Duration::from_secs(1), 100);
        assert!(e1 > 0 && e1 < 100, "EMA must lag the raw rate, got {e1}");
    }

    #[test]
    fn rate_estimator_zero_dt_skips_update() {
        let mut est = Speed::default();
        let t0 = Instant::now();
        assert_eq!(est.sample(t0, 0), 0);
        assert_eq!(
            est.sample(t0, 500),
            0,
            "zero dt must leave the EMA untouched"
        );
        let r = est.sample(t0 + Duration::from_secs(1), 600);
        assert!(
            r > 0 && r < 100,
            "next interval measures from the new baseline"
        );
    }

    #[test]
    fn rate_estimator_decays_when_stalled() {
        let mut est = Speed::default();
        let t0 = Instant::now();
        est.sample(t0, 0);
        let warm = est.sample(t0 + Duration::from_secs(1), 100);
        assert!(warm > 0);
        let decayed = est.sample(t0 + Duration::from_secs(2), 100);
        assert!(decayed < warm, "a stall must decay the EMA");
    }

    #[test]
    fn meter_accumulates_and_samples_the_rate() {
        let mut meter = Meter::default();
        meter.add(100);
        meter.add(50);
        assert_eq!(meter.bytes(), 150);

        let t0 = Instant::now();
        assert_eq!(meter.sample(t0), 0, "first sample has no interval");
        meter.add(150);
        let bps = meter.sample(t0 + Duration::from_secs(1));
        assert!(bps > 0 && bps < 150, "EMA must lag the raw rate, got {bps}");
    }
}
