//! Run shell startup off the event loop, with conservative cached checks.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use headroom_core::shims::{self, ShimsStatus};

const VERIFY_TTL: Duration = Duration::from_secs(60);
const WORKER_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone, PartialEq, Eq)]
struct Installation(Vec<Option<PathBuf>>);

impl Installation {
    fn read(dir: &Path) -> Self {
        Self(
            shims::NAMES
                .iter()
                .map(|name| {
                    let path = dir.join(name);
                    shims::valid_shim(&path)
                        .then(|| path.canonicalize().ok())
                        .flatten()
                })
                .collect(),
        )
    }
}

struct Pending {
    inputs: Installation,
    result: Receiver<ShimsStatus>,
    invalidated: bool,
    started: Instant,
}

struct Cached {
    inputs: Installation,
    checked: Instant,
    state: ShimsStatus,
}

#[derive(Default)]
pub(crate) struct ShimsVerification {
    pending: Option<Pending>,
    cached: Option<Cached>,
}

impl ShimsVerification {
    pub(crate) fn refresh(&mut self, home: &Path, now: Instant, force: bool) -> ShimsStatus {
        self.refresh_with(home, now, force, |home, dir| {
            background_probe(home, dir, shims::check_default_shell)
        })
    }

    fn refresh_with(
        &mut self,
        home: &Path,
        now: Instant,
        force: bool,
        launch: impl FnOnce(PathBuf, PathBuf) -> Receiver<ShimsStatus>,
    ) -> ShimsStatus {
        let dir = shims::default_dir(home);
        let inputs = Installation::read(&dir);
        if force
            || self
                .cached
                .as_ref()
                .is_some_and(|c| c.inputs != inputs || now.duration_since(c.checked) >= VERIFY_TTL)
        {
            self.cached = None;
        }
        if let Some(pending) = self.pending.as_mut() {
            pending.invalidated |= force || pending.inputs != inputs;
            match pending.result.try_recv() {
                Err(TryRecvError::Empty) => {}
                result => {
                    let pending = self.pending.take().unwrap();
                    if !pending.invalidated && now.duration_since(pending.started) < VERIFY_TTL {
                        self.cached = Some(Cached {
                            inputs: pending.inputs,
                            checked: now,
                            state: result.unwrap_or_else(|_| {
                                ShimsStatus::checking(&dir)
                                    .unverified("The default shell status check was unavailable.")
                            }),
                        });
                    }
                }
            }
        }
        if let Some(cached) = &self.cached {
            return cached.state.clone();
        }
        if self.pending.is_none() {
            self.pending = Some(Pending {
                inputs,
                result: launch(home.into(), dir.clone()),
                invalidated: false,
                started: now,
            });
        }
        let checking = ShimsStatus::checking(&dir);
        if self
            .pending
            .as_ref()
            .is_some_and(|p| now.duration_since(p.started) >= WORKER_DEADLINE)
        {
            // Account directory services can also stall. Keep a single worker
            // in flight and show uncertainty instead of launching more threads.
            checking.unverified("The default shell status check timed out.")
        } else {
            checking
        }
    }
}

fn background_probe(
    home: PathBuf,
    dir: PathBuf,
    probe: impl FnOnce(&Path, &Path) -> ShimsStatus + Send + 'static,
) -> Receiver<ShimsStatus> {
    let (sender, receiver) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("shim-shell-status".into())
        .spawn(move || {
            let _ = sender.send(probe(&home, &dir));
        });
    receiver
}

#[cfg(test)]
mod tests {
    use super::*;
    use headroom_core::shims::Status;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::Sender;

    struct Fixture {
        home: PathBuf,
        refresh: ShimsVerification,
        requests: Vec<Sender<ShimsStatus>>,
        now: Instant,
    }

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let home = std::env::temp_dir().join(format!(
                "headroom-shim-refresh-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&home).unwrap();
            let binary = home.join("headroom");
            fs::write(&binary, "#!/bin/sh\n").unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
            let dir = shims::default_dir(&home);
            fs::create_dir_all(&dir).unwrap();
            for name in shims::NAMES {
                symlink(&binary, dir.join(name)).unwrap();
            }
            Self {
                home,
                refresh: ShimsVerification::default(),
                requests: Vec::new(),
                now: Instant::now(),
            }
        }

        fn tick(&mut self, force: bool) -> ShimsStatus {
            self.refresh
                .refresh_with(&self.home, self.now, force, |home, dir| {
                    assert_eq!(home, self.home);
                    assert_eq!(dir, shims::default_dir(&home));
                    let (sender, receiver) = mpsc::channel();
                    self.requests.push(sender);
                    receiver
                })
        }

        fn answer(&self, index: usize, status: Status) {
            let mut result = ShimsStatus::checking(&shims::default_dir(&self.home));
            result.status = status;
            result.shell = Some("/fixture/zsh".into());
            self.requests[index].send(result).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.home);
        }
    }

    #[test]
    fn background_probe_never_runs_on_the_calling_thread() {
        let caller = std::thread::current().id();
        let (release_tx, release_rx) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let result = background_probe(
            "/fixture/home".into(),
            "/fixture/shims".into(),
            move |home, dir| {
                assert_ne!(std::thread::current().id(), caller);
                assert_eq!(home, Path::new("/fixture/home"));
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                ShimsStatus::checking(dir)
            },
        );
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(result.try_recv(), Err(TryRecvError::Empty));
        release_tx.send(()).unwrap();
        assert_eq!(
            result.recv_timeout(Duration::from_secs(2)).unwrap().status,
            Status::Checking
        );
    }

    #[test]
    fn installation_count_is_available_while_the_single_probe_is_pending() {
        let mut f = Fixture::new();
        let state = f.tick(false);
        assert_eq!(state.status, Status::Checking);
        assert_eq!(state.installed, 16);
        assert_eq!(state.total, 16);
        for _ in 0..10 {
            assert_eq!(f.tick(false).status, Status::Checking);
        }
        assert_eq!(f.requests.len(), 1);
        f.answer(0, Status::Active);
        assert_eq!(f.tick(false).status, Status::Active);
        f.now += VERIFY_TTL - Duration::from_secs(1);
        assert_eq!(f.tick(false).status, Status::Active);
        assert_eq!(f.requests.len(), 1);
        f.now += Duration::from_secs(1);
        assert_eq!(f.tick(false).status, Status::Checking);
        assert_eq!(f.requests.len(), 2);
    }

    #[test]
    fn check_again_invalidates_a_cached_result_without_waiting_for_ttl() {
        let mut f = Fixture::new();
        f.tick(false);
        f.answer(0, Status::Active);
        assert_eq!(f.tick(false).status, Status::Active);
        assert_eq!(f.tick(true).status, Status::Checking);
        assert_eq!(f.requests.len(), 2);
        f.answer(1, Status::NotOnPath);
        assert_eq!(f.tick(false).status, Status::NotOnPath);
    }

    #[test]
    fn installation_changes_discard_results_for_the_old_links() {
        let mut f = Fixture::new();
        f.tick(false);
        f.answer(0, Status::Active);
        fs::remove_file(shims::default_dir(&f.home).join("pnpm")).unwrap();
        let state = f.tick(false);
        assert_eq!(state.status, Status::Checking);
        assert_eq!(state.installed, 15);
        assert_eq!(f.requests.len(), 2);
        f.answer(1, Status::Partial);
        assert_eq!(f.tick(false).status, Status::Partial);
    }

    #[test]
    fn stalled_workers_and_manual_retries_never_spawn_overlapping_shells() {
        let mut f = Fixture::new();
        f.tick(false);
        f.now += WORKER_DEADLINE;
        assert_eq!(f.tick(false).status, Status::Unverified);
        assert_eq!(f.tick(true).status, Status::Unverified);
        assert_eq!(f.requests.len(), 1);
        f.answer(0, Status::Active);
        assert_eq!(f.tick(false).status, Status::Checking);
        assert_eq!(f.requests.len(), 2);
    }

    #[test]
    fn disconnected_workers_show_uncertainty_and_throttle_retries() {
        let mut f = Fixture::new();
        f.tick(false);
        f.requests.clear();
        assert_eq!(f.tick(false).status, Status::Unverified);
        assert_eq!(f.tick(false).status, Status::Unverified);
        assert!(f.requests.is_empty());
        f.now += VERIFY_TTL;
        assert_eq!(f.tick(false).status, Status::Checking);
        assert_eq!(f.requests.len(), 1);
    }
}
