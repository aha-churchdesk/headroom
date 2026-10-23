//! The menu bar app's state, without the window around it.
//!
//! The daemon owns the queue; this side only asks it for a listing, reads
//! what the daemon persists (config, profiles), looks at a few things the
//! daemon has no business knowing (other apps, agent hooks, the login item)
//! and turns popover intents into requests. `main.rs` drives it from the
//! event loop, so everything here can be exercised against a real daemon in
//! a test without a window.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use headroom_core::config::Config;
use headroom_core::hookio::Agent;
use headroom_core::hooksettings::{self, Scope, Status};
use headroom_core::profile::Profiles;
use headroom_core::protocol::{ClientMsg, Control, DaemonMsg, Listing, request};
use headroom_core::shims::{self, ShimsStatus};
use headroom_core::store::{Store, UiPreferences};
use headroom_core::tally::Tally;
use serde_json::Value;

use crate::bridge::{self, Do, UiCmd};
use crate::hook_refresh::HookVerification;
use crate::shim_refresh::ShimsVerification;
use crate::view::{self, AppUsage, Extras, Login};
use crate::{login, sample};

/// Apps listed on the "Other apps" page.
const OTHERS: usize = 8;

fn wall() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    Continue,
    Quit,
}

pub struct App {
    store: Store,
    socket: PathBuf,
    home: PathBuf,
    /// The `headroom` CLI, as written into agent hooks.
    cli: String,
    /// What the login item launches.
    exe: String,
    since: u64,
    dismissed: HashSet<String>,
    classify: Option<Value>,
    cfg: Config,
    profiles: Profiles,
    tally: Tally,
    others: Vec<AppUsage>,
    hooks: Vec<(&'static str, &'static str)>,
    hook_verification: HookVerification,
    shims: ShimsStatus,
    shims_verification: ShimsVerification,
    login: Login,
    listing: Option<Listing>,
    attached: bool,
    data_clear: Option<Value>,
    ui_preferences: UiPreferences,
    setup_error: Option<String>,
    copy_result: Option<Value>,
    /// The last thing the daemon refused, for stderr; not shown in the page.
    pub last_error: Option<String>,
}

/// Per agent, the hook as the Settings page words it. A hook pointing at a
/// different `headroom` that still exists (a dev build, the brew one) works,
/// so only a missing binary counts as stale.
pub fn hook_states(home: &Path, cli: &str) -> Vec<(&'static str, &'static str)> {
    Agent::ALL
        .iter()
        .map(|&agent| {
            let st = if !hooksettings::agent_present(agent, home) {
                "absent"
            } else {
                let path = hooksettings::settings_path(agent, Scope::User, home, home);
                match path.map(|p| hooksettings::status(agent, &p, cli)) {
                    Some(Ok(Status::Installed)) => "installed",
                    Some(Ok(Status::NeedsReview)) => "needs_review",
                    Some(Ok(Status::Disabled)) => "disabled",
                    Some(Ok(Status::ReviewStatusUnknown)) => "review_unknown",
                    Some(Ok(Status::Stale { exe }))
                        if agent != Agent::Codex && Path::new(&exe).exists() =>
                    {
                        "installed"
                    }
                    Some(Ok(Status::Stale { .. })) => "stale",
                    Some(Err(_)) if agent == Agent::Codex => "review_unknown",
                    // For other agents, an unreadable or broken file is shown as missing:
                    // `headroom hook install` explains what is wrong with it.
                    _ => "not_installed",
                }
            };
            (agent.name(), st)
        })
        .collect()
}

impl App {
    pub fn new(store: Store, home: PathBuf, cli: String, exe: String) -> Self {
        let socket = store.socket();
        let mut app = Self {
            cfg: store.load_config(),
            profiles: store.load_profiles(),
            tally: store.load_tally(),
            ui_preferences: store.load_ui_preferences(),
            shims: ShimsStatus::checking(&shims::default_dir(&home)),
            shims_verification: ShimsVerification::default(),
            store,
            socket,
            home,
            cli,
            exe,
            since: wall(),
            dismissed: HashSet::new(),
            classify: None,
            others: Vec::new(),
            hooks: Vec::new(),
            hook_verification: HookVerification::default(),
            login: Login::Off,
            listing: None,
            attached: false,
            data_clear: None,
            setup_error: None,
            copy_result: None,
            last_error: None,
        };
        app.refresh_settings();
        app.refresh_hooks();
        app.refresh_shims(false);
        app
    }

    /// Ask the daemon for the queue. A failed ask keeps the last listing:
    /// a daemon restarting for a second shouldn't blank the popover.
    pub fn poll(&mut self) -> bool {
        self.refresh_shims(false);
        match request(&self.socket, &ClientMsg::List) {
            Ok(DaemonMsg::Jobs(l)) => {
                self.listing = Some(l);
                true
            }
            Ok(other) => {
                self.last_error = Some(format!("unexpected answer {other:?}"));
                false
            }
            Err(e) => {
                self.last_error = Some(e);
                false
            }
        }
    }

    /// What the daemon persists, and the login item.
    pub fn refresh_settings(&mut self) {
        self.cfg = self.store.load_config();
        self.profiles = self.store.load_profiles();
        self.tally = self.store.load_tally();
        self.login = login::state(&self.home);
    }

    /// Walks every process, so only while the popover is open.
    pub fn refresh_others(&mut self) {
        self.refresh_others_among(&sample::all_pids());
    }

    /// The "Other apps" page from just these processes. Paths under the
    /// home directory are shown from `~`.
    pub fn refresh_others_among(&mut self, pids: &[i32]) {
        let skip: Vec<i32> = self
            .listing
            .iter()
            .flat_map(|l| l.running.iter().filter_map(|j| j.pgid))
            .collect();
        let home = self.home.display().to_string();
        let short = |s: &str| match s.strip_prefix(&home) {
            Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => {
                format!("~{rest}")
            }
            _ => s.to_string(),
        };
        self.others = sample::top_apps(pids, &skip, OTHERS)
            .into_iter()
            .map(|mut a| {
                a.path = short(&a.path);
                for p in &mut a.top {
                    p.cmd = p.cmd.replace(&format!("{home}/"), "~/");
                    p.cwd = p.cwd.as_deref().map(short);
                }
                a
            })
            .collect();
    }

    pub fn refresh_hooks(&mut self) {
        self.hooks = hook_states(&self.home, &self.cli);
        if let Some((_, state)) = self.hooks.iter_mut().find(|(agent, _)| *agent == "codex") {
            *state = self.hook_verification.refresh(
                &self.home.join(".codex/hooks.json"),
                &self.cli,
                state,
                Instant::now(),
            );
        }
    }

    fn refresh_shims(&mut self, force: bool) {
        self.shims = self
            .shims_verification
            .refresh(&self.home, Instant::now(), force);
    }

    /// Whether the queue this app shows runs in another headroomd.
    pub fn set_attached(&mut self, on: bool) {
        self.attached = on;
    }

    pub fn listing(&self) -> Option<&Listing> {
        self.listing.as_ref()
    }

    /// The popover's state, once the daemon has answered at least once.
    pub fn state(&self) -> Option<Value> {
        let l = self.listing.as_ref()?;
        let mut state = view::state(
            l,
            &Extras {
                cfg: &self.cfg,
                profiles: &self.profiles,
                tally: &self.tally,
                others: &self.others,
                hooks: &self.hooks,
                classify: self.classify.clone(),
                dismissed: &self.dismissed,
                wall: wall(),
                since: self.since,
                login: self.login,
                attached: self.attached,
            },
        );
        state["data_clear"] = self.data_clear.clone().unwrap_or(Value::Null);
        state["setup"] = serde_json::json!({
            "complete": self.ui_preferences.setup_complete,
            "error": self.setup_error,
        });
        state["copy_result"] = self.copy_result.clone().unwrap_or(Value::Null);
        state["shims"] = serde_json::to_value(&self.shims).unwrap();
        Some(state)
    }

    /// One intent from the page. Anything malformed or refused is noted in
    /// `last_error` and otherwise ignored: the page re-renders from the
    /// next state either way.
    pub fn command(&mut self, body: &str) -> Next {
        let cmd = match serde_json::from_str::<UiCmd>(body) {
            Ok(c) => c,
            Err(e) => {
                self.last_error = Some(format!("bad message {body}: {e}"));
                return Next::Continue;
            }
        };
        let action = match bridge::translate(cmd, &self.cfg) {
            Ok(a) => a,
            Err(e) => {
                self.last_error = Some(e);
                return Next::Continue;
            }
        };
        match action {
            Do::Quit => return Next::Quit,
            Do::CheckShims => {
                self.refresh_shims(true);
                return Next::Continue;
            }
            Do::FinishSetup => {
                let mut preferences = self.ui_preferences.clone();
                preferences.setup_complete = true;
                match self.store.save_ui_preferences(&preferences) {
                    Ok(()) => {
                        self.ui_preferences = preferences;
                        self.setup_error = None;
                    }
                    Err(e) => {
                        self.setup_error = Some(format!("Could not save setup: {e}"));
                    }
                }
                return Next::Continue;
            }
            Do::Control(c) => {
                if c == Control::ClearData {
                    let result = match request(&self.socket, &ClientMsg::Control(c)) {
                        Ok(DaemonMsg::Ok) => Ok(()),
                        Ok(DaemonMsg::Error { msg }) => Err(msg),
                        Ok(other) => Err(format!("unexpected clear-data answer {other:?}")),
                        Err(e) => Err(e),
                    };
                    self.data_clear = Some(match result {
                        Ok(()) => {
                            self.refresh_settings();
                            self.dismissed.clear();
                            self.classify = None;
                            self.last_error = None;
                            serde_json::json!({ "ok": true, "message": "Saved profiles, history and statistics cleared." })
                        }
                        Err(message) => {
                            self.last_error = Some(message.clone());
                            serde_json::json!({ "ok": false, "message": message })
                        }
                    });
                    self.poll();
                    return Next::Continue;
                }
                // The daemon saves the config after it answers, so the new
                // one is taken from what was sent rather than re-read.
                let sent = match &c {
                    Control::Configure { config } => Some((**config).clone()),
                    _ => None,
                };
                match request(&self.socket, &ClientMsg::Control(c)) {
                    Ok(DaemonMsg::Error { msg }) => self.last_error = Some(msg),
                    Ok(_) => {
                        if let Some(cfg) = sent {
                            self.cfg = cfg;
                        }
                    }
                    Err(e) => self.last_error = Some(e),
                }
            }
            Do::Dismiss(id) => {
                self.dismissed.insert(id);
            }
            Do::Copy(text) => {
                let result = copy(&text);
                self.record_copy_result(&text, result);
            }
            Do::Login(on) => match login::set(&self.home, on, &self.exe) {
                Ok(l) => self.login = l,
                Err(e) => self.last_error = Some(format!("login item: {e}")),
            },
            Do::Classify(text) => self.classify = Some(bridge::classify_json(&text, &self.cfg)),
        }
        self.poll();
        Next::Continue
    }

    fn record_copy_result(&mut self, text: &str, result: std::io::Result<()>) {
        let (ok, message) = match result {
            Ok(()) => (true, "Copied to clipboard.".to_string()),
            Err(e) => {
                self.last_error = Some(format!("copy: {e}"));
                (false, format!("Could not copy: {e}"))
            }
        };
        self.copy_result = Some(serde_json::json!({
            "text": text,
            "ok": ok,
            "message": message,
        }));
    }
}

fn copy(text: &str) -> std::io::Result<()> {
    copy_with_command(text, &mut Command::new("/usr/bin/pbcopy"))
}

fn copy_with_command(text: &str, command: &mut Command) -> std::io::Result<()> {
    let mut child = command.stdin(Stdio::piped()).spawn()?;
    let written = if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())
    } else {
        Err(std::io::Error::other("clipboard input unavailable"))
    };
    let status = child.wait()?;
    written?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "pbcopy exited with {status}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::{self, Probe};
    use headroom_core::model::{GIB, Pressure, Snapshot};
    use headroom_core::scheduler::Usage;
    use std::collections::HashMap;
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    struct Calm;
    impl Probe for Calm {
        fn system(&mut self) -> Snapshot {
            Snapshot {
                pressure: Pressure::Normal,
                mem_total: 32 * GIB,
                mem_used: 8 * GIB,
                mem_available: 24 * GIB,
                cores: 8,
                ..Snapshot::default()
            }
        }
        fn groups(&mut self, _: &[i32]) -> HashMap<i32, Usage> {
            HashMap::new()
        }
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hr-app-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    struct Running {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }
    impl Drop for Running {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    fn with_daemon(name: &str) -> (App, Running) {
        let root = dir(name);
        let stop = Arc::new(AtomicBool::new(false));
        let (s, store) = (stop.clone(), Store::at(root.join("data")).unwrap());
        let thread =
            std::thread::spawn(move || daemon::run_with(store, s, Box::new(Calm)).unwrap());
        let mut app = App::new(
            Store::at(root.join("data")).unwrap(),
            root.join("home"),
            "/usr/local/bin/headroom".into(),
            "/Applications/Headroom.app/Contents/MacOS/headroomd".into(),
        );
        let t0 = Instant::now();
        while !app.poll() {
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "daemon never answered"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        app.last_error = None;
        (
            app,
            Running {
                stop,
                thread: Some(thread),
            },
        )
    }

    #[test]
    fn clear_data_reports_durable_success_and_preserves_settings() {
        let (mut app, _d) = with_daemon("clear-data");
        app.command(r#"{"cmd":"finish_setup"}"#);
        app.command(r#"{"cmd":"set","key":"wait_cap_s","value":123}"#);
        app.command(r#"{"cmd":"hold","on":true}"#);
        // Persisted legacy data is cleared even if this daemon did not load it.
        std::fs::write(
            app.store.root().join("profiles.json.corrupt"),
            "old private data",
        )
        .unwrap();
        assert_eq!(app.command(r#"{"cmd":"clear_data"}"#), Next::Continue);
        let state = app.state().unwrap();
        assert_eq!(state["data_clear"]["ok"], true);
        assert_eq!(state["held"], true);
        assert_eq!(state["settings"]["wait_cap_s"], 123);
        assert_eq!(state["setup"]["complete"], true);
        assert!(app.store.load_ui_preferences().setup_complete);
        assert_eq!(app.store.load_config().wait_cap_s, 123);
        assert!(!app.store.root().join("profiles.json.corrupt").exists());
        assert_eq!(app.store.load_profiles(), Profiles::default());
        assert!(app.store.load_history().is_empty());
        assert_eq!(app.store.load_tally(), Tally::default());
    }

    #[test]
    fn finishing_setup_is_durable_and_does_not_change_scheduler_state() {
        let (mut app, _d) = with_daemon("finish-setup");
        app.command(r#"{"cmd":"set","key":"wait_cap_s","value":123}"#);
        app.command(r#"{"cmd":"hold","on":true}"#);
        let config = fs::read(app.store.root().join("config.json")).unwrap();
        let mut before = app.state().unwrap();
        assert_eq!(
            before["setup"],
            serde_json::json!({"complete": false, "error": null})
        );
        assert_eq!(before["copy_result"], Value::Null);
        assert_eq!(app.command(r#"{"cmd":"finish_setup"}"#), Next::Continue);
        before["setup"]["complete"] = Value::Bool(true);
        let mut after = app.state().unwrap();
        // Background shell verification is independent of setup completion.
        before.as_object_mut().unwrap().remove("shims");
        after.as_object_mut().unwrap().remove("shims");
        assert_eq!(after, before);
        assert_eq!(
            fs::read(app.store.root().join("config.json")).unwrap(),
            config
        );
        let mut reopened = App::new(
            app.store.clone(),
            app.home.clone(),
            app.cli.clone(),
            app.exe.clone(),
        );
        assert!(reopened.poll());
        assert_eq!(
            reopened.state().unwrap()["setup"],
            serde_json::json!({"complete": true, "error": null})
        );
        reopened.refresh_settings();
        assert!(reopened.ui_preferences.setup_complete);
    }

    #[test]
    fn finishing_setup_without_a_daemon_only_saves_ui_preferences() {
        let root = dir("finish-without-daemon");
        let mut app = App::new(
            Store::at(root.join("data")).unwrap(),
            root.join("home"),
            "h".into(),
            "d".into(),
        );
        app.command(r#"{"cmd":"finish_setup"}"#);
        assert!(app.store.load_ui_preferences().setup_complete);
        assert_eq!(app.last_error, None, "no daemon request is needed");
        assert!(!app.store.root().join("config.json").exists());
    }

    #[test]
    fn checking_shims_does_not_need_or_control_a_daemon() {
        let root = dir("check-shims-local");
        let mut app = App::new(
            Store::at(root.join("data")).unwrap(),
            root.join("home"),
            "h".into(),
            "d".into(),
        );
        assert_eq!(app.command(r#"{"cmd":"check_shims"}"#), Next::Continue);
        assert_eq!(app.last_error, None);
        assert_eq!(app.shims.installed, 0);
        assert_eq!(app.shims.total, shims::NAMES.len());
        assert_eq!(app.shims.path, shims::default_dir(&app.home));
        assert!(!app.store.root().join("config.json").exists());
        assert!(!app.shims.path.exists());
    }

    #[test]
    fn finishing_setup_save_failure_stays_incomplete_until_a_successful_retry() {
        let (mut app, _d) = with_daemon("finish-setup-failed");
        let path = app.store.root().join("ui.json");
        fs::create_dir(&path).unwrap();
        app.command(r#"{"cmd":"finish_setup"}"#);
        let state = app.state().unwrap();
        assert_eq!(state["setup"]["complete"], false);
        assert!(
            state["setup"]["error"]
                .as_str()
                .unwrap()
                .starts_with("Could not save setup:")
        );
        assert!(!app.store.load_ui_preferences().setup_complete);
        let restarted = App::new(
            app.store.clone(),
            app.home.clone(),
            app.cli.clone(),
            app.exe.clone(),
        );
        assert!(!restarted.ui_preferences.setup_complete);
        fs::remove_dir(&path).unwrap();
        app.command(r#"{"cmd":"finish_setup"}"#);
        assert_eq!(
            app.state().unwrap()["setup"],
            serde_json::json!({"complete": true, "error": null})
        );
        assert!(app.store.load_ui_preferences().setup_complete);
    }

    #[test]
    fn copy_feedback_reports_the_text_and_the_actual_result() {
        let (mut app, _d) = with_daemon("copy-feedback");
        app.record_copy_result(
            "headroom status",
            Err(std::io::Error::other("clipboard unavailable")),
        );
        let state = app.state().unwrap();
        assert_eq!(state["copy_result"]["text"], "headroom status");
        assert_eq!(state["copy_result"]["ok"], false);
        assert!(
            state["copy_result"]["message"]
                .as_str()
                .unwrap()
                .contains("clipboard unavailable")
        );
        app.record_copy_result("headroom doctor", Ok(()));
        let state = app.state().unwrap();
        assert_eq!(state["copy_result"]["text"], "headroom doctor");
        assert_eq!(state["copy_result"]["ok"], true);
        assert_eq!(state["copy_result"]["message"], "Copied to clipboard.");
    }

    #[test]
    fn a_copy_process_that_exits_unsuccessfully_is_a_failure() {
        // A fake clipboard consumer keeps tests away from the user's clipboard.
        let result = copy_with_command(
            "test",
            Command::new("/bin/sh").args(["-c", "cat >/dev/null; exit 7"]),
        );
        assert!(result.unwrap_err().to_string().contains("exit status: 7"));
        assert!(
            copy_with_command(
                "test",
                Command::new("/bin/sh").args(["-c", "cat >/dev/null"])
            )
            .is_ok()
        );
    }

    #[test]
    fn clear_data_persistence_failure_is_visible_and_does_not_claim_success() {
        let (mut app, _d) = with_daemon("clear-data-error");
        std::fs::write(app.store.root().join("profiles.json"), "original bytes").unwrap();
        std::fs::create_dir(app.store.root().join("stats.json")).unwrap();
        app.command(r#"{"cmd":"clear_data"}"#);
        let state = app.state().unwrap();
        assert_eq!(state["data_clear"]["ok"], false);
        assert!(
            state["data_clear"]["message"]
                .as_str()
                .unwrap()
                .contains("non-file")
        );
        assert_eq!(
            std::fs::read_to_string(app.store.root().join("profiles.json")).unwrap(),
            "original bytes"
        );
        assert!(app.last_error.is_some());
    }

    #[test]
    fn hold_reaches_the_daemon_and_comes_back_in_the_state() {
        let (mut app, _d) = with_daemon("hold");
        assert_eq!(app.state().unwrap()["held"], false);
        assert_eq!(app.command(r#"{"cmd":"hold","on":true}"#), Next::Continue);
        assert_eq!(app.state().unwrap()["held"], true);
        assert_eq!(app.last_error, None);
    }

    #[test]
    fn settings_edits_are_persisted_by_the_daemon_and_reread() {
        let (mut app, _d) = with_daemon("settings");
        app.command(r#"{"cmd":"add_pattern","pattern":"bun test"}"#);
        app.command(r#"{"cmd":"set","key":"wait_cap_s","value":120}"#);
        let s = app.state().unwrap();
        assert_eq!(s["settings"]["wait_cap_s"], 120);
        assert!(
            s["settings"]["patterns"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p == "bun test")
        );
        let saved = app.store.load_config();
        assert_eq!(saved.wait_cap_s, 120);
        assert!(saved.patterns.contains(&"bun test".to_string()));
    }

    #[test]
    fn classify_and_dismiss_stay_in_the_app() {
        let (mut app, _d) = with_daemon("local");
        app.command(r#"{"cmd":"classify","text":"pnpm vitest run"}"#);
        assert_eq!(app.state().unwrap()["classify"]["result"], "heavy");
        app.command(r#"{"cmd":"dismiss","id":"k7"}"#);
        assert!(app.dismissed.contains("k7"));
        assert_eq!(app.command(r#"{"cmd":"quit"}"#), Next::Quit);
    }

    #[test]
    fn the_state_says_when_the_queue_runs_in_another_daemon() {
        let (mut app, _d) = with_daemon("attached");
        assert_eq!(app.state().unwrap()["attached"], false);
        app.set_attached(true);
        assert_eq!(app.state().unwrap()["attached"], true);
    }

    #[test]
    fn controls_for_jobs_that_dont_exist_are_reported_not_fatal() {
        let (mut app, _d) = with_daemon("missing");
        assert_eq!(app.command(r#"{"cmd":"kill","id":"999"}"#), Next::Continue);
        assert!(app.last_error.is_some(), "the daemon's refusal is kept");
        assert!(app.state().is_some(), "and the popover still renders");
    }

    #[test]
    fn malformed_intents_are_ignored() {
        let (mut app, _d) = with_daemon("malformed");
        assert_eq!(app.command("not json"), Next::Continue);
        assert!(
            app.last_error
                .as_deref()
                .unwrap()
                .starts_with("bad message")
        );
        assert_eq!(
            app.command(r#"{"cmd":"set","key":"headroom","value":"lots"}"#),
            Next::Continue
        );
        assert_eq!(
            app.state().unwrap()["settings"]["headroom"],
            Config::default().headroom
        );
    }

    #[test]
    fn without_a_daemon_there_is_no_state_and_nothing_breaks() {
        let root = dir("nodaemon");
        let mut app = App::new(
            Store::at(root.join("data")).unwrap(),
            root.join("home"),
            "h".into(),
            "d".into(),
        );
        assert!(!app.poll());
        assert_eq!(app.last_error.as_deref(), Some("headroomd isn't running"));
        assert_eq!(app.command(r#"{"cmd":"hold","on":true}"#), Next::Continue);
        assert!(app.state().is_none());
    }

    #[test]
    fn a_daemon_that_goes_away_leaves_the_last_listing() {
        let (mut app, d) = with_daemon("goes-away");
        app.command(r#"{"cmd":"hold","on":true}"#);
        drop(d);
        assert!(!app.poll());
        assert_eq!(app.state().unwrap()["held"], true);
    }

    #[test]
    fn login_toggles_our_agent_but_leaves_brew_alone() {
        let (mut app, _d) = with_daemon("login");
        app.command(r#"{"cmd":"set","key":"start_at_login","value":true}"#);
        assert_eq!(app.state().unwrap()["settings"]["start_at_login"], true);
        let plist = app.home.join("Library/LaunchAgents/dev.headroom.plist");
        assert!(
            fs::read_to_string(&plist)
                .unwrap()
                .contains("Headroom.app/Contents/MacOS/headroomd")
        );
        app.command(r#"{"cmd":"set","key":"start_at_login","value":false}"#);
        assert!(!plist.exists());

        fs::write(
            app.home
                .join("Library/LaunchAgents/homebrew.mxcl.headroom.plist"),
            "",
        )
        .unwrap();
        app.refresh_settings();
        app.command(r#"{"cmd":"set","key":"start_at_login","value":false}"#);
        let s = app.state().unwrap();
        assert_eq!(s["settings"]["login_managed_by_brew"], true);
        assert_eq!(s["settings"]["start_at_login"], true);
    }

    #[test]
    fn codex_hook_states_do_not_claim_unverified_capture_is_installed() {
        let home = dir("codex-review");
        let cli = "/nonexistent/bin/headroom";
        let path = home.join(".codex/hooks.json");
        hooksettings::install(Agent::Codex, &path, cli, false).unwrap();
        assert_eq!(hook_states(&home, cli)[1], ("codex", "needs_review"));
        let config = home.join(".codex/config.toml");
        fs::write(
            &config,
            format!(
                "[hooks.state.\"{}:pre_tool_use:0:0\"]\ntrusted_hash = \"sha256:old\"\n",
                path.display()
            ),
        )
        .unwrap();
        assert_eq!(hook_states(&home, cli)[1], ("codex", "review_unknown"));
        fs::write(&config, "[features]\nhooks = false\n").unwrap();
        assert_eq!(hook_states(&home, cli)[1], ("codex", "disabled"));

        let alternate = home.join("bin/headroom");
        fs::create_dir_all(alternate.parent().unwrap()).unwrap();
        fs::write(&alternate, "").unwrap();
        hooksettings::install(Agent::Codex, &path, alternate.to_str().unwrap(), false).unwrap();
        fs::write(
            &config,
            format!(
                "[hooks.state.\"{}:pre_tool_use:0:0\"]\ntrusted_hash = \"sha256:old\"\n",
                path.display()
            ),
        )
        .unwrap();
        // An existing alternate binary does not prove the stored approval matches.
        assert_eq!(hook_states(&home, cli)[1], ("codex", "review_unknown"));
        fs::remove_file(alternate).unwrap();
        assert_eq!(hook_states(&home, cli)[1], ("codex", "stale"));
    }

    #[test]
    fn hook_states_cover_each_agent() {
        let home = dir("hooks");
        let cli = "/nonexistent/bin/headroom";
        // Claude: hooked. Codex: present, no hook. pi: not on this machine.
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        hooksettings::install(
            Agent::Claude,
            &home.join(".claude/settings.json"),
            cli,
            false,
        )
        .unwrap();
        assert_eq!(
            hook_states(&home, cli),
            vec![
                ("claude", "installed"),
                ("codex", "not_installed"),
                ("pi", "absent")
            ]
        );
        // The hook names a binary that is gone; one that exists is fine.
        assert_eq!(
            hook_states(&home, "/elsewhere/headroom")[0],
            ("claude", "stale")
        );
        let real = home.join("bin/headroom");
        fs::create_dir_all(home.join("bin")).unwrap();
        fs::write(&real, "").unwrap();
        hooksettings::install(
            Agent::Claude,
            &home.join(".claude/settings.json"),
            real.to_str().unwrap(),
            false,
        )
        .unwrap();
        assert_eq!(hook_states(&home, cli)[0], ("claude", "installed"));
        // Broken JSON is inconclusive, not proof of a missing hook.
        fs::write(home.join(".codex/hooks.json"), "{").unwrap();
        assert_eq!(hook_states(&home, cli)[1], ("codex", "review_unknown"));
    }

    #[test]
    fn unreadable_codex_files_and_invalid_config_remain_unknown() {
        let home = dir("codex-unreadable");
        let cli = "/nonexistent/bin/headroom";
        let path = home.join(".codex/hooks.json");
        hooksettings::install(Agent::Codex, &path, cli, false).unwrap();
        let config = home.join(".codex/config.toml");
        fs::write(&config, "[broken").unwrap();
        assert_eq!(hook_states(&home, cli)[1], ("codex", "review_unknown"));
        fs::remove_file(&config).unwrap();
        fs::create_dir(&config).unwrap();
        assert_eq!(hook_states(&home, cli)[1], ("codex", "review_unknown"));
        fs::remove_dir(&config).unwrap();
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert_eq!(hook_states(&home, cli)[1], ("codex", "review_unknown"));
        fs::remove_dir(&path).unwrap();
        assert_eq!(hook_states(&home, cli)[1], ("codex", "not_installed"));
    }
}
