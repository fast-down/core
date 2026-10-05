//! Aggregated download statistics: folds the engine's per-worker
//! fetch/write events into the [`DownloadStats`] carried by
//! [`Event::Progress`].
//!
//! This is deliberately pure and single-owner: the run loop feeds it every
//! engine event and asks for a snapshot on its progress cadence, so no state is
//! shared between tasks.

use crate::{DownloadStats, TransferStats, WorkerState, WorkerStats};
use fast_down::{Merge, Total, WorkerId};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

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
            let dt = now - t;
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

/// One direction's running state: bytes seen, smoothed rate, and the
/// out-of-order ranges it has covered.
#[derive(Debug, Default)]
struct Direction {
    downloaded: u64,
    bps: u64,
    progress: Vec<fast_down::ProgressEntry>,
    rate: RateEstimator,
}

impl Direction {
    /// Seed this direction with progress carried over from a previous run, so
    /// the snapshot covers the whole file rather than only this session's bytes.
    fn seed(&mut self, initial: &[fast_down::ProgressEntry]) {
        self.progress = initial.to_vec();
        self.downloaded = self.progress.total();
    }

    /// Fold a freshly-observed range into this direction and update its rate.
    ///
    /// `downloaded` is derived from the merged ranges (never accumulated
    /// directly), so an overlapping or repeated range cannot inflate it past the
    /// bytes actually covered — it stays consistent with `progress`.
    fn record(&mut self, range: &fast_down::ProgressEntry, now: Instant) {
        self.progress.merge_progress(range.clone());
        self.downloaded = self.progress.total();
        self.bps = self.rate.observe(now, self.downloaded);
    }

    /// Build this direction's public stats for a `total`-byte file and the
    /// session-wide `elapsed`.
    fn stats(&self, total: u64, elapsed: Duration) -> TransferStats {
        let avg_bps = avg_bps(self.downloaded, elapsed);
        TransferStats {
            downloaded: self.downloaded,
            bps: self.bps,
            percent: percent(self.downloaded, total),
            avg_bps,
            eta: eta(total, self.downloaded, self.bps, avg_bps),
        }
    }
}

/// One worker's pull-side running state.
#[derive(Debug)]
struct WorkerTracker {
    state: WorkerState,
    pulled: u64,
    bps: u64,
    rate: RateEstimator,
}

impl Default for WorkerTracker {
    fn default() -> Self {
        Self {
            state: WorkerState::Pulling,
            pulled: 0,
            bps: 0,
            rate: RateEstimator::default(),
        }
    }
}

impl WorkerTracker {
    /// Fold a freshly-fetched range into this worker: add its raw byte length
    /// (re-fetches included) and update its rate.
    fn record(&mut self, range: &fast_down::ProgressEntry, now: Instant) {
        self.pulled = self.pulled.saturating_add(range.total());
        self.bps = self.rate.observe(now, self.pulled);
    }
}

/// Folds engine events into aggregate and per-worker statistics.
#[derive(Debug)]
pub struct StatsTracker {
    total: u64,
    loaded_elapsed: Duration,
    start: Instant,
    pull: Direction,
    push: Direction,
    workers: BTreeMap<WorkerId, WorkerTracker>,
}

impl StatsTracker {
    /// Create a tracker for a `total`-byte file whose prior resume runs
    /// contributed `loaded_elapsed` of active time and `initial` bytes already on
    /// disk. Both directions start from `initial`, so the snapshot covers the
    /// whole file on a resumed run.
    #[must_use]
    pub fn new(total: u64, loaded_elapsed: Duration, initial: &[fast_down::ProgressEntry]) -> Self {
        let mut pull = Direction::default();
        let mut push = Direction::default();
        pull.seed(initial);
        push.seed(initial);
        Self {
            total,
            loaded_elapsed,
            start: Instant::now(),
            pull,
            push,
            workers: BTreeMap::new(),
        }
    }

    /// Fold one engine event in, updating the matching worker and direction.
    pub fn observe<RE, WE>(&mut self, event: &fast_down::Event<RE, WE>, now: Instant) {
        match event {
            fast_down::Event::Pulling(id) => {
                self.workers.entry(*id).or_default().state = WorkerState::Pulling;
            }
            fast_down::Event::PullProgress(id, range) => {
                self.workers.entry(*id).or_default().record(range, now);
                self.pull.record(range, now);
            }
            fast_down::Event::Pushing(id, _) => {
                self.workers.entry(*id).or_default().state = WorkerState::Pushing;
            }
            fast_down::Event::PushProgress(range) => self.push.record(range, now),
            fast_down::Event::PullTimeout(id) => {
                self.workers.entry(*id).or_default().state = WorkerState::TimedOut;
            }
            fast_down::Event::Finished(id) => {
                self.workers.entry(*id).or_default().state = WorkerState::Finished;
            }
            _ => {}
        }
    }

    /// The total active time so far (prior runs plus this one).
    fn elapsed(&self) -> Duration {
        self.loaded_elapsed.saturating_add(self.start.elapsed())
    }

    /// The written ranges (the push side) in canonical, merged form.
    fn written(&self) -> &[fast_down::ProgressEntry] {
        &self.push.progress
    }

    /// A live snapshot. Rates are the current EMAs.
    #[must_use]
    pub fn snapshot(&self) -> DownloadStats {
        self.build(self.elapsed(), false)
    }

    /// A terminal snapshot taken at `elapsed`: all rates are zeroed, since no
    /// further bytes will arrive and a stale EMA would mislead. The caller
    /// passes the same elapsed it persisted, so the final frame and the `.fd`
    /// clock agree exactly (a resumed run then continues from it).
    #[must_use]
    pub fn snapshot_terminal(&self, elapsed: Duration) -> DownloadStats {
        self.build(elapsed, true)
    }

    fn build(&self, elapsed: Duration, terminal: bool) -> DownloadStats {
        let (pull, push, workers) = if terminal {
            let zero = |d: &Direction| {
                let avg_bps = avg_bps(d.downloaded, elapsed);
                TransferStats {
                    downloaded: d.downloaded,
                    bps: 0,
                    percent: percent(d.downloaded, self.total),
                    avg_bps,
                    eta: eta(self.total, d.downloaded, 0, avg_bps),
                }
            };
            let workers = self
                .workers
                .iter()
                .map(|(id, w)| {
                    (
                        *id,
                        WorkerStats {
                            state: w.state,
                            pulled: w.pulled,
                            bps: 0,
                        },
                    )
                })
                .collect();
            (zero(&self.pull), zero(&self.push), workers)
        } else {
            let workers = self
                .workers
                .iter()
                .map(|(id, w)| {
                    (
                        *id,
                        WorkerStats {
                            state: w.state,
                            pulled: w.pulled,
                            bps: w.bps,
                        },
                    )
                })
                .collect();
            (
                self.pull.stats(self.total, elapsed),
                self.push.stats(self.total, elapsed),
                workers,
            )
        };
        DownloadStats {
            progress: self.written().to_vec(),
            total: self.total,
            elapsed,
            pull,
            push,
            workers,
        }
    }
}

/// Percentage of `total` covered by `downloaded`; `0.0` for a zero-size file.
fn percent(downloaded: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        #[allow(clippy::cast_precision_loss)]
        {
            downloaded as f64 / total as f64 * 100.0
        }
    }
}

/// Session-wide average rate in bytes/second.
fn avg_bps(downloaded: u64, elapsed: Duration) -> u64 {
    let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    downloaded
        .saturating_mul(1000)
        .checked_div(elapsed_ms)
        .unwrap_or(0)
}

/// Remaining time = `(total - downloaded) / rate`, preferring the smoothed
/// recent `bps` and falling back to `avg_bps` while the EMA warms up.
fn eta(total: u64, downloaded: u64, bps: u64, avg_bps: u64) -> Option<Duration> {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use fast_down::Event;
    use std::convert::Infallible;

    type TestEvent = Event<Infallible, Infallible>;

    fn tracker(total: u64) -> StatsTracker {
        StatsTracker::new(total, Duration::ZERO, &[])
    }

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
            "next interval measures from the new baseline"
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
        assert!(decayed < warm, "a stall must decay the EMA");
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn pull_and_push_are_tracked_separately() {
        let mut t = tracker(1000);
        let now = Instant::now();
        // Pull runs ahead: two ranges pulled, only one written.
        t.observe(&TestEvent::Pulling(0), now);
        t.observe(&TestEvent::PullProgress(0, 0..300), now);
        t.observe(&TestEvent::PullProgress(0, 300..500), now);
        t.observe(&TestEvent::Pushing(0, 0..300), now);
        t.observe(&TestEvent::PushProgress(0..300), now);

        let s = t.snapshot();
        assert_eq!(s.pull.downloaded, 500, "pull counts both fetched ranges");
        assert_eq!(s.push.downloaded, 300, "push counts only the written range");
        assert!(s.pull.downloaded > s.push.downloaded, "pull leads push");
        assert_eq!(
            s.progress,
            vec![0u64..300],
            "snapshot progress is the written side"
        );
        assert!((s.push.percent - 30.0).abs() < f64::EPSILON);
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn overlapping_ranges_do_not_inflate_downloaded() {
        // A re-sent or overlapping range must not raise `downloaded` past the
        // bytes actually covered: `downloaded` is derived from the merged
        // ranges, so it stays consistent with `progress`.
        let mut t = tracker(1000);
        let now = Instant::now();
        t.observe(&TestEvent::PushProgress(0..300), now);
        t.observe(&TestEvent::PushProgress(0..300), now); // exact duplicate
        t.observe(&TestEvent::PushProgress(200..500), now); // partial overlap
        let s = t.snapshot();
        assert_eq!(s.push.downloaded, 500, "coverage is [0..500), counted once");
        assert_eq!(s.progress, vec![0u64..500]);
        assert!(
            (s.push.percent - 50.0).abs() < f64::EPSILON,
            "no >100% inflation"
        );
    }

    #[test]
    fn per_worker_stats_are_attributed_by_id() {
        let mut t = tracker(1000);
        let now = Instant::now();
        t.observe(&TestEvent::Pulling(1), now);
        t.observe(&TestEvent::Pulling(2), now);
        t.observe(&TestEvent::PullProgress(1, 0..100), now);
        t.observe(
            &TestEvent::PullProgress(2, 100..250),
            now + Duration::from_secs(1),
        );
        t.observe(&TestEvent::Finished(1), now + Duration::from_secs(1));

        let s = t.snapshot();
        assert_eq!(s.workers.len(), 2);
        assert_eq!(s.workers[&1].pulled, 100, "worker 1's bytes");
        assert_eq!(s.workers[&2].pulled, 150, "worker 2's bytes");
        assert_eq!(s.workers[&1].state, WorkerState::Finished);
        assert_eq!(s.workers[&2].state, WorkerState::Pulling);
        assert_eq!(s.pull.downloaded, 250, "aggregate is the sum");
    }

    #[test]
    fn push_state_and_timeout_are_recorded() {
        let mut t = tracker(1000);
        let now = Instant::now();
        t.observe(&TestEvent::Pushing(3, 0..10), now);
        assert_eq!(t.snapshot().workers[&3].state, WorkerState::Pushing);
        t.observe(&TestEvent::PullTimeout(3), now);
        assert_eq!(t.snapshot().workers[&3].state, WorkerState::TimedOut);
    }

    #[test]
    fn terminal_snapshot_zeroes_rates() {
        // A non-zero prior elapsed makes `avg_bps` (and thus the ETA) measurable.
        let mut t = StatsTracker::new(1000, Duration::from_secs(1), &[]);
        let now = Instant::now();
        t.observe(&TestEvent::PullProgress(0, 0..500), now);
        t.observe(&TestEvent::PushProgress(0..500), now);
        // Warm the EMAs across an interval.
        t.observe(
            &TestEvent::PullProgress(0, 500..1000),
            now + Duration::from_secs(1),
        );
        t.observe(
            &TestEvent::PushProgress(500..1000),
            now + Duration::from_secs(1),
        );

        let live = t.snapshot();
        assert!(live.pull.bps > 0 || live.push.bps > 0, "EMA warmed up");

        let terminal = t.snapshot_terminal(Duration::from_secs(2));
        assert_eq!(terminal.pull.bps, 0);
        assert_eq!(terminal.push.bps, 0);
        assert_eq!(terminal.push.downloaded, 1000);
        assert!((terminal.push.percent - 100.0).abs() < f64::EPSILON);
        assert_eq!(terminal.push.eta, Some(Duration::ZERO));
        assert!(terminal.workers.values().all(|w| w.bps == 0));
    }

    #[test]
    fn zero_total_reports_zero_percent() {
        let t = tracker(0);
        let s = t.snapshot();
        assert!(s.push.percent.abs() < f64::EPSILON);
        assert!(s.pull.percent.abs() < f64::EPSILON);
        assert!(s.push.eta.is_none());
    }
}
