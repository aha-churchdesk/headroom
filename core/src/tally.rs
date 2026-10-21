//! What each command has cost, per day, for the Commands page.
//!
//! The history keeps about a day of runs, and a profile only its last 20.
//! This keeps totals per profile key and day for `TALLY_DAYS`, so "what ran
//! most this month" doesn't need every run kept. Days are UTC days.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::model::{Bytes, HistoryEntry, Outcome};

pub const TALLY_DAYS: u64 = 90;
const DAY_S: u64 = 86_400;
const MIB: u64 = 1 << 20;

/// One command's totals for one day.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Day {
    /// Times it ran to an end. A run the guard killed and requeued counts
    /// once, when its retry ends.
    pub runs: u32,
    /// Exited with an error, or killed twice to free memory.
    pub failed: u32,
    /// Time running, including runs the guard killed.
    pub run_ms: u64,
    /// Time queued before starting, including jobs cancelled while queued.
    pub wait_ms: u64,
    /// Peak memory times run time, in MiB-seconds: the memory each run kept
    /// from other jobs, for as long as it ran.
    pub mib_s: u64,
    pub peak: Bytes,
}

impl Day {
    pub fn add(&mut self, d: &Day) {
        self.runs += d.runs;
        self.failed += d.failed;
        self.run_ms += d.run_ms;
        self.wait_ms += d.wait_ms;
        self.mib_s += d.mib_s;
        self.peak = self.peak.max(d.peak);
    }
}

/// Profile key → UTC day number → totals.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Tally(BTreeMap<String, BTreeMap<u64, Day>>);

impl Tally {
    /// Totals from the runs a history holds, for a store that predates the
    /// tally.
    pub fn from_history<'a>(history: impl IntoIterator<Item = &'a HistoryEntry>) -> Self {
        let mut t = Self::default();
        let mut newest = 0;
        for h in history {
            t.count(h);
            newest = newest.max(h.ended_at);
        }
        t.prune(newest);
        t
    }

    /// Count one history entry, and forget days that have fallen out.
    pub fn add(&mut self, h: &HistoryEntry) {
        self.count(h);
        self.prune(h.ended_at);
    }

    fn count(&mut self, h: &HistoryEntry) {
        let ran = h.has_run();
        let ended = ran && !matches!(h.outcome, Outcome::Requeued | Outcome::Cancelled);
        let failed = ran
            && match h.outcome {
                Outcome::Exited => h.exit != Some(0),
                Outcome::GaveUp => true,
                _ => false,
            };
        let run_ms = if ran { h.duration_ms } else { 0 };
        let mib_s = (h.peak as u128 * run_ms as u128 / (MIB as u128 * 1000)) as u64;
        let d = Day {
            runs: ended as u32,
            failed: failed as u32,
            run_ms,
            wait_ms: h.waited_ms,
            mib_s,
            peak: if ran { h.peak } else { 0 },
        };
        self.0
            .entry(h.key.clone())
            .or_default()
            .entry(h.ended_at / DAY_S)
            .or_default()
            .add(&d);
    }

    fn prune(&mut self, wall: u64) {
        let first = (wall / DAY_S).saturating_sub(TALLY_DAYS - 1);
        for days in self.0.values_mut() {
            days.retain(|&day, _| day >= first);
        }
        self.0.retain(|_, days| !days.is_empty());
    }

    /// Each command's totals over the `days` days up to and including the
    /// one `wall` falls in, most time running first. Commands with nothing
    /// in that window are left out.
    pub fn window(&self, wall: u64, days: u64) -> Vec<(&str, Day)> {
        let today = wall / DAY_S;
        let first = today.saturating_sub(days.saturating_sub(1));
        let mut rows: Vec<(&str, Day)> = self
            .0
            .iter()
            .filter_map(|(k, by_day)| {
                let mut sum = Day::default();
                for (_, d) in by_day.range(first..=today) {
                    sum.add(d);
                }
                (sum != Day::default()).then_some((k.as_str(), sum))
            })
            .collect();
        rows.sort_by(|a, b| b.1.run_ms.cmp(&a.1.run_ms).then(a.0.cmp(b.0)));
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::GIB;

    fn entry(key: &str, ended_at: u64, outcome: Outcome, exit: Option<i32>) -> HistoryEntry {
        HistoryEntry {
            id: 1,
            label: "repo".into(),
            cmd: "pnpm test".into(),
            key: key.into(),
            duration_ms: 60_000,
            started: None,
            peak: 2 * GIB,
            estimate: GIB,
            exit,
            ended_at,
            outcome,
            why: None,
            tuned: None,
            ahead_of: None,
            waited_ms: 5_000,
        }
    }

    const NOW: u64 = 1_000 * DAY_S + 3_600;

    #[test]
    fn sums_runs_time_and_memory_per_command() {
        let mut t = Tally::default();
        t.add(&entry("/r|test", NOW - 10, Outcome::Exited, Some(0)));
        t.add(&entry("/r|test", NOW - 5, Outcome::Exited, Some(1)));
        t.add(&entry("/r|build", NOW, Outcome::Exited, Some(0)));
        let rows = t.window(NOW, 7);
        assert_eq!(rows[0].0, "/r|test", "most time running first");
        let test = &rows[0].1;
        assert_eq!((test.runs, test.failed), (2, 1));
        assert_eq!((test.run_ms, test.wait_ms), (120_000, 10_000));
        // 2 GiB for a minute, twice.
        assert_eq!(test.mib_s, 2 * 2048 * 60);
        assert_eq!(test.peak, 2 * GIB);
        assert_eq!(rows[1].1.runs, 1);
    }

    #[test]
    fn a_requeued_run_costs_time_but_counts_once_when_its_retry_ends() {
        let mut t = Tally::default();
        t.add(&entry("/r|e2e", NOW - 100, Outcome::Requeued, None));
        t.add(&entry("/r|e2e", NOW, Outcome::GaveUp, None));
        let d = &t.window(NOW, 1)[0].1;
        assert_eq!((d.runs, d.failed, d.run_ms), (1, 1, 120_000));
    }

    #[test]
    fn a_job_that_never_started_adds_its_wait_and_nothing_else() {
        let mut t = Tally::default();
        let mut cancelled = entry("/r|test", NOW, Outcome::Cancelled, None);
        cancelled.duration_ms = 0;
        let mut left = entry("/r|test", NOW, Outcome::Disconnected, None);
        left.duration_ms = 0;
        t.add(&cancelled);
        t.add(&left);
        let d = &t.window(NOW, 1)[0].1;
        assert_eq!((d.runs, d.run_ms, d.mib_s, d.peak), (0, 0, 0, 0));
        assert_eq!(d.wait_ms, 10_000);
        // Its client left after it started: it ran.
        t.add(&entry("/r|test", NOW, Outcome::Disconnected, None));
        assert_eq!(t.window(NOW, 1)[0].1.runs, 1);
    }

    #[test]
    fn explicit_unstarted_history_ignores_run_time_and_memory() {
        let mut h = entry("/r|test", NOW, Outcome::Disconnected, None);
        h.started = Some(false);
        let t = Tally::from_history([&h]);
        let d = &t.window(NOW, 1)[0].1;
        assert_eq!((d.runs, d.run_ms, d.mib_s, d.peak), (0, 0, 0, 0));
        assert_eq!(d.wait_ms, h.waited_ms);
    }

    #[test]
    fn a_window_covers_whole_days_up_to_today() {
        let mut t = Tally::default();
        let today = NOW / DAY_S * DAY_S;
        t.add(&entry(
            "/r|test",
            today - 6 * DAY_S,
            Outcome::Exited,
            Some(0),
        ));
        t.add(&entry(
            "/r|test",
            today - 7 * DAY_S + DAY_S - 1,
            Outcome::Exited,
            Some(0),
        ));
        t.add(&entry("/r|test", today, Outcome::Exited, Some(0)));
        assert_eq!(t.window(NOW, 7)[0].1.runs, 2, "the 7th day back is in");
        assert_eq!(t.window(NOW, 8)[0].1.runs, 3, "the day before it is not");
        assert_eq!(t.window(NOW, 1)[0].1.runs, 1);
        assert!(Tally::default().window(NOW, 7).is_empty());
        // Nothing in the window: the command isn't listed at all.
        assert!(t.window(NOW + 30 * DAY_S, 7).is_empty());
    }

    #[test]
    fn days_past_the_limit_are_forgotten() {
        let mut t = Tally::default();
        t.add(&entry(
            "/r|old",
            NOW - TALLY_DAYS * DAY_S,
            Outcome::Exited,
            Some(0),
        ));
        t.add(&entry(
            "/r|kept",
            NOW - (TALLY_DAYS - 1) * DAY_S,
            Outcome::Exited,
            Some(0),
        ));
        t.add(&entry("/r|test", NOW, Outcome::Exited, Some(0)));
        let keys: Vec<&str> = t.window(NOW, TALLY_DAYS + 10).iter().map(|r| r.0).collect();
        assert_eq!(keys.len(), 2);
        assert!(!keys.contains(&"/r|old"));
    }

    #[test]
    fn rebuilt_from_a_history_it_matches_counting_as_they_happened() {
        let hist = [
            entry("/r|test", NOW - 20, Outcome::Exited, Some(0)),
            entry("/r|e2e", NOW - 10, Outcome::Requeued, None),
            entry("/r|e2e", NOW, Outcome::Exited, Some(0)),
        ];
        let mut live = Tally::default();
        for h in &hist {
            live.add(h);
        }
        assert_eq!(Tally::from_history(&hist), live);
        assert_eq!(Tally::from_history(&[]), Tally::default());
        // Saved and loaded, nothing changes.
        let back: Tally = serde_json::from_str(&serde_json::to_string(&live).unwrap()).unwrap();
        assert_eq!(back, live);
    }
}
