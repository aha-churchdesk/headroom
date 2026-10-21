//! Memory, swap and CPU over the last day, for the popover's graphs.
//!
//! Readings arrive every second; they are kept as one point per `STEP_S`
//! seconds, so a day is a few thousand points. A point keeps the most memory
//! and swap in use during its step, the mean CPU, and the worst pressure: a
//! spike that lasted two seconds still shows on a day-long graph.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::model::{Bytes, Pressure, Snapshot};

pub const TREND_S: u64 = 86_400;
pub const STEP_S: u64 = 30;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Point {
    /// Unix seconds at the start of its step.
    pub at: u64,
    pub mem_used: Bytes,
    pub mem_total: Bytes,
    pub swap_used: Bytes,
    pub cpu_pct: f64,
    pub pressure: Pressure,
    /// Readings merged into it, for the mean CPU.
    pub n: u32,
}

impl Point {
    fn merge(&mut self, p: &Point) {
        let n = self.n + p.n;
        self.cpu_pct = (self.cpu_pct * self.n as f64 + p.cpu_pct * p.n as f64) / n.max(1) as f64;
        self.n = n;
        self.mem_used = self.mem_used.max(p.mem_used);
        self.mem_total = self.mem_total.max(p.mem_total);
        self.swap_used = self.swap_used.max(p.swap_used);
        self.pressure = self.pressure.max(p.pressure);
    }
}

/// Oldest first.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Trend(VecDeque<Point>);

impl Trend {
    pub fn record(&mut self, s: &Snapshot, wall: u64) {
        let p = Point {
            at: wall / STEP_S * STEP_S,
            mem_used: s.mem_used,
            mem_total: s.mem_total,
            swap_used: s.swap_used,
            cpu_pct: s.cpu_pct,
            pressure: s.pressure,
            n: 1,
        };
        match self.0.back_mut() {
            // The same step, or the clock went back: part of the last point.
            Some(last) if p.at <= last.at => last.merge(&p),
            _ => self.0.push_back(p),
        }
        let first = wall.saturating_sub(TREND_S);
        while self.0.front().is_some_and(|p| p.at < first) {
            self.0.pop_front();
        }
    }

    /// The points of the last `secs` before `wall`, merged into at most
    /// `max` evenly spaced steps. Returns the step width with them. Steps
    /// with no readings (the app wasn't running) are left out, so a graph
    /// can show the gap.
    pub fn window(&self, wall: u64, secs: u64, max: usize) -> (u64, Vec<Point>) {
        let width = secs.div_ceil(max.max(1) as u64).div_ceil(STEP_S).max(1) * STEP_S;
        let first = wall.saturating_sub(secs);
        let mut out: Vec<Point> = Vec::new();
        for p in self.0.iter().filter(|p| p.at >= first) {
            let at = p.at / width * width;
            match out.last_mut() {
                Some(last) if last.at == at => last.merge(p),
                _ => out.push(Point { at, ..p.clone() }),
            }
        }
        (width, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::GIB;

    fn snap(used: u64, swap: u64, cpu: f64, pressure: Pressure) -> Snapshot {
        Snapshot {
            pressure,
            mem_total: 36 * GIB,
            mem_used: used * GIB,
            swap_used: swap * GIB,
            cpu_pct: cpu,
            ..Snapshot::default()
        }
    }

    const T: u64 = 1_000_020;

    #[test]
    fn readings_in_one_step_keep_the_peak_and_the_mean_cpu() {
        let mut t = Trend::default();
        t.record(&snap(20, 1, 10.0, Pressure::Normal), T);
        t.record(&snap(30, 3, 50.0, Pressure::Warn), T + 1);
        t.record(&snap(25, 2, 30.0, Pressure::Normal), T + 2);
        let (width, pts) = t.window(T + 2, 3_600, 120);
        assert_eq!((width, pts.len()), (STEP_S, 1));
        let p = &pts[0];
        assert_eq!((p.mem_used, p.swap_used), (30 * GIB, 3 * GIB));
        assert_eq!(p.cpu_pct, 30.0);
        assert_eq!(p.pressure, Pressure::Warn, "the worst moment shows");
        assert_eq!(p.n, 3);
    }

    #[test]
    fn a_new_step_starts_a_new_point() {
        let mut t = Trend::default();
        let step = T / STEP_S * STEP_S;
        t.record(&snap(20, 0, 0.0, Pressure::Normal), step + STEP_S - 1);
        t.record(&snap(21, 0, 0.0, Pressure::Normal), step + STEP_S);
        let (_, pts) = t.window(step + STEP_S, 3_600, 120);
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[1].at - pts[0].at, STEP_S);
    }

    #[test]
    fn a_day_is_kept_and_older_readings_are_dropped() {
        let mut t = Trend::default();
        t.record(&snap(20, 0, 0.0, Pressure::Normal), T);
        t.record(&snap(21, 0, 0.0, Pressure::Normal), T + TREND_S - STEP_S);
        assert_eq!(t.window(T + TREND_S, TREND_S, 10_000).1.len(), 2);
        t.record(&snap(22, 0, 0.0, Pressure::Normal), T + TREND_S + STEP_S);
        let (_, pts) = t.window(T + TREND_S + STEP_S, TREND_S, 10_000);
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[0].mem_used, 21 * GIB);
    }

    #[test]
    fn a_clock_that_goes_back_adds_to_the_last_point() {
        let mut t = Trend::default();
        t.record(&snap(20, 0, 0.0, Pressure::Normal), T + 300);
        t.record(&snap(30, 0, 0.0, Pressure::Normal), T);
        let (_, pts) = t.window(T + 300, 3_600, 120);
        assert_eq!(pts.len(), 1);
        assert_eq!(pts[0].mem_used, 30 * GIB);
    }

    #[test]
    fn a_long_window_is_merged_into_at_most_max_points() {
        let mut t = Trend::default();
        let start = T / 3_600 * 3_600;
        for i in 0..(TREND_S / STEP_S) {
            t.record(
                &snap(10 + i % 7, 0, 10.0, Pressure::Normal),
                start + i * STEP_S,
            );
        }
        let end = start + TREND_S - STEP_S;
        let (width, pts) = t.window(end, TREND_S, 288);
        assert_eq!(width, 300);
        assert!(pts.len() <= 289, "{}", pts.len());
        assert!(
            pts.iter().all(|p| p.mem_used == 16 * GIB),
            "each keeps its step's peak"
        );
        // A window wider than what was kept, or an empty trend.
        assert_eq!(Trend::default().window(end, TREND_S, 288).1.len(), 0);
        assert_eq!(t.window(end, 3_600, 0).0, 3_600);
    }

    #[test]
    fn a_gap_while_the_app_was_off_stays_a_gap() {
        let mut t = Trend::default();
        t.record(&snap(20, 0, 0.0, Pressure::Normal), T);
        t.record(&snap(20, 0, 0.0, Pressure::Normal), T + 1_800);
        let (width, pts) = t.window(T + 1_800, 3_600, 120);
        assert_eq!(pts.len(), 2);
        assert!(pts[1].at - pts[0].at > 2 * width);
    }

    #[test]
    fn saved_and_loaded_it_is_the_same() {
        let mut t = Trend::default();
        t.record(&snap(20, 1, 12.5, Pressure::Critical), T);
        let back: Trend = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!(back, t);
    }
}
