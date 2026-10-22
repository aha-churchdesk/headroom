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
    /// The queue holds while `load1 ≥ cores × cpu_factor`.
    pub cpu_factor: f64,
    /// Commands whose normalised form matches one of these are queued.
    /// `*` matches any program: `* test` covers `pnpm test` and `cargo test`.
    pub patterns: Vec<String>,
    /// The default patterns this config has been offered. A default added
    /// in a later version joins `patterns` once; one the user removed
    /// stays removed. Configs saved before this field had the first
    /// version's defaults.
    #[serde(default = "first_defaults")]
    pub patterns_seen: Vec<String>,
    /// Long-lived commands, never queued: they would hold a slot forever.
    /// A `--flag` entry matches anywhere in the command; a word matches a
    /// subcommand or script name such as `dev` or `dev:web`.
    pub excluded: Vec<String>,
    pub start_at_login: bool,
    /// Let fitting jobs pass a head waiting on memory within the skip/time
    /// limits. When runtimes are known, they must fit its predicted wait.
    pub backfill: bool,
    /// After this many jobs started ahead of it, or this long since the
    /// first one did, the head is waited for strictly.
    pub backfill_skips: u32,
    pub backfill_wait_s: u64,
}

/// The default heavy patterns.
pub const DEFAULT_PATTERNS: &[&str] = &[
    "* test",
    "* build",
    "* e2e",
    "* typecheck",
    "vitest",
    "jest",
    "mocha",
    "playwright",
    "tsc",
    "eslint",
    "nx",
    "turbo",
    "next build",
    "xcodebuild",
    "cargo clippy",
    "gradle",
    "pytest",
];

/// The defaults before `patterns_seen` existed.
fn first_defaults() -> Vec<String> {
    DEFAULT_PATTERNS
        .iter()
        .filter(|p| !["mocha", "eslint"].contains(p))
        .map(|p| p.to_string())
        .collect()
}

impl Config {
    /// Add the default patterns this config hasn't been offered yet.
    /// Returns whether anything changed.
    pub fn adopt_new_defaults(&mut self) -> bool {
        let mut changed = false;
        for p in DEFAULT_PATTERNS {
            if !self.patterns_seen.iter().any(|s| s == p) {
                if !self.patterns.iter().any(|s| s == p) {
                    self.patterns.push(p.to_string());
                }
                changed = true;
            }
        }
        if changed {
            // The defaults in their order, then any older ones since dropped.
            let older: Vec<String> = self
                .patterns_seen
                .drain(..)
                .filter(|s| !DEFAULT_PATTERNS.contains(&s.as_str()))
                .collect();
            self.patterns_seen = DEFAULT_PATTERNS.iter().map(|p| p.to_string()).collect();
            self.patterns_seen.extend(older);
        }
        changed
    }
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
            patterns: DEFAULT_PATTERNS.iter().map(|p| p.to_string()).collect(),
            patterns_seen: DEFAULT_PATTERNS.iter().map(|p| p.to_string()).collect(),
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
            backfill: true,
            backfill_skips: 3,
            backfill_wait_s: 300,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_from_before_gets_the_new_defaults_once() {
        // Saved by an earlier version: the first defaults, minus one the
        // user removed, plus one they added.
        let mut old = first_defaults();
        old.retain(|p| p != "gradle");
        old.push("yarn".into());
        let mut c: Config = serde_json::from_value(serde_json::json!({ "patterns": old })).unwrap();
        assert!(c.adopt_new_defaults());
        assert!(c.patterns.contains(&"mocha".into()) && c.patterns.contains(&"eslint".into()));
        assert!(
            !c.patterns.contains(&"gradle".into()),
            "a removed default stays removed"
        );
        assert!(c.patterns.contains(&"yarn".into()), "the user's own stays");
        // Removed after being offered: not brought back.
        c.patterns.retain(|p| p != "eslint");
        assert!(!c.adopt_new_defaults());
        assert!(!c.patterns.contains(&"eslint".into()));
        // Through a save and a load, nothing is offered twice.
        let mut again: Config = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert!(!again.adopt_new_defaults());
        assert_eq!(again.patterns, c.patterns);
    }

    #[test]
    fn a_fresh_config_has_nothing_new_to_adopt() {
        let mut c = Config::default();
        assert!(!c.adopt_new_defaults());
        // A new default that's already there by hand isn't added twice.
        let mut c: Config =
            serde_json::from_value(serde_json::json!({ "patterns": ["eslint"] })).unwrap();
        c.adopt_new_defaults();
        assert_eq!(c.patterns.iter().filter(|p| *p == "eslint").count(), 1);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let c: Config = serde_json::from_str(r#"{"headroom": 1073741824}"#).unwrap();
        assert_eq!(c.headroom, GIB);
        assert_eq!(c.kill_mode, KillMode::FreezeKill);
        assert_eq!(c.patterns, Config::default().patterns);
    }

    #[test]
    fn legacy_worker_tuning_settings_are_ignored_and_not_saved_again() {
        let old = serde_json::json!({
            "headroom": GIB,
            "tune": true,
            "tune_floor": 1,
            "tune_off": ["vitest"]
        });
        let c: Config = serde_json::from_value(old).unwrap();
        assert_eq!(c.headroom, GIB);
        assert!(c.backfill);
        let saved = serde_json::to_value(c).unwrap();
        for removed in ["tune", "tune_floor", "tune_off"] {
            assert!(saved.get(removed).is_none());
        }
    }

    #[test]
    fn empty_object_is_the_default_config_once_new_defaults_are_adopted() {
        let mut c: Config = serde_json::from_str("{}").unwrap();
        c.adopt_new_defaults();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn unknown_kill_mode_is_rejected_rather_than_guessed() {
        assert!(serde_json::from_str::<Config>(r#"{"kill_mode": "nuke"}"#).is_err());
    }
}
