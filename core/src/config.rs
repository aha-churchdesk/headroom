//! User-editable settings, persisted as `config.json`.
//!
//! Every field has a default, and missing fields fall back to it, so a config
//! written by an older version keeps loading.

use serde::{Deserialize, Serialize};

use crate::model::{Bytes, GIB};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KillMode {
    /// Never pause or kill; jobs just stay queued under pressure.
    Off,
    /// Freeze under critical pressure, never kill.
    Freeze,
    /// Freeze first; if pressure stays critical, kill and requeue once.
    #[default]
    FreezeKill,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Memory kept free before a new job may start.
    pub headroom: Bytes,
    pub kill_mode: KillMode,
    /// How long a foreground command waits in the queue before it is told
    /// to retry. Background commands wait as long as needed.
    pub wait_cap_s: u64,
    /// Assumed peak for a command with no history yet.
    pub default_estimate: Bytes,
    /// A job past `runaway_factor × estimate` (and past `runaway_cap`)
    /// under memory pressure is the guard's first victim.
    pub runaway_factor: f64,
    pub runaway_cap: Bytes,
    pub freeze_after_s: u64,
    pub thaw_after_s: u64,
    pub kill_after_s: u64,
    /// Seconds between SIGTERM and SIGKILL.
    pub kill_grace_s: u64,
    /// Swap growth since a job started that gets it killed outright.
    pub swap_kill_growth: Bytes,
    /// The queue holds while `load1 ≥ perf_cores × cpu_factor`.
    pub cpu_factor: f64,
    /// Commands whose normalised form matches one of these are queued.
    /// `*` matches any program: `* test` covers `pnpm test` and `cargo test`.
    pub patterns: Vec<String>,
    /// Long-lived commands, never queued: they would hold a slot forever.
    /// A `--flag` entry matches anywhere in the command; a word matches a
    /// subcommand or script name such as `dev` or `dev:web`.
    pub excluded: Vec<String>,
    pub start_at_login: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            headroom: 2 * GIB,
            kill_mode: KillMode::FreezeKill,
            wait_cap_s: 480,
            default_estimate: 3 * GIB / 2,
            runaway_factor: 2.0,
            runaway_cap: 8 * GIB,
            freeze_after_s: 10,
            thaw_after_s: 15,
            kill_after_s: 30,
            kill_grace_s: 10,
            swap_kill_growth: 4 * GIB,
            cpu_factor: 1.5,
            patterns: [
                "* test",
                "* build",
                "* e2e",
                "* typecheck",
                "vitest",
                "jest",
                "playwright",
                "tsc",
                "nx",
                "turbo",
                "next build",
                "xcodebuild",
                "cargo clippy",
                "gradle",
                "pytest",
            ]
            .map(String::from)
            .to_vec(),
            excluded: [
                "dev",
                "serve",
                "start",
                "watch",
                "preview",
                "storybook",
                "--watch",
                "--watchAll",
                "--ui",
                "docker compose up",
            ]
            .map(String::from)
            .to_vec(),
            start_at_login: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let c: Config = serde_json::from_str(r#"{"headroom": 1073741824}"#).unwrap();
        assert_eq!(c.headroom, GIB);
        assert_eq!(c.kill_mode, KillMode::FreezeKill);
        assert_eq!(c.patterns, Config::default().patterns);
    }

    #[test]
    fn empty_object_is_the_default_config() {
        let c: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn unknown_kill_mode_is_rejected_rather_than_guessed() {
        assert!(serde_json::from_str::<Config>(r#"{"kill_mode": "nuke"}"#).is_err());
    }
}
