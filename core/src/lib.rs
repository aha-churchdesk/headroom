//! Headroom's decisions, with no platform calls.
//!
//! Everything that decides something — whether a command is heavy, when a
//! queued job may start, which job the guard freezes, what an agent's hook
//! answers — lives here, so it can be tested against scripted time and
//! scripted memory. The daemon and the CLI only measure the machine and carry
//! out what this crate decides.

pub mod admission;
pub mod classify;
pub mod config;
pub mod guard;
pub mod hookio;
pub mod hooksettings;
pub mod model;
pub mod profile;
pub mod protocol;
pub mod scheduler;
pub mod shell;
pub mod store;
pub mod tally;
pub mod tweaks;
