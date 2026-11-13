//! Aggregated download statistics: folds the engine's per-worker
//! fetch/write events into the [`DownloadStats`] carried by
//! [`Event::Progress`](crate::Event::Progress).
//!
//! This is deliberately pure and single-owner: the run loop feeds it every
//! engine event and asks for a snapshot on its progress cadence, so no state is
//! shared between tasks. Events only move byte counters and worker states; rates
//! are sampled when a snapshot is taken.

use crate::{DownloadStats, WorkerState, WorkerStats, rate::Meter};
use fast_down::{Merge, Total, WorkerId};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

/// The pull side: deduplicated byte coverage and its meter.
#[derive(Debug, Default)]
struct Pull {
    meter: Meter,
    progress: Vec<fast_down::ProgressEntry>,
}

impl Pull {
    /// Seed this side with progress carried over from a previous run, so the
    /// snapshot covers the whole file rather than only this session's bytes.
    fn seed(&mut self, initial: &[fast_down::ProgressEntry]) {
        self.progress = initial.to_vec();
        self.meter.set(self.progress.total());
    }

    /// Fold a freshly-observed range in. Coverage is derived from the merged
    /// ranges, so an overlapping or repeated range cannot inflate it.
    fn record(&mut self, range: &fast_down::ProgressEntry) {
        self.progress.merge_progress(range.clone());
        self.meter.set(self.progress.total());
    }
}

/// One worker's pull-side running state.
#[derive(Debug)]
struct WorkerTracker {
    state: WorkerState,
    meter: Meter,
}

impl Default for WorkerTracker {
    fn default() -> Self {
        Self {
            state: WorkerState::Pulling,
            meter: Meter::default(),
        }
    }
}

impl WorkerTracker {
    /// Fold a freshly-fetched range into this worker: add its raw byte length
    /// (re-fetches included).
    fn record(&mut self, range: &fast_down::ProgressEntry) {
        self.meter.add(range.total());
    }
}

/// Folds engine events into aggregate and per-worker statistics.
#[derive(Debug)]
pub struct StatsTracker {
    total: u64,
    loaded_elapsed: Duration,
    start: Instant,
    pull: Pull,
    push: Meter,
    workers: BTreeMap<WorkerId, WorkerTracker>,
}

impl StatsTracker {
    /// Create a tracker for a `total`-byte file whose prior resume runs
    /// contributed `loaded_elapsed` of active time and `initial` bytes already on
    /// disk. Both directions start from `initial`, so the snapshot covers the
    /// whole file on a resumed run.
    #[must_use]
    pub fn new(total: u64, loaded_elapsed: Duration, initial: &[fast_down::ProgressEntry]) -> Self {
        let mut pull = Pull::default();
        pull.seed(initial);
        let mut push = Meter::default();
        push.set(initial.total());
        Self {
            total,
            loaded_elapsed,
            start: Instant::now(),
            pull,
            push,
            workers: BTreeMap::new(),
        }
    }

    /// Fold one engine event in, updating the matching worker and byte counters.
    pub fn observe<RE, WE>(&mut self, event: &fast_down::Event<RE, WE>) {
        match event {
            fast_down::Event::Pulling(id) => {
                self.workers.entry(*id).or_default().state = WorkerState::Pulling;
            }
            fast_down::Event::PullProgress(id, range) => {
                self.workers.entry(*id).or_default().record(range);
                self.pull.record(range);
            }
            fast_down::Event::Pushing(id, _) => {
                self.workers.entry(*id).or_default().state = WorkerState::Pushing;
            }
            fast_down::Event::PullTimeout(id) => {
                self.workers.entry(*id).or_default().state = WorkerState::TimedOut;
            }
            fast_down::Event::Finished(id) => {
                self.workers.entry(*id).or_default().state = WorkerState::Finished;
            }
            // The write side's coverage belongs to the persisted record, which
            // calls [`set_written`](Self::set_written) after each merge.
            _ => {}
        }
    }

    /// Set the push-side coverage from the written progress. The record is the
    /// single owner of that range set; this only mirrors its byte total.
    pub const fn set_written(&mut self, bytes: u64) {
        self.push.set(bytes);
    }

    /// Total active time up to `now` (prior runs plus this one).
    fn elapsed_at(&self, now: Instant) -> Duration {
        self.loaded_elapsed
            .saturating_add(now.saturating_duration_since(self.start))
    }

    /// A live snapshot at `now`, with `progress` the written ranges.
    ///
    /// Every meter is sampled here, so a transfer that has stalled decays toward
    /// zero instead of reporting its last value forever.
    #[must_use]
    pub fn snapshot(
        &mut self,
        now: Instant,
        progress: &[fast_down::ProgressEntry],
    ) -> DownloadStats {
        self.pull.meter.sample(now);
        self.push.sample(now);
        for worker in self.workers.values_mut() {
            worker.meter.sample(now);
        }
        self.build(self.elapsed_at(now), progress, false)
    }

    /// A terminal snapshot taken at `elapsed`: all rates are zeroed, since no
    /// further bytes will arrive and a stale EMA would mislead. The caller
    /// passes the same elapsed it persisted, so the final frame and the `.fd`
    /// clock agree exactly (a resumed run then continues from it).
    #[must_use]
    pub fn snapshot_terminal(
        &self,
        elapsed: Duration,
        progress: &[fast_down::ProgressEntry],
    ) -> DownloadStats {
        self.build(elapsed, progress, true)
    }

    fn build(
        &self,
        elapsed: Duration,
        progress: &[fast_down::ProgressEntry],
        terminal: bool,
    ) -> DownloadStats {
        let (pull, push) = if terminal {
            (
                self.pull.meter.terminal_stats(self.total, elapsed),
                self.push.terminal_stats(self.total, elapsed),
            )
        } else {
            (
                self.pull.meter.stats(self.total, elapsed),
                self.push.stats(self.total, elapsed),
            )
        };
        let workers = self
            .workers
            .iter()
            .map(|(id, w)| {
                (
                    *id,
                    WorkerStats {
                        state: w.state,
                        pulled: w.meter.bytes(),
                        bps: if terminal { 0 } else { w.meter.bps() },
                    },
                )
            })
            .collect();
        DownloadStats {
            progress: progress.to_vec(),
            total: self.total,
            elapsed,
            pull,
            push,
            workers,
        }
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
    #[allow(clippy::single_range_in_vec_init)]
    fn pull_and_push_are_tracked_separately() {
        let mut t = tracker(1000);
        let now = Instant::now();
        // Pull runs ahead: two ranges pulled, only one written.
        t.observe(&TestEvent::Pulling(0));
        t.observe(&TestEvent::PullProgress(0, 0..300));
        t.observe(&TestEvent::PullProgress(0, 300..500));
        t.observe(&TestEvent::Pushing(0, 0..300));
        t.set_written(300);

        let s = t.snapshot(now, &[0u64..300]);
        assert_eq!(s.pull.downloaded, 500, "pull counts both fetched ranges");
        assert_eq!(s.push.downloaded, 300, "push counts only the written range");
        assert!(s.pull.downloaded > s.push.downloaded, "pull leads push");
        assert_eq!(
            s.progress,
            vec![0u64..300],
            "the snapshot carries the written ranges it was given"
        );
        assert!((s.push.percent - 30.0).abs() < f64::EPSILON);
    }

    #[test]
    fn overlapping_ranges_do_not_inflate_downloaded() {
        // A re-sent or overlapping pull must not raise coverage past the bytes
        // actually covered: it is derived from the merged ranges.
        let mut t = tracker(1000);
        let now = Instant::now();
        t.observe(&TestEvent::PullProgress(0, 0..300));
        t.observe(&TestEvent::PullProgress(0, 0..300)); // exact duplicate
        t.observe(&TestEvent::PullProgress(0, 200..500)); // partial overlap
        let s = t.snapshot(now, &[]);
        assert_eq!(s.pull.downloaded, 500, "coverage is [0..500), counted once");
        assert!(
            (s.pull.percent - 50.0).abs() < f64::EPSILON,
            "no >100% inflation"
        );
    }

    #[test]
    fn per_worker_stats_are_attributed_by_id() {
        let mut t = tracker(1000);
        t.observe(&TestEvent::Pulling(1));
        t.observe(&TestEvent::Pulling(2));
        t.observe(&TestEvent::PullProgress(1, 0..100));
        t.observe(&TestEvent::PullProgress(2, 100..250));
        t.observe(&TestEvent::Finished(1));

        let s = t.snapshot(Instant::now(), &[]);
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
        t.observe(&TestEvent::Pushing(3, 0..10));
        assert_eq!(t.snapshot(now, &[]).workers[&3].state, WorkerState::Pushing);
        t.observe(&TestEvent::PullTimeout(3));
        assert_eq!(
            t.snapshot(now, &[]).workers[&3].state,
            WorkerState::TimedOut
        );
    }

    #[test]
    fn snapshots_sample_the_rate_so_a_stall_decays() {
        let mut t = StatsTracker::new(1000, Duration::ZERO, &[]);
        let t0 = Instant::now();
        t.observe(&TestEvent::PullProgress(0, 0..500));
        t.set_written(500);
        assert_eq!(
            t.snapshot(t0, &[]).push.bps,
            0,
            "first sample has no interval"
        );

        t.observe(&TestEvent::PullProgress(0, 500..1000));
        t.set_written(1000);
        let live = t.snapshot(t0 + Duration::from_secs(1), &[]);
        assert!(live.pull.bps > 0 && live.push.bps > 0, "EMA warmed up");

        // No new bytes: the next sample must decay the rate, not repeat it.
        let stalled = t.snapshot(t0 + Duration::from_secs(4), &[]);
        assert!(
            stalled.push.bps < live.push.bps,
            "a stall must decay the rate"
        );
    }

    #[test]
    fn terminal_snapshot_zeroes_rates() {
        // A non-zero prior elapsed makes `avg_bps` (and thus the ETA) measurable.
        let mut t = StatsTracker::new(1000, Duration::from_secs(1), &[]);
        let t0 = Instant::now();
        t.observe(&TestEvent::PullProgress(0, 0..500));
        t.set_written(500);
        let _ = t.snapshot(t0, &[]);
        t.observe(&TestEvent::PullProgress(0, 500..1000));
        t.set_written(1000);
        let live = t.snapshot(t0 + Duration::from_secs(1), &[]);
        assert!(live.pull.bps > 0 || live.push.bps > 0, "EMA warmed up");

        let terminal = t.snapshot_terminal(Duration::from_secs(2), &[]);
        assert_eq!(terminal.pull.bps, 0);
        assert_eq!(terminal.push.bps, 0);
        assert_eq!(terminal.push.downloaded, 1000);
        assert!((terminal.push.percent - 100.0).abs() < f64::EPSILON);
        assert_eq!(terminal.push.eta, Some(Duration::ZERO));
        assert!(terminal.workers.values().all(|w| w.bps == 0));
    }

    #[test]
    fn zero_total_reports_zero_percent() {
        let mut t = tracker(0);
        let s = t.snapshot(Instant::now(), &[]);
        assert!(s.push.percent.abs() < f64::EPSILON);
        assert!(s.pull.percent.abs() < f64::EPSILON);
        assert!(s.push.eta.is_none());
    }
}
