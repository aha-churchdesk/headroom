//! Where Headroom keeps its files, and reading and writing them.
//!
//! `~/Library/Application Support/dev.headroom/`, or `$HEADROOM_HOME` when
//! set, which is how the tests (and two side-by-side checkouts) keep their
//! state apart. Writes go through a temp file and a rename: the daemon runs
//! all day, and a crash mid-write must not corrupt the profiles it has learned.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::config::Config;
use crate::model::HistoryEntry;
use crate::profile::Profiles;

pub const HOME_ENV: &str = "HEADROOM_HOME";
/// `sun_path` is 104 bytes on macOS, including the terminating NUL.
const SOCKET_MAX: usize = 104;

unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// The default location, created if missing.
    pub fn open() -> io::Result<Self> {
        let root = match std::env::var_os(HOME_ENV) {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => directories::BaseDirs::new()
                .ok_or_else(|| io::Error::other("no home directory"))?
                .data_dir()
                .join("dev.headroom"),
        };
        Self::at(root)
    }

    pub fn at(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The daemon's socket. A Unix socket path must fit in 104 bytes; when
    /// the data directory is too deep for that (a long `$HEADROOM_HOME`),
    /// the socket moves to a short per-user, per-directory name in /tmp.
    pub fn socket(&self) -> PathBuf {
        let p = self.root.join("headroomd.sock");
        if p.as_os_str().len() < SOCKET_MAX {
            return p;
        }
        // FNV-1a: stable across runs and versions, unlike std's hasher.
        let hash = self
            .root
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .fold(0xcbf29ce484222325u64, |h, &b| {
                (h ^ b as u64).wrapping_mul(0x100000001b3)
            });
        let uid = unsafe { libc_getuid() };
        PathBuf::from(format!("/tmp/headroom-{uid}-{hash:016x}.sock"))
    }

    fn load<T: DeserializeOwned + Default>(&self, name: &str) -> T {
        // A file that is missing or unreadable starts fresh rather than
        // keeping the daemon down. The bad copy is kept for inspection.
        let path = self.root.join(name);
        match fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| {
                let _ = fs::rename(&path, path.with_extension("json.corrupt"));
                T::default()
            }),
            Err(_) => T::default(),
        }
    }

    fn save<T: Serialize>(&self, name: &str, value: &T) -> io::Result<()> {
        let path = self.root.join(name);
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
        fs::write(&tmp, body)?;
        fs::rename(&tmp, &path)
    }

    pub fn load_config(&self) -> Config {
        self.load("config.json")
    }

    pub fn save_config(&self, c: &Config) -> io::Result<()> {
        self.save("config.json", c)
    }

    pub fn load_profiles(&self) -> Profiles {
        self.load("profiles.json")
    }

    pub fn save_profiles(&self, p: &Profiles) -> io::Result<()> {
        self.save("profiles.json", p)
    }

    pub fn load_history(&self) -> VecDeque<HistoryEntry> {
        self.load("history.json")
    }

    pub fn save_history(&self, h: &VecDeque<HistoryEntry>) -> io::Result<()> {
        self.save("history.json", h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Run;

    fn tmp(name: &str) -> Store {
        let p = std::env::temp_dir().join(format!("headroom-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        Store::at(p).unwrap()
    }

    #[test]
    fn round_trips_every_file() {
        let s = tmp("rt");
        let c = Config {
            wait_cap_s: 7,
            ..Config::default()
        };
        s.save_config(&c).unwrap();
        assert_eq!(s.load_config(), c);
        let mut p = Profiles::default();
        p.record(
            "k",
            "f",
            Run {
                peak: 1,
                duration_ms: 2,
                exit: Some(0),
                flagged: false,
                at: 3,
                workers: None,
            },
        );
        s.save_profiles(&p).unwrap();
        assert_eq!(s.load_profiles(), p);
        assert!(!s.root().join("profiles.json.tmp").exists());
    }

    #[test]
    fn missing_files_load_as_defaults() {
        let s = tmp("missing");
        assert_eq!(s.load_config(), Config::default());
        assert!(s.load_history().is_empty());
    }

    #[test]
    fn a_corrupt_file_starts_fresh_and_is_kept_aside() {
        let s = tmp("corrupt");
        fs::write(s.root().join("profiles.json"), "{ truncated").unwrap();
        assert_eq!(s.load_profiles(), Profiles::default());
        assert_eq!(
            fs::read_to_string(s.root().join("profiles.json.corrupt")).unwrap(),
            "{ truncated"
        );
    }

    #[test]
    fn socket_lives_in_the_root() {
        let s = tmp("sock");
        assert_eq!(s.socket(), s.root().join("headroomd.sock"));
    }

    #[test]
    fn a_deep_root_gets_a_short_stable_socket_path() {
        let deep = std::env::temp_dir().join("x".repeat(90));
        let a = Store::at(&deep).unwrap().socket();
        assert!(a.as_os_str().len() < SOCKET_MAX, "{}", a.display());
        assert!(a.starts_with("/tmp"));
        assert_eq!(
            a,
            Store::at(&deep).unwrap().socket(),
            "same root, same socket"
        );
        let other = Store::at(std::env::temp_dir().join("y".repeat(90)))
            .unwrap()
            .socket();
        assert_ne!(a, other, "different roots don't collide");
        let _ = fs::remove_dir_all(&deep);
    }
}
