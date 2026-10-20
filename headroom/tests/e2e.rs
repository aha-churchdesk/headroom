//! End to end: a real daemon, the real `headroom` binary, real processes.
//!
//! Only the machine's memory readings are scripted (the probe below), so the
//! queue and the guard can be driven deterministically on a busy laptop or a
//! CI runner. Process groups, signals and footprints are all real.

use std::collections::HashMap;
use std::io::{BufReader, Read};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use headroom_core::config::Config;
use headroom_core::model::{GIB, JobState, Outcome, Pressure, Snapshot};
use headroom_core::protocol::{ClientMsg, DaemonMsg, Listing, read_msg, write_msg};
use headroom_core::scheduler::Usage;
use headroom_core::store::Store;
use headroomd::daemon::{self, Probe};
use headroomd::sample::Sampler;

const BIN: &str = env!("CARGO_BIN_EXE_headroom");
const HOG: &str = env!("CARGO_BIN_EXE_headroom-hog");

/// Real per-process measurements, scripted system readings.
struct Scripted {
    real: Sampler,
    pressure: Arc<Mutex<Pressure>>,
}

impl Probe for Scripted {
    fn system(&mut self) -> Snapshot {
        Snapshot {
            pressure: *self.pressure.lock().unwrap(),
            mem_total: 64 * GIB,
            mem_used: 16 * GIB,
            mem_available: 48 * GIB,
            swap_used: 0,
            load1: 0.0,
            perf_cores: 8,
            cpu_pct: 0.0,
        }
    }
    fn groups(&mut self, pgids: &[i32]) -> HashMap<i32, Usage> {
        self.real.groups(pgids)
    }
}

struct Daemon {
    home: PathBuf,
    stop: Arc<AtomicBool>,
    pressure: Arc<Mutex<Pressure>>,
    thread: Option<JoinHandle<()>>,
}

fn tmp(name: &str) -> PathBuf {
    // Short: a Unix socket path must fit in 104 bytes.
    let p = std::env::temp_dir().join(format!("hr-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn wait_for<T>(what: &str, secs: u64, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

impl Daemon {
    fn start(name: &str, cfg: Config) -> Self {
        let home = tmp(name);
        let store = Store::at(&home).unwrap();
        store.save_config(&cfg).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let pressure = Arc::new(Mutex::new(Pressure::Normal));
        let probe = Scripted {
            real: Sampler::new(),
            pressure: pressure.clone(),
        };
        let s = stop.clone();
        let thread =
            std::thread::spawn(move || daemon::run_with(store, s, Box::new(probe)).unwrap());
        let sock = home.join("headroomd.sock");
        wait_for("socket", 5, || sock.exists().then_some(()));
        Daemon {
            home,
            stop,
            pressure,
            thread: Some(thread),
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .env("HEADROOM_HOME", &self.home)
            .env_remove("HEADROOM_JOB");
        c
    }

    fn output(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn spawn(&self, args: &[&str]) -> Child {
        self.cmd(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn ls(&self) -> Listing {
        let mut s = UnixStream::connect(self.home.join("headroomd.sock")).unwrap();
        write_msg(&mut s, &ClientMsg::List).unwrap();
        match read_msg::<_, DaemonMsg>(&mut BufReader::new(s)).unwrap() {
            Some(DaemonMsg::Jobs(l)) => l,
            other => panic!("unexpected {other:?}"),
        }
    }

    fn set_pressure(&self, p: Pressure) {
        *self.pressure.lock().unwrap() = p;
    }

    /// The single running job's (id, pgid), once it has started.
    fn running(&self) -> (u64, i32) {
        wait_for("a running job", 10, || {
            self.ls()
                .running
                .first()
                .and_then(|j| j.pgid.map(|p| (j.id, p)))
        })
    }

    fn queued_id(&self) -> u64 {
        wait_for("a queued job", 10, || {
            self.ls().queued.first().map(|j| j.id)
        })
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn finish(mut c: Child) -> (i32, String, String) {
    let status = c.wait().unwrap();
    let mut out = String::new();
    let mut err = String::new();
    c.stdout.take().unwrap().read_to_string(&mut out).unwrap();
    c.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    (status.code().unwrap_or(-1), out, err)
}

fn group_alive(pgid: i32) -> bool {
    unsafe { libc::killpg(pgid, 0) == 0 }
}

fn quick() -> Config {
    Config {
        freeze_after_s: 1,
        kill_after_s: 1,
        kill_grace_s: 1,
        thaw_after_s: 600,
        ..Config::default()
    }
}

#[test]
fn runs_a_command_and_learns_from_it() {
    let d = Daemon::start("learn", Config::default());
    let o = d.output(&["run", "--", "/bin/sh", "-c", "echo hi"]);
    assert_eq!(
        (o.status.code(), text(&o.stdout).as_str()),
        (Some(0), "hi\n")
    );
    let h = wait_for("history", 5, || d.ls().history.first().cloned());
    assert_eq!(h.outcome, Outcome::Exited);
    assert_eq!(h.cmd, "sh");
    let profiles = std::fs::read_to_string(d.home.join("profiles.json")).unwrap();
    assert!(profiles.contains("|sh\""), "{profiles}");
}

#[test]
fn exit_codes_and_signals_pass_through() {
    let d = Daemon::start("codes", Config::default());
    assert_eq!(d.output(&["run", "--sh", "exit 3"]).status.code(), Some(3));
    assert_eq!(
        d.output(&["run", "--sh", "kill -9 $$"]).status.code(),
        Some(137)
    );
}

#[test]
fn without_a_daemon_the_command_runs_directly() {
    let home = tmp("nodaemon");
    let o = Command::new(BIN)
        .args(["run", "--sh", "echo direct; exit 4"])
        .env("HEADROOM_HOME", &home)
        .env_remove("HEADROOM_JOB")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(4));
    assert_eq!(text(&o.stdout), "direct\n");
    assert!(text(&o.stderr).contains("isn't running; running directly"));
}

#[test]
fn the_wait_cap_gives_up_with_75_and_nothing_runs() {
    let d = Daemon::start("cap", Config::default());
    assert!(d.output(&["hold", "on"]).status.success());
    let marker = d.home.join("ran");
    let o = d.output(&[
        "run",
        "--wait-cap",
        "1",
        "--sh",
        &format!("touch {}", marker.display()),
    ]);
    assert_eq!(o.status.code(), Some(75));
    let err = text(&o.stderr);
    assert!(
        err.contains("still queued at #1")
            && err.contains("held by you")
            && err.contains("Nothing ran"),
        "{err}"
    );
    assert!(!marker.exists());
    // The abandoned place is gone from the queue.
    wait_for("empty queue", 5, || d.ls().queued.is_empty().then_some(()));
}

#[test]
fn a_held_job_starts_when_the_hold_is_released() {
    let d = Daemon::start("hold", Config::default());
    d.output(&["hold", "on"]);
    let c = d.spawn(&["run", "--background", "--sh", "echo started"]);
    d.queued_id();
    // Long enough in the queue to be worth mentioning when it starts.
    std::thread::sleep(Duration::from_millis(1200));
    assert!(d.output(&["hold", "off"]).status.success());
    let (code, out, err) = finish(c);
    assert_eq!((code, out.as_str()), (0, "started\n"));
    assert!(err.contains("queued #1 of 1 · held by you"), "{err}");
    assert!(err.contains("starting after"), "{err}");
}

#[test]
fn cancel_ends_a_queued_job_before_it_runs() {
    let d = Daemon::start("cancel", Config::default());
    d.output(&["hold", "on"]);
    let marker = d.home.join("ran");
    let c = d.spawn(&[
        "run",
        "--background",
        "--sh",
        &format!("touch {}", marker.display()),
    ]);
    let id = d.queued_id();
    assert!(d.output(&["cancel", &id.to_string()]).status.success());
    let (code, _, err) = finish(c);
    assert_eq!(code, 130);
    assert!(err.contains("cancelled"), "{err}");
    assert!(!marker.exists());
    // Cancelling it again finds nothing.
    let again = d.output(&["cancel", &id.to_string()]);
    assert_eq!(again.status.code(), Some(1));
    assert!(text(&again.stderr).contains("no job"));
}

#[test]
fn a_wrapper_that_dies_takes_its_job_with_it() {
    let d = Daemon::start("drop", Config::default());
    let mut c = d.spawn(&["run", "--sh", "sleep 60"]);
    let (_, pgid) = d.running();
    assert!(group_alive(pgid));
    c.kill().unwrap(); // SIGKILL: no chance to clean up
    c.wait().unwrap();
    wait_for("job group to end", 15, || {
        (!group_alive(pgid)).then_some(())
    });
    let h = wait_for("history", 5, || d.ls().history.first().cloned());
    assert_eq!(h.outcome, Outcome::Disconnected);
}

#[test]
fn a_nested_run_goes_straight_through() {
    let d = Daemon::start("nested", Config::default());
    // The outer job holds the queue, so a nested run that queued would wait forever.
    let line = format!("{BIN} hold on && {BIN} run --wait-cap 2 --sh 'echo inner'");
    let o = d.output(&["run", "--sh", &line]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert_eq!(text(&o.stdout), "inner\n");
}

#[test]
fn manual_kill_ends_the_job_without_requeueing() {
    let d = Daemon::start("kill", Config::default());
    let c = d.spawn(&["run", "--sh", "sleep 60"]);
    let (id, _) = d.running();
    assert!(d.output(&["kill", &id.to_string()]).status.success());
    let (code, _, err) = finish(c);
    // SIGTERM to the shell: 128 + 15.
    assert_eq!(code, 143);
    assert!(err.contains("killed from the queue"), "{err}");
    let h = wait_for("history", 5, || d.ls().history.first().cloned());
    assert_eq!(h.outcome, Outcome::ManualKill);
    assert!(d.ls().queued.is_empty());
}

#[test]
fn freeze_and_resume_stop_and_continue_the_real_process() {
    let d = Daemon::start("freeze", Config::default());
    let c = d.spawn(&["run", "--", "/bin/sleep", "60"]);
    let (id, pgid) = d.running();
    let stat = || {
        let o = Command::new("ps")
            .args(["-o", "stat=", "-p", &pgid.to_string()])
            .output()
            .unwrap();
        text(&o.stdout).trim().to_string()
    };
    assert!(d.output(&["freeze", &id.to_string()]).status.success());
    wait_for("stopped", 5, || stat().starts_with('T').then_some(()));
    assert_eq!(d.ls().running[0].state, JobState::Frozen);
    assert!(d.output(&["resume", &id.to_string()]).status.success());
    wait_for("running again", 5, || {
        (!stat().starts_with('T')).then_some(())
    });
    d.output(&["kill", &id.to_string()]);
    let (_, _, err) = finish(c);
    assert!(err.contains("paused") && err.contains("resumed"), "{err}");
}

#[test]
fn the_guard_freezes_kills_requeues_and_finally_gives_up() {
    let d = Daemon::start("guard", quick());
    let c = d.spawn(&["run", "--", HOG, "0.2", "120"]);
    let (_, first) = d.running();

    // Round one: critical pressure. Frozen, then killed and requeued.
    d.set_pressure(Pressure::Critical);
    wait_for("requeue", 20, || {
        d.ls()
            .history
            .iter()
            .any(|h| h.outcome == Outcome::Requeued)
            .then_some(())
    });
    wait_for("first group gone", 10, || {
        (!group_alive(first)).then_some(())
    });
    assert_eq!(
        d.ls().queued.first().map(|j| j.retry),
        Some(1),
        "back in line, marked as a retry"
    );

    // Room again: it reruns, on the same wrapper.
    d.set_pressure(Pressure::Normal);
    let second = wait_for("rerun", 10, || {
        d.ls()
            .running
            .first()
            .and_then(|j| j.pgid)
            .filter(|&p| p != first)
    });

    // Round two: killed again, and this time the wrapper gives up.
    d.set_pressure(Pressure::Critical);
    let (code, _, err) = finish(c);
    assert_eq!(code, 137);
    assert!(err.contains("paused"), "{err}");
    assert!(err.contains("requeued, will run again"), "{err}");
    assert!(err.contains("gave up"), "{err}");
    assert!(!group_alive(second));
    let outcomes: Vec<Outcome> = d.ls().history.iter().map(|h| h.outcome.clone()).collect();
    assert_eq!(outcomes, vec![Outcome::GaveUp, Outcome::Requeued]);

    // The killed runs taught the profile that this command needs more.
    let profiles = std::fs::read_to_string(d.home.join("profiles.json")).unwrap();
    assert!(profiles.contains("\"flagged\": true"), "{profiles}");
}

#[test]
fn ls_shows_the_queue_and_fails_cleanly_without_a_daemon() {
    let d = Daemon::start("ls", Config::default());
    let c = d.spawn(&["run", "--label", "sc-1", "--", "/bin/sleep", "30"]);
    let (id, _) = d.running();
    let o = d.output(&["ls"]);
    let out = text(&o.stdout);
    assert!(
        out.contains("running  1") && out.contains("sc-1") && out.contains("sleep"),
        "{out}"
    );
    d.output(&["kill", &id.to_string()]);
    finish(c);

    let o = Command::new(BIN)
        .arg("ls")
        .env("HEADROOM_HOME", tmp("ls-none"))
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("isn't running"));
}

fn hook(agent: &str, payload: &str, home: &Path) -> Output {
    let mut c = Command::new(BIN)
        .args(["hook", "--agent", agent])
        .env("HEADROOM_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    c.stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    c.wait_with_output().unwrap()
}

#[test]
fn the_claude_hook_wraps_heavy_commands_and_ignores_the_rest() {
    let home = tmp("hook");
    let p = r#"{"session_id":"s1","tool_name":"Bash","tool_input":{"command":"pnpm test","description":"d"}}"#;
    let o = hook("claude", p, &home);
    assert!(o.status.success());
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let cmd = v["hookSpecificOutput"]["updatedInput"]["command"]
        .as_str()
        .unwrap();
    assert!(
        cmd.starts_with(&format!("{BIN} run --agent claude --session s1")),
        "{cmd}"
    );
    assert_eq!(v["hookSpecificOutput"]["updatedInput"]["description"], "d");

    let light = r#"{"tool_name":"Bash","tool_input":{"command":"git status"}}"#;
    let o = hook("claude", light, &home);
    assert!(o.status.success() && o.stdout.is_empty());

    // Garbage still exits 0 with no output: the agent's command goes ahead.
    let o = hook("codex", "not json at all", &home);
    assert!(o.status.success() && o.stdout.is_empty());
}

#[test]
fn hook_install_status_and_uninstall_against_a_scratch_home() {
    let home = tmp("install");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(
        home.join(".claude/settings.json"),
        "{\n  \"model\": \"opus\"\n}\n",
    )
    .unwrap();
    let run = |args: &[&str]| {
        let o = Command::new(BIN)
            .args(args)
            .env("HOME", &home)
            .env("HEADROOM_HOME", &home)
            .output()
            .unwrap();
        (o.status.code().unwrap(), text(&o.stdout))
    };

    let (code, out) = run(&["hook", "install", "--dry-run"]);
    assert_eq!(code, 0);
    assert!(
        out.contains("claude would install")
            && out.contains("+ ")
            && out.contains("codex  not found"),
        "{out}"
    );
    assert!(
        !std::fs::read_to_string(home.join(".claude/settings.json"))
            .unwrap()
            .contains("hook --agent")
    );

    let (code, out) = run(&["hook", "install"]);
    assert_eq!(code, 0);
    assert!(
        out.contains("claude installed") && out.contains("~/.claude/settings.json"),
        "{out}"
    );
    let settings = std::fs::read_to_string(home.join(".claude/settings.json")).unwrap();
    assert!(
        settings.contains("\"model\": \"opus\"")
            && settings.contains(&format!("{BIN} hook --agent claude"))
    );

    let (_, out) = run(&["hook", "status", "--agent", "claude"]);
    assert!(out.contains("installed"), "{out}");
    let (_, out) = run(&["hook", "install", "--agent", "claude"]);
    assert!(out.contains("unchanged"), "{out}");

    let (code, out) = run(&["hook", "uninstall", "--agent", "claude"]);
    assert_eq!(code, 0);
    assert!(out.contains("removed"), "{out}");
    assert_eq!(
        std::fs::read_to_string(home.join(".claude/settings.json")).unwrap(),
        "{\n  \"model\": \"opus\"\n}\n"
    );
}

#[test]
fn a_broken_settings_file_is_reported_and_left_alone() {
    let home = tmp("broken");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(home.join(".claude/settings.json"), "{ oops").unwrap();
    let o = Command::new(BIN)
        .args(["hook", "install", "--agent", "claude"])
        .env("HOME", &home)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stdout).contains("not valid JSON"));
    assert_eq!(
        std::fs::read_to_string(home.join(".claude/settings.json")).unwrap(),
        "{ oops"
    );
}

#[test]
fn the_mcp_server_reports_and_reorders_the_real_queue() {
    use std::io::Write;
    let d = Daemon::start("mcp", Config::default());
    d.output(&["hold", "on"]);
    let a = d.spawn(&["run", "--background", "--label", "first", "--sh", "true"]);
    d.queued_id();
    let b = d.spawn(&["run", "--background", "--label", "second", "--sh", "true"]);
    wait_for("two queued", 5, || (d.ls().queued.len() == 2).then_some(()));
    let second = d.ls().queued[1].id;

    let call = |id: u32, name: &str, args: &str| {
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#
        )
    };
    let input = [
        r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#.to_string(),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string(),
        call(1, "system_status", "{}"),
        call(2, "move_job", &format!(r#"{{"id":{second}}}"#)),
        call(3, "pin_job", &format!(r#"{{"id":{second},"on":true}}"#)),
        call(4, "pin_job", r#"{"id":999}"#),
    ]
    .join("\n")
        + "\n";
    let mut child = d
        .cmd(&["mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let answers: Vec<serde_json::Value> = text(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        answers.len(),
        5,
        "one per request, none for the notification"
    );

    let status = &answers[1]["result"]["structuredContent"];
    assert_eq!(
        (status["queued"].clone(), status["held"].clone()),
        (2.into(), true.into())
    );
    assert_eq!(status["waiting_because"], "held by you");

    let moved = &answers[2]["result"]["structuredContent"]["jobs"];
    assert_eq!(moved[0]["label"], "second", "moved to the top");
    assert_eq!(
        answers[3]["result"]["structuredContent"]["jobs"][0]["pinned"],
        true
    );
    assert_eq!(d.ls().queued[0].id, second);
    assert!(d.ls().queued[0].pinned);

    assert_eq!(answers[4]["result"]["isError"], true);
    assert!(
        answers[4]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("999")
    );

    d.output(&["hold", "off"]);
    for c in [a, b] {
        assert_eq!(finish(c).0, 0);
    }
}

#[test]
fn a_job_that_does_not_fit_runs_with_fewer_workers() {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
    let default = cores - 1;
    // Stands in for vitest: says what it was told, then takes real memory.
    let bin = tmp("tune-bin");
    let vitest = bin.join("vitest");
    std::fs::write(
        &vitest,
        format!("#!/bin/sh\necho \"workers=${{VITEST_MAX_WORKERS:-unset}}\"\nexec {HOG} 0.5 1\n"),
    )
    .unwrap();
    std::fs::set_permissions(&vitest, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let line = format!("{} run", vitest.display());

    // 48 GB available, 1.5 GB reserved by the job blocking the queue, 0.4 GB left.
    let d = Daemon::start(
        "tune",
        Config {
            headroom: 48 * GIB - 3 * GIB / 2 - 2 * GIB / 5,
            tune_floor: 1,
            ..Config::default()
        },
    );
    let run = |args: &[&str]| {
        d.cmd(args)
            .env_remove("VITEST_MAX_WORKERS")
            .output()
            .unwrap()
    };

    // First run: the queue is idle, so it starts untouched and is profiled
    // at the tool's default worker count.
    let first = run(&["run", "--sh", &line]);
    assert_eq!(text(&first.stdout).lines().next(), Some("workers=unset"));

    // Second run, behind a running job: ~1.2 GB estimated, 0.4 GB free.
    let blocker = d.spawn(&["run", "--background", "--sh", "sleep 30"]);
    d.running();
    let second = run(&["run", "--sh", &line]);
    let (out, err) = (text(&second.stdout), text(&second.stderr));
    assert_eq!(second.status.code(), Some(0), "{err}");
    let n: u32 = out
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("workers="))
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("not tuned: {out} {err}"));
    assert!((1..default).contains(&n), "{n} of {default}");
    assert!(
        err.contains(&format!(
            "starting with {n} vitest workers instead of {default}"
        )),
        "{err}"
    );
    let h = wait_for("history", 5, || {
        d.ls().history.into_iter().find(|h| h.tuned.is_some())
    });
    assert_eq!(h.tuned.map(|t| (t.workers, t.default)), Some((n, default)));

    d.output(&["kill", &d.ls().running[0].id.to_string()]);
    finish(blocker);
}

/// Writes the popover states the node UI tests render, from real runs.
/// Not part of the suite: rerun it when the state contract changes, with
/// `cargo test -p headroom --test e2e capture_ui_states -- --ignored`.
#[test]
#[ignore]
fn capture_ui_states() {
    use headroom_core::hookio::Agent;
    use headroom_core::hooksettings;
    use headroomd::app::App;
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/ui/fixtures");
    std::fs::create_dir_all(&out).unwrap();
    let save = |app: &mut App, name: &str| {
        app.refresh_settings();
        app.refresh_hooks();
        assert!(app.poll(), "daemon answered");
        let s = app.state().unwrap();
        std::fs::write(
            out.join(format!("{name}.json")),
            // The fixtures are checked in: no home directory in them.
            serde_json::to_string_pretty(&s)
                .unwrap()
                .replace(&std::env::var("HOME").unwrap(), "~")
                + "\n",
        )
        .unwrap();
    };
    // Claude hooked, Codex present without a hook, no pi; optionally
    // started at login by `brew services`.
    let app_for = |d: &Daemon, name: &str, brew: bool| {
        let home = tmp(&format!("{name}-home"));
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        hooksettings::install(
            Agent::Claude,
            &home.join(".claude/settings.json"),
            BIN,
            false,
        )
        .unwrap();
        if brew {
            let agents = home.join("Library/LaunchAgents");
            std::fs::create_dir_all(&agents).unwrap();
            std::fs::write(agents.join("homebrew.mxcl.headroom.plist"), "").unwrap();
        }
        App::new(
            Store::at(&d.home).unwrap(),
            home,
            BIN.into(),
            "/Applications/Headroom.app/Contents/MacOS/headroomd".into(),
        )
    };

    // A full machine: one job running, two waiting on memory, one of them
    // pinned and one low.
    let d = Daemon::start(
        "ui-busy",
        Config {
            headroom: 47 * GIB,
            ..Config::default()
        },
    );
    let mut app = app_for(&d, "ui-busy", false);
    let a = d.spawn(&[
        "run",
        "--background",
        "--label",
        "sc-49123",
        "--",
        HOG,
        "0.3",
        "60",
    ]);
    let (_, _) = d.running();
    let b = d.spawn(&[
        "run",
        "--background",
        "--low",
        "--label",
        "fix-main",
        "--sh",
        "sleep 30",
    ]);
    wait_for("two queued", 5, || (d.ls().queued.len() == 1).then_some(()));
    let c = d.spawn(&[
        "run",
        "--background",
        "--label",
        "browser-cd",
        "--sh",
        "sleep 31",
    ]);
    wait_for("two queued", 5, || (d.ls().queued.len() == 2).then_some(()));
    let pin = d.ls().queued[1].id.to_string();
    d.output(&["pin", &pin]);
    let e = d.spawn(&[
        "run",
        "--background",
        "--label",
        "form-service",
        "--sh",
        "sleep 32",
    ]);
    wait_for("two queued", 5, || (d.ls().queued.len() == 2).then_some(()));
    // A few samples, so the running job has a footprint.
    std::thread::sleep(Duration::from_millis(2500));
    save(&mut app, "busy");
    d.output(&["hold", "on"]);
    save(&mut app, "held");
    for j in d.ls().queued {
        d.output(&["cancel", &j.id.to_string()]);
    }
    for j in d.ls().running {
        d.output(&["kill", &j.id.to_string()]);
    }
    for ch in [a, b, c, e] {
        finish(ch);
    }
    drop(d);

    // History of every kind of ending, then the guard at work.
    let d = Daemon::start("ui-guard", quick());
    let mut app = app_for(&d, "ui-guard", true);
    d.output(&["run", "--label", "fix-main", "--sh", "exit 3"]);
    d.output(&["run", "--label", "fix-main", "--sh", "echo ok"]);
    let mut w = d.spawn(&["run", "--label", "sc-48870", "--sh", "sleep 60"]);
    d.running();
    w.kill().unwrap();
    w.wait().unwrap();
    wait_for("disconnected", 15, || {
        d.ls()
            .history
            .iter()
            .any(|h| h.outcome == Outcome::Disconnected)
            .then_some(())
    });
    let c = d.spawn(&["run", "--label", "sc-49201", "--", HOG, "0.2", "120"]);
    d.running();
    d.set_pressure(Pressure::Critical);
    wait_for("guard freeze", 10, || {
        d.ls()
            .running
            .first()
            .filter(|j| j.state == JobState::Frozen)
            .map(|_| ())
    });
    save(&mut app, "frozen");
    wait_for("requeue", 20, || {
        d.ls()
            .history
            .iter()
            .any(|h| h.outcome == Outcome::Requeued)
            .then_some(())
    });
    d.set_pressure(Pressure::Normal);
    wait_for("rerun", 10, || {
        d.ls().running.first().and_then(|j| j.pgid).map(|_| ())
    });
    d.set_pressure(Pressure::Critical);
    let (code, _, _) = finish(c);
    assert_eq!(code, 137);
    d.set_pressure(Pressure::Normal);
    std::thread::sleep(Duration::from_millis(1500));
    save(&mut app, "history");
    drop(d);

    // A job started with fewer workers, as in
    // a_job_that_does_not_fit_runs_with_fewer_workers.
    let bin = tmp("ui-tune-bin");
    let vitest = bin.join("vitest");
    std::fs::write(&vitest, format!("#!/bin/sh\nexec {HOG} 0.5 4\n")).unwrap();
    std::fs::set_permissions(&vitest, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let line = format!("{} run", vitest.display());
    let d = Daemon::start(
        "ui-tune",
        Config {
            headroom: 48 * GIB - 3 * GIB / 2 - 2 * GIB / 5,
            tune_floor: 1,
            ..Config::default()
        },
    );
    let mut app = app_for(&d, "ui-tune", false);
    d.cmd(&["run", "--label", "sc-49123", "--sh", &line])
        .env_remove("VITEST_MAX_WORKERS")
        .output()
        .unwrap();
    let blocker = d.spawn(&[
        "run",
        "--background",
        "--label",
        "fix-main",
        "--sh",
        "sleep 30",
    ]);
    d.running();
    let tuned = d.spawn(&["run", "--label", "sc-49123", "--sh", &line]);
    wait_for("tuned job", 10, || {
        d.ls()
            .running
            .iter()
            .any(|j| j.tuned.is_some() && j.footprint > 0)
            .then_some(())
    });
    save(&mut app, "tuned");
    finish(tuned);
    // The wrapper exits as soon as it has reported; the daemon may not have
    // recorded the run yet.
    wait_for("tuned run in history", 10, || {
        d.ls()
            .history
            .iter()
            .any(|h| h.tuned.is_some())
            .then_some(())
    });
    save(&mut app, "tuned-history");
    d.output(&["kill", &d.ls().running[0].id.to_string()]);
    finish(blocker);
    drop(d);

    // Backfill: a 1 GB, 3 s job runs; another waits on memory; a quick one
    // starts ahead of it. Each has three runs behind it first.
    let bin = tmp("ui-backfill-bin");
    let script = |name: &str, body: String| {
        let p = bin.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        p.display().to_string()
    };
    let big = script("e2e.sh", format!("exec {HOG} 1 3"));
    let quick = script("lint.sh", "true".into());
    let d = Daemon::start(
        "ui-backfill",
        Config {
            headroom: 47 * GIB + GIB / 2,
            ..Config::default()
        },
    );
    let mut app = app_for(&d, "ui-backfill", false);
    for _ in 0..3 {
        d.output(&["run", "--", &big]);
        d.output(&["run", "--", &quick]);
    }
    let running = d.spawn(&["run", "--label", "sc-49123", "--", &big]);
    d.running();
    let head = d.spawn(&["run", "--label", "browser-cd", "--", &big]);
    d.queued_id();
    let ahead = d.spawn(&["run", "--label", "fix-main", "--", &quick]);
    wait_for("the quick job to run ahead", 5, || {
        d.ls()
            .history
            .iter()
            .any(|h| h.ahead_of.is_some())
            .then_some(())
    });
    save(&mut app, "backfill");
    for c in [running, head, ahead] {
        finish(c);
    }

    // Other apps: a process Headroom didn't start, from a shell, in a
    // project directory. Only it is listed, so nothing else on this machine
    // ends up in the fixture.
    let project = tmp("ui-others-project");
    let mut outside = Command::new("/bin/sh")
        .args(["-c", &format!("{HOG} 0.3 30; true")])
        .current_dir(&project)
        .spawn()
        .unwrap();
    let hog = wait_for("the hog under its shell", 5, || {
        let out = Command::new("pgrep")
            .args(["-P", &outside.id().to_string()])
            .output()
            .ok()?;
        text(&out.stdout).trim().parse::<i32>().ok()
    });
    std::thread::sleep(Duration::from_millis(500));
    app.refresh_others_among(&[outside.id() as i32, hog]);
    save(&mut app, "others");
    outside.kill().unwrap();
    unsafe { libc::kill(hog, libc::SIGKILL) };
    outside.wait().unwrap();
}
