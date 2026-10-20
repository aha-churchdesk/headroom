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
    /// Performance cores. Zero means unknown, which disables the CPU rule
    /// rather than blocking everything.
    pub perf_cores: u32,
    pub cpu_pct: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Frozen,
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
            pgid: None,
            scaling: None,
            tuned: None,
            median_ms: None,
            skipped: 0,
            skipped_since: None,
            ahead_of: None,
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
