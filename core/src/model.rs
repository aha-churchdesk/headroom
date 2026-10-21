//! The shapes every other module shares.

use serde::{Deserialize, Serialize};

use crate::profile::Scaling;
use crate::tweaks::Tunable;

pub type JobId = u64;
pub type Bytes = u64;
/// Milliseconds on a monotonic clock the caller owns. Core never reads a
/// clock itself, which is what lets tests script time.
pub type Millis = u64;

pub const GIB: Bytes = 1 << 30;

/// macOS memory pressure, as `kern.memorystatus_vm_pressure_level` reports it.
/// Ordered, so "at least warn" is a comparison.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pressure {
    #[default]
    Normal,
    Warn,
    Critical,
}

/// One reading of the machine.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub pressure: Pressure,
    pub mem_total: Bytes,
    pub mem_used: Bytes,
    pub mem_available: Bytes,
    pub swap_used: Bytes,
    pub load1: f64,
    /// Physical cores, performance and efficiency. Zero means unknown, which
    /// disables the CPU rule rather than blocking everything.
    pub cores: u32,
    pub cpu_pct: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Frozen,
}

/// A command running inside a job, merged over the processes running it:
/// turbo's `tsc --noEmit`, a git hook's `eslint`, vitest's workers.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Part {
    pub cmd: String,
    /// Most memory its processes held at once.
    pub peak: Bytes,
    /// Most of its processes running at once.
    pub procs: u32,
    /// Samples it was seen in, one a second.
    pub samples: u32,
}

/// Parts kept per job and per run, the largest.
pub const PARTS: usize = 10;

/// Add one sample of `now` to `parts`, keeping the `PARTS` largest.
pub fn merge_parts(parts: &mut Vec<Part>, now: &[Part]) {
    for p in now {
        match parts.iter_mut().find(|q| q.cmd == p.cmd) {
            Some(q) => {
                q.peak = q.peak.max(p.peak);
                q.procs = q.procs.max(p.procs);
                q.samples += p.samples;
            }
            None => parts.push(p.clone()),
        }
    }
    parts.sort_by(|a, b| b.peak.cmp(&a.peak).then_with(|| a.cmd.cmp(&b.cmd)));
    parts.truncate(PARTS);
}

/// Who froze a job decides who may thaw it: the guard never undoes a pause
/// the user asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrozenBy {
    Guard,
    User,
}

/// What a client asks to run. Built by `headroom run`, which has the cwd and
/// the git context; the daemon trusts it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct JobSpec {
    /// The command as the user or agent wrote it.
    pub full_cmd: String,
    /// Normalised command, e.g. `vitest run`. Shown in the UI.
    pub cmd: String,
    /// Profile key: repository, subdirectory and normalised command.
    pub key: String,
    /// Coarser key (repository and program) used while `key` has few runs.
    pub fallback_key: String,
    /// Worktree or directory name, e.g. `sc-49123`.
    pub label: String,
    pub cwd: String,
    pub session: Option<String>,
    pub agent: Option<String>,
    pub low: bool,
    pub background: bool,
    /// Set when the command can be told to use fewer workers.
    #[serde(default)]
    pub tune: Option<Tunable>,
    /// The asking process. One inside a running job's processes runs
    /// directly: it is part of that job, even when the tools between them
    /// dropped `HEADROOM_JOB` (turbo passes its tasks only the variables it
    /// is told to), and queued it would wait for memory its parent holds.
    #[serde(default)]
    pub pid: Option<i32>,
}

/// A job started with fewer workers than it would use untouched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tuned {
    pub workers: u32,
    pub default: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    #[serde(flatten)]
    pub spec: JobSpec,
    pub pinned: bool,
    pub state: JobState,
    pub frozen_by: Option<FrozenBy>,
    /// Bytes this job is expected to peak at, from its profile.
    pub estimate: Bytes,
    /// Clean runs behind the estimate; zero means the default was used.
    pub samples: u32,
    pub footprint: Bytes,
    pub peak: Bytes,
    pub cpu_pct: f64,
    pub procs: u32,
    pub enqueued_at: Millis,
    pub started_at: Option<Millis>,
    pub frozen_at: Option<Millis>,
    /// Ever frozen during this attempt. Such a run's peak and duration are
    /// not representative, so the profile flags it.
    pub was_frozen: bool,
    pub swap_at_start: Bytes,
    /// Guard kills survived so far; a job is requeued at most once.
    pub retry: u8,
    /// Why the guard froze it, while it is frozen.
    #[serde(default)]
    pub guard_note: Option<String>,
    /// The user resumed it from a guard freeze. The guard leaves it alone
    /// until memory is back to normal, instead of freezing it again.
    #[serde(default)]
    pub spared: bool,
    pub pgid: Option<i32>,
    /// How its peak grows with workers, while it may be tuned.
    #[serde(default)]
    pub scaling: Option<Scaling>,
    /// The worker count it was started with, if admission lowered it.
    #[serde(default)]
    pub tuned: Option<Tuned>,
    /// Typical run time from its profile; what backfilling plans with.
    #[serde(default)]
    pub median_ms: Option<u64>,
    /// Times a later job started ahead of it while it waited on memory,
    /// and when that first happened.
    #[serde(default)]
    pub skipped: u32,
    #[serde(default)]
    pub skipped_since: Option<Millis>,
    /// The job it started ahead of, as "label · cmd".
    #[serde(default)]
    pub ahead_of: Option<String>,
    /// The largest process running inside it, when that isn't the command
    /// itself, as a short command line.
    #[serde(default)]
    pub now: Option<String>,
    /// What has run inside it so far.
    #[serde(default)]
    pub parts: Vec<Part>,
}

impl Job {
    pub fn queued(id: JobId, spec: JobSpec, estimate: Bytes, samples: u32, now: Millis) -> Self {
        Self {
            id,
            spec,
            pinned: false,
            state: JobState::Queued,
            frozen_by: None,
            estimate,
            samples,
            footprint: 0,
            peak: 0,
            cpu_pct: 0.0,
            procs: 0,
            enqueued_at: now,
            started_at: None,
            frozen_at: None,
            was_frozen: false,
            swap_at_start: 0,
            retry: 0,
            guard_note: None,
            spared: false,
            pgid: None,
            scaling: None,
            tuned: None,
            median_ms: None,
            skipped: 0,
            skipped_since: None,
            ahead_of: None,
            now: None,
            parts: Vec::new(),
        }
    }

    pub fn low(&self) -> bool {
        self.spec.low
    }

    /// Footprint above which this job counts as a runaway. A job with no
    /// history has no estimate worth doubling, so only the cap applies.
    pub fn runaway_at(&self, factor: f64, cap: Bytes) -> Bytes {
        if self.samples == 0 {
            cap
        } else {
            cap.max((self.estimate as f64 * factor) as Bytes)
        }
    }
}

/// How a run ended, for the History page and the agent's message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Exited,
    /// The guard killed it and put it back in the queue.
    Requeued,
    /// The guard killed it a second time; the agent was told.
    GaveUp,
    /// Killed from the menu bar or `headroom kill`.
    ManualKill,
    /// Removed from the queue before it ran.
    Cancelled,
    /// The client went away (agent timeout, Ctrl-C, closed terminal).
    Disconnected,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: JobId,
    pub label: String,
    pub cmd: String,
    pub key: String,
    pub duration_ms: u64,
    /// Whether the wrapper reported a started child. Older history omitted
    /// this, so duration and outcome remain the fallback for those entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<bool>,
    pub peak: Bytes,
    pub estimate: Bytes,
    pub exit: Option<i32>,
    /// Unix seconds.
    pub ended_at: u64,
    pub outcome: Outcome,
    pub why: Option<String>,
    #[serde(default)]
    pub tuned: Option<Tuned>,
    #[serde(default)]
    pub ahead_of: Option<String>,
    /// Time in the queue before it started, or before it was cancelled or
    /// its client left.
    #[serde(default)]
    pub waited_ms: u64,
}

impl HistoryEntry {
    pub fn has_run(&self) -> bool {
        self.started.unwrap_or(match self.outcome {
            Outcome::Cancelled => false,
            Outcome::Disconnected => self.duration_ms > 0,
            _ => true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_start_metadata_round_trips_and_old_history_still_loads() {
        let old = serde_json::json!({
            "id": 1, "label": "r", "cmd": "test", "key": "/r|test",
            "duration_ms": 0, "peak": 0, "estimate": 0, "exit": null,
            "ended_at": 100, "outcome": "disconnected", "why": null
        });
        let mut h: HistoryEntry = serde_json::from_value(old).unwrap();
        assert_eq!(h.started, None);
        assert!(!h.has_run());
        h.duration_ms = 1;
        assert!(h.has_run());
        h.duration_ms = 0;
        h.outcome = Outcome::Exited;
        assert!(h.has_run(), "legacy instant exits still count");
        h.outcome = Outcome::Disconnected;
        for started in [false, true] {
            h.started = Some(started);
            let loaded: HistoryEntry =
                serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
            assert_eq!(loaded, h);
            assert_eq!(loaded.has_run(), started);
        }
    }

    fn part(cmd: &str, gb: u64, procs: u32) -> Part {
        Part {
            cmd: cmd.into(),
            peak: gb * GIB,
            procs,
            samples: 1,
        }
    }

    #[test]
    fn samples_of_a_part_keep_its_peak_and_add_up_its_time() {
        let mut parts = Vec::new();
        merge_parts(
            &mut parts,
            &[part("tsc --noEmit", 2, 1), part("forks.js", 1, 8)],
        );
        merge_parts(
            &mut parts,
            &[part("tsc --noEmit", 3, 1), part("forks.js", 1, 6)],
        );
        merge_parts(&mut parts, &[part("tsc --noEmit", 1, 1)]);
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[0],
            Part {
                cmd: "tsc --noEmit".into(),
                peak: 3 * GIB,
                procs: 1,
                samples: 3
            }
        );
        assert_eq!(
            (parts[1].procs, parts[1].samples),
            (8, 2),
            "most processes at once"
        );
        merge_parts(&mut parts, &[]);
        assert_eq!(
            parts.len(),
            2,
            "a sample with nothing inside changes nothing"
        );
    }

    #[test]
    fn only_the_largest_parts_are_kept() {
        let mut parts = Vec::new();
        let many: Vec<Part> = (0..PARTS as u64 + 3)
            .map(|i| part(&format!("c{i}"), i + 1, 1))
            .collect();
        merge_parts(&mut parts, &many);
        assert_eq!(parts.len(), PARTS);
        assert_eq!(parts[0].cmd, format!("c{}", PARTS + 2));
        assert!(!parts.iter().any(|p| p.cmd == "c0"), "the smallest went");
    }
}
