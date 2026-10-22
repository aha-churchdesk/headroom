//! Read-only Codex verification away from the app's event loop.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use headroom_core::hookio::Agent;
use headroom_core::hooksettings::{self, Status};

// Also bounds retries when Codex is missing or its API is unavailable. Refresh
// periodically even with unchanged local files: external config and the Codex
// executable can change the effective state.
const VERIFY_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, PartialEq, Eq)]
struct Inputs {
    hooks: Vec<u8>,
    config: Option<Vec<u8>>,
}

impl Inputs {
    fn read(path: &Path) -> Option<Self> {
        let hooks = fs::read(path).ok()?;
        let config = match fs::read(path.parent()?.join("config.toml")) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(_) => return None,
        };
        Some(Self { hooks, config })
    }
}

type ProbeResult = Result<Status, String>;

struct Pending {
    inputs: Inputs,
    result: Receiver<ProbeResult>,
    invalidated: bool,
    started: Instant,
}

struct Cached {
    inputs: Inputs,
    checked: Instant,
    status: Option<Status>,
}

#[derive(Default)]
pub(crate) struct HookVerification {
    pending: Option<Pending>,
    cached: Option<Cached>,
}

impl HookVerification {
    pub(crate) fn refresh(
        &mut self,
        path: &Path,
        cli: &str,
        fallback: &'static str,
        now: Instant,
    ) -> &'static str {
        self.refresh_with(path, cli, fallback, now, |path, cli, inputs| {
            background_probe(path, cli, move |path, cli| {
                verify_snapshot(path, cli, &inputs)
            })
        })
    }

    fn refresh_with(
        &mut self,
        path: &Path,
        cli: &str,
        fallback: &'static str,
        now: Instant,
        launch: impl FnOnce(PathBuf, String, Inputs) -> Receiver<ProbeResult>,
    ) -> &'static str {
        let eligible = matches!(fallback, "needs_review" | "review_unknown" | "disabled");
        let inputs = eligible.then(|| Inputs::read(path)).flatten();

        if self.cached.as_ref().is_some_and(|cache| {
            Some(&cache.inputs) != inputs.as_ref()
                || now.duration_since(cache.checked) >= VERIFY_TTL
        }) {
            self.cached = None;
        }

        if let Some(pending) = self.pending.as_mut() {
            pending.invalidated |= Some(&pending.inputs) != inputs.as_ref();
            match pending.result.try_recv() {
                Err(TryRecvError::Empty) => {}
                result => {
                    let pending = self.pending.take().unwrap();
                    if !pending.invalidated && now.duration_since(pending.started) < VERIFY_TTL {
                        self.cached = Some(Cached {
                            inputs: pending.inputs,
                            checked: pending.started,
                            // A failed check replaces any earlier Installed result
                            // with the current file-only status until the next retry.
                            status: result.ok().and_then(Result::ok),
                        });
                    }
                }
            }
        }

        if let Some(cache) = &self.cached {
            return match cache.status.as_ref() {
                Some(Status::Installed) => "installed",
                Some(Status::NeedsReview) => "needs_review",
                Some(Status::Disabled) => "disabled",
                Some(Status::ReviewStatusUnknown) => "review_unknown",
                Some(Status::NotInstalled) => "not_installed",
                Some(Status::Stale { .. }) => "stale",
                None => fallback,
            };
        }

        if self.pending.is_none()
            && let Some(inputs) = inputs
        {
            self.pending = Some(Pending {
                result: launch(path.to_path_buf(), cli.to_owned(), inputs.clone()),
                inputs,
                invalidated: false,
                started: now,
            });
        }
        fallback
    }
}

fn verify_snapshot(path: &Path, cli: &str, inputs: &Inputs) -> ProbeResult {
    // Bind the core's own pre-query reads to the UI's cache key. Checking only
    // after completion would miss A -> B -> A edits during a pending check.
    hooksettings::verified_status_for_snapshot(
        Agent::Codex,
        path,
        cli,
        &inputs.hooks,
        inputs.config.as_deref(),
    )
}

fn background_probe(
    path: PathBuf,
    cli: String,
    probe: impl FnOnce(&Path, &str) -> ProbeResult + Send + 'static,
) -> Receiver<ProbeResult> {
    let (sender, receiver) = mpsc::channel();
    // Spawn failure or a worker panic disconnects the channel; refresh treats
    // that like any other unavailable probe and applies the retry interval.
    let _ = thread::Builder::new()
        .name("codex-hook-status".into())
        .spawn(move || {
            let _ = sender.send(probe(&path, &cli));
        });
    receiver
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::Sender;

    struct Fixture {
        path: PathBuf,
        refresh: HookVerification,
        requests: Vec<Sender<ProbeResult>>,
        now: Instant,
    }

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "headroom-hook-refresh-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join("hooks.json");
            fs::write(&path, "original hook bytes").unwrap();
            Self {
                path,
                refresh: HookVerification::default(),
                requests: Vec::new(),
                now: Instant::now(),
            }
        }

        fn tick(&mut self, fallback: &'static str) -> &'static str {
            self.refresh.refresh_with(
                &self.path,
                "/bin/headroom",
                fallback,
                self.now,
                |path, cli, inputs| {
                    assert_eq!(path, self.path);
                    assert_eq!(cli, "/bin/headroom");
                    assert!(Some(inputs) == Inputs::read(&path));
                    let (sender, receiver) = mpsc::channel();
                    self.requests.push(sender);
                    receiver
                },
            )
        }

        fn answer(&self, index: usize, result: ProbeResult) {
            self.requests[index].send(result).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.path.parent().unwrap());
        }
    }

    #[test]
    fn probe_runs_off_the_calling_thread_and_returns_only_metadata() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let caller = thread::current().id();
        let result = background_probe(
            "/fixture/hooks.json".into(),
            "headroom".into(),
            move |p, c| {
                assert_ne!(thread::current().id(), caller);
                assert_eq!(p, Path::new("/fixture/hooks.json"));
                assert_eq!(c, "headroom");
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(Status::Installed)
            },
        );
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(result.try_recv(), Err(TryRecvError::Empty));
        release_tx.send(()).unwrap();
        assert_eq!(
            result.recv_timeout(Duration::from_secs(2)).unwrap(),
            Ok(Status::Installed)
        );
    }

    #[test]
    fn only_one_request_runs_and_unchanged_inputs_are_cached_until_ttl() {
        let mut f = Fixture::new();
        assert_eq!(f.tick("review_unknown"), "review_unknown");
        for _ in 0..10 {
            f.now += Duration::from_secs(5);
            assert_eq!(f.tick("review_unknown"), "review_unknown");
        }
        assert_eq!(f.requests.len(), 1);
        f.answer(0, Ok(Status::Installed));
        assert_eq!(f.tick("review_unknown"), "installed");
        f.now += Duration::from_secs(9);
        assert_eq!(f.tick("review_unknown"), "installed");
        assert_eq!(f.requests.len(), 1);
        f.now += Duration::from_secs(1);
        assert_eq!(f.tick("review_unknown"), "review_unknown");
        assert_eq!(f.requests.len(), 2);
        f.answer(1, Ok(Status::NeedsReview));
        assert_eq!(f.tick("review_unknown"), "needs_review");
    }

    #[test]
    fn changes_to_either_file_invalidate_installed_and_discard_inflight_results() {
        for filename in ["hooks.json", "config.toml"] {
            let mut f = Fixture::new();
            let changed = f.path.parent().unwrap().join(filename);
            fs::write(&changed, "before").unwrap();
            f.tick("review_unknown");
            f.answer(0, Ok(Status::Installed));
            assert_eq!(f.tick("review_unknown"), "installed");
            // Same length: cache keys must compare bytes, not file sizes.
            fs::write(&changed, "after!").unwrap();
            assert_eq!(f.tick("review_unknown"), "review_unknown");
            assert_eq!(f.requests.len(), 2);
            fs::write(&changed, "third!").unwrap();
            assert_eq!(f.tick("review_unknown"), "review_unknown");
            assert_eq!(f.requests.len(), 2);
            f.answer(1, Ok(Status::Installed));
            assert_eq!(f.tick("review_unknown"), "review_unknown");
            assert_eq!(f.requests.len(), 3);
            f.answer(2, Ok(Status::Installed));
            assert_eq!(f.tick("review_unknown"), "installed");
        }
    }

    #[test]
    fn errors_and_disconnected_workers_use_fast_status_and_throttle_retries() {
        for disconnected in [false, true] {
            let mut f = Fixture::new();
            f.tick("needs_review");
            if disconnected {
                f.requests.clear();
            } else {
                f.answer(0, Err("Codex missing or incompatible".into()));
            }
            assert_eq!(f.tick("needs_review"), "needs_review");
            let count = f.requests.len();
            for _ in 0..11 {
                f.now += Duration::from_secs(5);
                assert_eq!(f.tick("needs_review"), "needs_review");
            }
            assert_eq!(f.requests.len(), count);
            f.now += Duration::from_secs(5);
            f.tick("needs_review");
            assert_eq!(f.requests.len(), count + 1);
        }
    }

    #[test]
    fn definite_fast_statuses_skip_probes_and_override_cached_trust() {
        for fallback in ["absent", "not_installed", "stale"] {
            let mut f = Fixture::new();
            assert_eq!(f.tick(fallback), fallback);
            assert!(f.requests.is_empty());
            f.tick("needs_review");
            f.answer(0, Ok(Status::Installed));
            assert_eq!(f.tick("needs_review"), "installed");
            assert_eq!(f.tick(fallback), fallback);
            assert_eq!(f.requests.len(), 1);
        }
    }

    #[test]
    fn disabled_file_status_is_checked_for_effective_config_overrides() {
        let mut f = Fixture::new();
        assert_eq!(f.tick("disabled"), "disabled");
        assert_eq!(f.requests.len(), 1);
        f.answer(0, Ok(Status::Installed));
        assert_eq!(f.tick("disabled"), "installed");
        assert_eq!(f.requests.len(), 1);
        fs::write(f.path.parent().unwrap().join("config.toml"), "changed").unwrap();
        assert_eq!(f.tick("disabled"), "disabled");
        assert_eq!(f.requests.len(), 2);
    }

    #[test]
    fn worker_rejects_changed_snapshot_even_if_files_revert_before_ui_refresh() {
        for filename in ["hooks.json", "config.toml"] {
            let mut f = Fixture::new();
            fs::write(&f.path, "{}").unwrap();
            hooksettings::install(Agent::Codex, &f.path, "/bin/headroom", false).unwrap();
            fs::write(
                f.path.parent().unwrap().join("config.toml"),
                "[features]\nhooks = true\n",
            )
            .unwrap();
            let mut worker = None;
            assert_eq!(
                f.refresh.refresh_with(
                    &f.path,
                    "/bin/headroom",
                    "review_unknown",
                    f.now,
                    |path, cli, expected| {
                        let (sender, receiver) = mpsc::channel();
                        worker = Some(thread::spawn(move || {
                            // The UI captured A, but the worker begins with B.
                            let changed = path.parent().unwrap().join(filename);
                            let original = fs::read(&changed).unwrap();
                            let mut other = original.clone();
                            other.push(b'\n');
                            fs::write(&changed, other).unwrap();
                            // The real core entry point rejects this mismatch
                            // before it can launch an external Codex process.
                            let result = verify_snapshot(&path, &cli, &expected);
                            fs::write(&changed, original).unwrap();
                            sender.send(result.clone()).unwrap();
                            result
                        }));
                        receiver
                    },
                ),
                "review_unknown"
            );
            assert!(worker.unwrap().join().unwrap().is_err());
            // The files are back to A. A result for B must not bless A.
            assert_eq!(f.tick("review_unknown"), "review_unknown");
            assert!(f.requests.is_empty());
        }
    }

    #[test]
    fn observed_change_cannot_restore_an_old_pending_result() {
        let mut f = Fixture::new();
        f.tick("needs_review");
        assert_eq!(f.tick("stale"), "stale");
        assert_eq!(f.tick("needs_review"), "needs_review");
        f.answer(0, Ok(Status::Installed));
        assert_eq!(f.tick("needs_review"), "needs_review");
        assert_eq!(f.requests.len(), 2);
    }

    #[test]
    fn reopening_after_ttl_discards_uncollected_results() {
        let mut f = Fixture::new();
        f.tick("review_unknown");
        f.answer(0, Ok(Status::Installed));
        // The popover can stay closed long after the worker has finished.
        f.now += VERIFY_TTL;
        assert_eq!(f.tick("review_unknown"), "review_unknown");
        assert_eq!(f.requests.len(), 2);
    }

    #[test]
    fn unreadable_inputs_cannot_keep_a_verified_result() {
        let mut f = Fixture::new();
        f.tick("review_unknown");
        f.answer(0, Ok(Status::Installed));
        assert_eq!(f.tick("review_unknown"), "installed");
        // A directory is consistently unreadable as bytes, including as root.
        fs::create_dir(f.path.parent().unwrap().join("config.toml")).unwrap();
        assert_eq!(f.tick("review_unknown"), "review_unknown");
        assert_eq!(f.requests.len(), 1);
    }

    #[test]
    fn all_verified_statuses_are_preserved() {
        for (status, expected) in [
            (Status::Installed, "installed"),
            (Status::NeedsReview, "needs_review"),
            (Status::Disabled, "disabled"),
            (Status::ReviewStatusUnknown, "review_unknown"),
            (Status::NotInstalled, "not_installed"),
            (Status::Stale { exe: "old".into() }, "stale"),
        ] {
            let mut f = Fixture::new();
            f.tick("needs_review");
            f.answer(0, Ok(status));
            assert_eq!(f.tick("needs_review"), expected);
            assert_eq!(f.requests.len(), 1);
        }
    }
}
