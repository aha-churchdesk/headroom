//! When to warn that the disk is filling up.
//!
//! One notice per threshold crossed on the way up. A disk that hovers around
//! a threshold would otherwise warn on every reading, so a threshold is only
//! re-armed once usage falls `REARM_PCT` points below it.

use crate::model::Bytes;

/// Percent used at which to warn, lowest first.
pub const THRESHOLDS: [u8; 4] = [70, 80, 90, 95];

/// How far usage must fall below a threshold before it can warn again.
pub const REARM_PCT: f64 = 2.0;

/// Percent of the disk in use.
pub fn used_pct(total: Bytes, free: Bytes) -> f64 {
    if total == 0 {
        return 0.0;
    }
    total.saturating_sub(free) as f64 / total as f64 * 100.0
}

/// The highest threshold at or below `pct`.
fn level(pct: f64) -> Option<u8> {
    THRESHOLDS
        .iter()
        .rev()
        .copied()
        .find(|&t| pct >= f64::from(t))
}

#[derive(Debug, Default)]
pub struct Alarm {
    /// The highest threshold warned about and not yet re-armed.
    warned: Option<u8>,
}

impl Alarm {
    /// Record a reading. Returns the threshold to warn about, if this
    /// reading crossed one not already warned about. A jump past several
    /// warns once, for the highest.
    pub fn check(&mut self, pct: f64) -> Option<u8> {
        let now = level(pct);
        if now > self.warned {
            self.warned = now;
            return now;
        }
        // Thresholds more than REARM_PCT above the reading can warn again.
        self.warned = self.warned.min(level(pct + REARM_PCT));
        None
    }
}

/// The notice for crossing `threshold`, as (title, body).
pub fn notice(threshold: u8, total: Bytes, free: Bytes) -> (String, String) {
    let gb = |b: Bytes| b as f64 / 1e9;
    (
        format!("Disk is {threshold}% full"),
        format!(
            "{:.1} GB free of {:.0} GB. Removing merged worktrees in Headroom can free space.",
            gb(free),
            gb(total)
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warns_once_per_threshold_on_the_way_up() {
        let mut a = Alarm::default();
        let warned: Vec<Option<u8>> = [50.0, 69.9, 70.0, 75.0, 80.0, 85.0, 90.0, 95.0, 99.0]
            .iter()
            .map(|&p| a.check(p))
            .collect();
        assert_eq!(
            warned,
            [
                None,
                None,
                Some(70),
                None,
                Some(80),
                None,
                Some(90),
                Some(95),
                None
            ]
        );
    }

    #[test]
    fn a_jump_past_several_thresholds_warns_once_for_the_highest() {
        let mut a = Alarm::default();
        assert_eq!(a.check(60.0), None);
        assert_eq!(a.check(91.0), Some(90));
        assert_eq!(a.check(92.0), None, "70 and 80 are not warned late");
    }

    #[test]
    fn already_full_at_start_warns_once() {
        let mut a = Alarm::default();
        assert_eq!(a.check(85.0), Some(80));
        assert_eq!(a.check(85.0), None);
    }

    #[test]
    fn hovering_around_a_threshold_does_not_warn_again() {
        let mut a = Alarm::default();
        assert_eq!(a.check(90.0), Some(90));
        for p in [89.5, 90.2, 88.1, 90.0, 89.0] {
            assert_eq!(a.check(p), None, "{p}");
        }
    }

    #[test]
    fn falling_well_below_a_threshold_rearms_it() {
        let mut a = Alarm::default();
        assert_eq!(a.check(90.0), Some(90));
        assert_eq!(a.check(88.0), None, "exactly REARM_PCT below still counts");
        assert_eq!(a.check(90.0), None);
        assert_eq!(a.check(87.9), None, "now re-armed");
        assert_eq!(a.check(90.0), Some(90));
    }

    #[test]
    fn freeing_lots_of_space_rearms_every_threshold_above() {
        let mut a = Alarm::default();
        assert_eq!(a.check(96.0), Some(95));
        assert_eq!(a.check(50.0), None);
        assert_eq!(a.check(70.0), Some(70));
        assert_eq!(a.check(96.0), Some(95));
    }

    #[test]
    fn used_percent_handles_an_unreadable_or_odd_disk() {
        assert_eq!(used_pct(100, 25), 75.0);
        assert_eq!(used_pct(0, 0), 0.0, "no size read");
        assert_eq!(used_pct(100, 200), 0.0, "more free than total");
        assert_eq!(used_pct(100, 0), 100.0);
    }

    #[test]
    fn the_notice_names_the_threshold_and_free_space() {
        let (title, body) = notice(90, 494_000_000_000, 49_400_000_000);
        assert_eq!(title, "Disk is 90% full");
        assert!(body.starts_with("49.4 GB free of 494 GB."), "{body}");
    }
}
