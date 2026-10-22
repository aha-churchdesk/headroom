//! Where Headroom keeps its files, and reading and writing them.
//!
//! `~/Library/Application Support/dev.headroom/`, or `$HEADROOM_HOME` when
//! set, which is how the tests (and two side-by-side checkouts) keep their
//! state apart. Writes go through a temp file and a rename: the daemon runs
//! all day, and a crash mid-write must not corrupt the profiles it has learned.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::config::Config;
use crate::model::HistoryEntry;
use crate::profile::Profiles;
use crate::tally::Tally;

pub const HOME_ENV: &str = "HEADROOM_HOME";
/// `sun_path` is 104 bytes on macOS, including the terminating NUL.
const SOCKET_MAX: usize = 104;
static SAVE_ID: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" {
    fn confstr(name: i32, buf: *mut u8, len: usize) -> usize;
}

/// `_CS_DARWIN_USER_TEMP_DIR`: the per-user temporary directory under
/// /var/folders, mode 0700. The same for every process of the user, whatever
/// `$TMPDIR` says.
const CS_DARWIN_USER_TEMP_DIR: i32 = 65537;

fn user_temp_dir() -> Option<PathBuf> {
    let mut buf = vec![0u8; 1024];
    let n = unsafe { confstr(CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr(), buf.len()) };
    if n == 0 || n > buf.len() {
        return None;
    }
    buf.truncate(n - 1);
    String::from_utf8(buf).ok().map(PathBuf::from)
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
    /// the socket moves to a short per-directory name in the user's private
    /// temporary directory. Not /tmp: anyone can create a name there first,
    /// and a socket someone else made would receive our commands.
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
        let name = format!("headroom-{hash:016x}.sock");
        // Without a private directory, the long path: binding it fails with
        // an error that names it, rather than falling back somewhere shared.
        user_temp_dir().map_or(p, |d| d.join(name))
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
        let body = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
        self.save_bytes(name, &body)
    }

    fn save_bytes(&self, name: &str, body: &[u8]) -> io::Result<()> {
        let path = self.root.join(name);
        // Hooks can save newly adopted defaults at the same time as the
        // daemon. Each writer must own its temporary file until rename.
        let (tmp, mut file) = loop {
            let id = SAVE_ID.fetch_add(1, Ordering::Relaxed);
            let tmp = self
                .root
                .join(format!(".{name}.{}.{id}.tmp", std::process::id()));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
            {
                Ok(file) => break (tmp, file),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        };
        let result = (|| {
            file.write_all(body)?;
            drop(file);
            fs::rename(&tmp, &path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    /// The config, with any new default patterns adopted and saved.
    pub fn load_config(&self) -> Config {
        let mut c: Config = self.load("config.json");
        if c.adopt_new_defaults() && self.root.join("config.json").exists() {
            let _ = self.save_config(&c);
        }
        c
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

    /// Per-command totals. A store from before they were kept starts from
    /// the runs its history holds.
    pub fn load_tally(&self) -> Tally {
        if self.root.join("stats.json").exists() {
            self.load("stats.json")
        } else {
            Tally::from_history(&self.load_history())
        }
    }

    pub fn save_tally(&self, t: &Tally) -> io::Result<()> {
        self.save("stats.json", t)
    }

    /// Clear only learned data, including corrupt copies kept by `load`.
    /// Preflight every path and retain original bytes so an ordinary I/O
    /// failure can roll back the entire operation before memory is cleared.
    pub fn clear_data(&self) -> io::Result<()> {
        let names = [
            "profiles.json",
            "history.json",
            "stats.json",
            "profiles.json.corrupt",
            "history.json.corrupt",
            "stats.json.corrupt",
            "trend.json",
            "trend.json.corrupt",
        ];
        let mut before = Vec::new();
        for name in names {
            let path = self.root.join(name);
            let bytes = match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_file() => Some(fs::read(&path)?),
                Ok(_) => {
                    return Err(io::Error::other(format!(
                        "cannot clear non-file {}",
                        path.display()
                    )));
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => return Err(e),
            };
            before.push((name, bytes));
        }
        let result = (|| {
            self.save_profiles(&Profiles::default())?;
            self.save_history(&VecDeque::new())?;
            self.save_tally(&Tally::default())?;
            for (name, bytes) in &before {
                if (name.ends_with(".corrupt") || *name == "trend.json") && bytes.is_some() {
                    fs::remove_file(self.root.join(name))?;
                }
            }
            for name in &names[..3] {
                fs::File::open(self.root.join(name))?.sync_all()?;
            }
            fs::File::open(&self.root)?.sync_all()
        })();
        if let Err(error) = result {
            let mut rollback_errors = Vec::new();
            for (name, bytes) in before {
                let restored = match bytes {
                    Some(bytes) => self.save_bytes(name, &bytes),
                    None => match fs::remove_file(self.root.join(name)) {
                        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                        _ => Ok(()),
                    },
                };
                if let Err(e) = restored {
                    rollback_errors.push(format!("{name}: {e}"));
                }
            }
            return Err(io::Error::other(if rollback_errors.is_empty() {
                format!("clear data failed; previous data restored: {error}")
            } else {
                format!(
                    "clear data failed: {error}; restoration incomplete: {}",
                    rollback_errors.join(", ")
                )
            }));
        }
        Ok(())
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
    fn clear_data_removes_learned_files_and_corrupt_copies_but_keeps_settings() {
        let s = tmp("clear-data");
        let config = Config {
            wait_cap_s: 123,
            ..Config::default()
        };
        s.save_config(&config).unwrap();
        for name in [
            "profiles.json",
            "history.json",
            "stats.json",
            "profiles.json.corrupt",
            "history.json.corrupt",
            "stats.json.corrupt",
            "trend.json",
            "trend.json.corrupt",
        ] {
            fs::write(s.root().join(name), "private old data").unwrap();
        }
        fs::write(s.root().join("unrelated.txt"), "keep").unwrap();
        s.clear_data().unwrap();
        assert_eq!(s.load_profiles(), Profiles::default());
        assert!(s.load_history().is_empty());
        assert_eq!(s.load_tally(), Tally::default());
        assert_eq!(s.load_config(), config);
        assert_eq!(
            fs::read_to_string(s.root().join("unrelated.txt")).unwrap(),
            "keep"
        );
        for name in [
            "profiles.json.corrupt",
            "history.json.corrupt",
            "stats.json.corrupt",
            "trend.json",
            "trend.json.corrupt",
        ] {
            assert!(!s.root().join(name).exists());
        }
        s.clear_data().unwrap();
    }

    #[test]
    fn clear_data_preflights_every_path_before_changing_any_file() {
        let s = tmp("clear-data-refused");
        fs::write(s.root().join("profiles.json"), "original").unwrap();
        fs::create_dir(s.root().join("stats.json")).unwrap();
        assert!(s.clear_data().is_err());
        assert_eq!(
            fs::read_to_string(s.root().join("profiles.json")).unwrap(),
            "original"
        );
        assert!(!s.root().join("history.json").exists());
        assert!(s.root().join("stats.json").is_dir());
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
                parts: Vec::new(),
            },
        );
        s.save_profiles(&p).unwrap();
        assert_eq!(s.load_profiles(), p);
        assert!(!s.root().join("profiles.json.tmp").exists());
    }

    #[test]
    fn the_tally_starts_from_the_history_until_it_is_saved() {
        let s = tmp("tally");
        assert_eq!(s.load_tally(), Tally::default());
        let run = HistoryEntry {
            id: 1,
            label: "r".into(),
            cmd: "pnpm test".into(),
            key: "/r|test".into(),
            duration_ms: 1_000,
            started: None,
            peak: 1,
            estimate: 1,
            exit: Some(0),
            ended_at: 100,
            outcome: crate::model::Outcome::Exited,
            why: None,
            tuned: None,
            ahead_of: None,
            waited_ms: 0,
        };
        s.save_history(&VecDeque::from([run.clone()])).unwrap();
        let from_history = s.load_tally();
        assert_eq!(from_history.window(100, 1)[0].1.runs, 1);
        // Once saved, the saved totals are what count, not the history.
        let mut t = from_history.clone();
        t.add(&run);
        s.save_tally(&t).unwrap();
        assert_eq!(s.load_tally(), t);
        // A corrupt file is kept aside, not rebuilt from a history that
        // holds only the last day.
        fs::write(s.root().join("stats.json"), "{ truncated").unwrap();
        assert_eq!(s.load_tally(), Tally::default());
        assert!(s.root().join("stats.json.corrupt").exists());
    }

    #[test]
    fn missing_files_load_as_defaults() {
        let s = tmp("missing");
        assert_eq!(s.load_config(), Config::default());
        assert!(s.load_history().is_empty());
    }

    #[test]
    fn concurrent_saves_publish_complete_files_without_colliding() {
        use std::sync::{Arc, Barrier};

        let store = tmp("concurrent");
        store.save_config(&Config::default()).unwrap();
        let start = Arc::new(Barrier::new(12));
        let threads: Vec<_> = (0..12)
            .map(|worker| {
                let store = store.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    for turn in 0..8 {
                        let config = Config {
                            wait_cap_s: worker * 8 + turn,
                            ..Config::default()
                        };
                        store.save_config(&config).unwrap();
                        let contents = fs::read(store.root().join("config.json")).unwrap();
                        serde_json::from_slice::<Config>(&contents).unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(fs::read_dir(store.root()).unwrap().count(), 1);
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
        let private = user_temp_dir().unwrap();
        assert!(a.starts_with(&private), "{}", a.display());
        // Only we can create names in it.
        use std::os::unix::fs::MetadataExt;
        let meta = fs::metadata(&private).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o700);
        assert_eq!(
            meta.uid(),
            fs::metadata(std::env::home_dir().unwrap()).unwrap().uid()
        );
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
