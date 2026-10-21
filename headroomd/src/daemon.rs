//! The daemon's main loop: carry out what the scheduler decides.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use headroom_core::model::{JobId, Snapshot};
use headroom_core::proctree;
use headroom_core::protocol::{ClientMsg, DaemonMsg};
use headroom_core::scheduler::{Effect, Scheduler, Sig, Usage};
use headroom_core::store::Store;

use crate::sample::{self, ProcessTracker, Sampler};
use crate::server::{self, Conns, Event};
use crate::single;

pub const TICK: Duration = Duration::from_secs(1);

fn wall() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn signal(pgid: i32, sig: Sig, processes: &mut ProcessTracker) {
    let s = match sig {
        Sig::Stop => libc::SIGSTOP,
        Sig::Cont => libc::SIGCONT,
        Sig::Term => libc::SIGTERM,
        Sig::Kill => libc::SIGKILL,
        Sig::Int => libc::SIGINT,
        Sig::Hup => libc::SIGHUP,
    };
    let groups = processes.groups(&sample::procs(), pgid);
    // Never a pgid of 0 or 1, nor our own: killpg(0) would hit our group.
    let own = unsafe { libc::getpgid(0) };
    for g in groups {
        if g > 1 && g != own {
            unsafe { libc::killpg(g, s) };
        }
    }
}

/// Where readings come from. The daemon uses the real [`Sampler`]; tests
/// wrap it to script memory pressure while still measuring real processes.
pub trait Probe: Send {
    fn system(&mut self) -> Snapshot;
    fn groups(&mut self, pgids: &[i32]) -> HashMap<i32, Usage>;
}

impl Probe for Sampler {
    fn system(&mut self) -> Snapshot {
        Sampler::system(self)
    }
    fn groups(&mut self, pgids: &[i32]) -> HashMap<i32, Usage> {
        Sampler::groups(self, pgids)
    }
}

/// Run until `stop` is set. Refuses to start beside another daemon on the
/// same store, which would admit jobs against the same memory twice.
pub fn run(store: Store, stop: Arc<AtomicBool>) -> io::Result<()> {
    run_with(store, stop, Box::new(Sampler::new()))
}

pub fn run_with(
    store: Store,
    stop: Arc<AtomicBool>,
    mut sampler: Box<dyn Probe>,
) -> io::Result<()> {
    let Some(_lock) = single::acquire(store.root()) else {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "headroomd is already running",
        ));
    };
    // Safe to remove: holding the lock means no live daemon owns it.
    let sock = store.socket();
    let _ = fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;
    fs::set_permissions(&sock, fs::Permissions::from_mode(0o600))?;

    let mut sched = Scheduler::new(
        store.load_config(),
        store.load_profiles(),
        store.load_history(),
    );
    sched.tally = store.load_tally();
    let start = Instant::now();
    let ms = || start.elapsed().as_millis() as u64;

    // A first reading before any client arrives, so the first job is
    // admitted against real numbers.
    let fx = sched.tick(sampler.system(), &HashMap::new(), 0);
    let conns: Conns = Arc::new(Mutex::new(HashMap::new()));
    let mut processes = ProcessTracker::default();
    apply(&store, &sched, &conns, &mut processes, fx);

    let (tx, rx) = mpsc::channel();
    let server = server::spawn(listener, tx, conns.clone())?;
    let mut next_tick = Instant::now() + TICK;

    while !stop.load(Ordering::Relaxed) {
        // A busy socket must not postpone sampling or owed SIGKILLs.
        if Instant::now() >= next_tick {
            next_tick = Instant::now() + TICK;
            let pgids = sched.pgids();
            processes.retain(&pgids);
            let table = if pgids.is_empty() {
                vec![]
            } else {
                sample::procs()
            };
            let mut tracked = HashMap::new();
            for pgid in &pgids {
                let pids = processes.pids(&table, *pgid);
                if !pids.is_empty() {
                    tracked.insert(
                        *pgid,
                        Usage {
                            procs: pids.len() as u32,
                            groups: table
                                .iter()
                                .filter(|p| pids.contains(&p.pid))
                                .map(|p| p.pgid)
                                .collect(),
                            ..Usage::default()
                        },
                    );
                }
            }
            let snap = sampler.system();
            let mut usage = sampler.groups(&pgids);
            // TERM can orphan a subgroup before the first memory sample.
            // Signal-time identities still establish that it needs SIGKILL.
            for (pgid, live) in tracked {
                usage.entry(pgid).or_insert(live);
            }
            let fx = sched.tick(snap, &usage, ms());
            apply(&store, &sched, &conns, &mut processes, fx);
        }
        let wait = next_tick
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(200));
        let fx = match rx.recv_timeout(wait) {
            Ok(Event::Msg(c, msg)) => match *msg {
                ClientMsg::Enqueue(spec) => match spec.pid.and_then(|p| enclosing(&sched, p)) {
                    Some(id) => vec![Effect::Send(c, DaemonMsg::Inside { id })],
                    None => sched.enqueue(c, spec, ms()),
                },
                ClientMsg::Started { pgid, .. } => sched.started(c, pgid),
                ClientMsg::Signal { signal } => match signal {
                    libc::SIGINT => sched.signalled(c, Sig::Int, ms()),
                    libc::SIGTERM => sched.signalled(c, Sig::Term, ms()),
                    libc::SIGHUP => sched.signalled(c, Sig::Hup, ms()),
                    _ => vec![],
                },
                ClientMsg::TerminalStopped { stopped } => sched.terminal_stopped(c, stopped, ms()),
                ClientMsg::Exited { code, signal } => {
                    let now = ms();
                    let mut fx = match signal {
                        Some(libc::SIGINT) => sched.child_signalled(c, Sig::Int, now),
                        Some(libc::SIGTERM) => sched.child_signalled(c, Sig::Term, now),
                        Some(libc::SIGHUP) => sched.child_signalled(c, Sig::Hup, now),
                        _ => vec![],
                    };
                    fx.extend(sched.exited(c, code, signal, now, wall()));
                    fx
                }
                ClientMsg::List => vec![Effect::Send(c, DaemonMsg::Jobs(sched.list()))],
                ClientMsg::Control(ctl) => sched.control(c, ctl, ms(), wall()),
            },
            Ok(Event::Closed(c)) => {
                conns.lock().unwrap_or_else(|p| p.into_inner()).remove(&c);
                sched.disconnected(c, ms(), wall())
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        apply(&store, &sched, &conns, &mut processes, fx);
    }
    // Release daemon pauses. Shell Ctrl-Z stays paused until shell fg/bg.
    for pgid in sched.shutdown_thaws() {
        signal(pgid, Sig::Cont, &mut processes);
    }
    drop(server);
    let _ = fs::remove_file(&sock);
    Ok(())
}

/// The running job whose processes include `pid`.
fn enclosing(sched: &Scheduler, pid: i32) -> Option<JobId> {
    let jobs: Vec<(JobId, i32)> = sched
        .list()
        .running
        .iter()
        .filter_map(|j| j.pgid.map(|g| (j.id, g)))
        .collect();
    if jobs.is_empty() {
        return None;
    }
    proctree::job_of(&sample::procs(), &jobs, pid)
}

fn apply(
    store: &Store,
    sched: &Scheduler,
    conns: &Conns,
    processes: &mut ProcessTracker,
    fx: Vec<Effect>,
) {
    let (mut profiles, mut history, mut config) = (false, false, false);
    for e in fx {
        match e {
            Effect::Send(c, m) => server::send(conns, c, &m),
            Effect::Signal { pgid, sig } => signal(pgid, sig, processes),
            Effect::ContinueWrapper { pid } => {
                if pid > 1 {
                    unsafe { libc::kill(pid, libc::SIGCONT) };
                }
            }
            Effect::SaveProfiles => profiles = true,
            Effect::SaveHistory => history = true,
            Effect::SaveConfig => config = true,
        }
    }
    // Losing a save is not worth stopping the queue for; the next one retries.
    if profiles {
        let _ = store.save_profiles(&sched.profiles);
    }
    if history {
        let _ = store.save_history(&sched.history);
        let _ = store.save_tally(&sched.tally);
    }
    if config {
        let _ = store.save_config(&sched.cfg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use headroom_core::model::{GIB, JobSpec};
    use headroom_core::protocol::{Control, read_msg, request, write_msg};
    use std::io::{BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};
    use std::sync::atomic::AtomicUsize;
    use std::thread::JoinHandle;

    struct Counting(Arc<AtomicUsize>);
    impl Probe for Counting {
        fn system(&mut self) -> Snapshot {
            self.0.fetch_add(1, Ordering::Relaxed);
            Snapshot {
                mem_total: 32 * GIB,
                mem_available: 24 * GIB,
                cores: 8,
                ..Snapshot::default()
            }
        }
        fn groups(&mut self, _: &[i32]) -> HashMap<i32, Usage> {
            HashMap::new()
        }
    }

    struct Running {
        store: Store,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<io::Result<()>>>,
        samples: Arc<AtomicUsize>,
    }
    impl Running {
        fn start(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("hr-daemon-{name}-{}", std::process::id()));
            let store = Store::at(root).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let samples = Arc::new(AtomicUsize::new(0));
            let (s, flag, count) = (store.clone(), stop.clone(), samples.clone());
            let thread = std::thread::spawn(move || run_with(s, flag, Box::new(Counting(count))));
            let running = Self {
                store,
                stop,
                thread: Some(thread),
                samples,
            };
            let start = Instant::now();
            while request(&running.store.socket(), &ClientMsg::List).is_err() {
                assert!(
                    start.elapsed() < Duration::from_secs(5),
                    "daemon did not start"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            running
        }

        fn shutdown(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap().unwrap();
            }
        }
    }
    impl Drop for Running {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    struct Process(Child);
    impl Drop for Process {
        fn drop(&mut self) {
            unsafe { libc::killpg(self.0.id() as i32, libc::SIGKILL) };
            let _ = self.0.wait();
        }
    }

    #[test]
    fn shutdown_thaws_jobs_and_closes_their_leases() {
        let mut daemon = Running::start("shutdown");
        let mut lease = UnixStream::connect(daemon.store.socket()).unwrap();
        lease
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        write_msg(&mut lease, &ClientMsg::Enqueue(JobSpec::default())).unwrap();
        let mut lease = BufReader::new(lease);
        let id = match read_msg::<_, DaemonMsg>(&mut lease).unwrap().unwrap() {
            DaemonMsg::Granted { id, .. } => id,
            other => panic!("expected admission, got {other:?}"),
        };
        let child = Process(
            Command::new("/bin/sleep")
                .arg("30")
                .process_group(0)
                .spawn()
                .unwrap(),
        );
        let pgid = child.0.id() as i32;
        write_msg(lease.get_mut(), &ClientMsg::Started { pid: pgid, pgid }).unwrap();
        // Wait for Started to be processed before asking on another connection.
        let start = Instant::now();
        loop {
            let DaemonMsg::Jobs(list) = request(&daemon.store.socket(), &ClientMsg::List).unwrap()
            else {
                panic!("listing")
            };
            if list.running.iter().any(|j| j.pgid == Some(pgid)) {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(3));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            request(
                &daemon.store.socket(),
                &ClientMsg::Control(Control::Freeze { id })
            )
            .unwrap(),
            DaemonMsg::Ok
        );
        let state = || {
            String::from_utf8(
                Command::new("/bin/ps")
                    .args(["-o", "stat=", "-p", &pgid.to_string()])
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap()
        };
        assert!(state().contains('T'), "process was frozen");
        daemon.shutdown();
        assert!(!state().contains('T'), "shutdown must resume the process");
        assert!(matches!(
            read_msg::<_, DaemonMsg>(&mut lease).unwrap(),
            Some(DaemonMsg::Frozen { .. })
        ));
        assert_eq!(
            read_msg::<_, DaemonMsg>(&mut lease).unwrap(),
            None,
            "shutdown must close the lease"
        );
    }

    #[test]
    fn continuous_client_traffic_does_not_starve_samples() {
        let mut daemon = Running::start("traffic");
        let mut stream = UnixStream::connect(daemon.store.socket()).unwrap();
        let shutdown = stream.try_clone().unwrap();
        let mut reader = stream.try_clone().unwrap();
        let drain = std::thread::spawn(move || {
            let _ = io::copy(&mut reader, &mut io::sink());
        });
        let flood = std::thread::spawn(move || {
            let batch = b"{\"t\":\"list\"}\n".repeat(8192);
            while stream.write_all(&batch).is_ok() {}
        });
        std::thread::sleep(Duration::from_millis(2300));
        let samples = daemon.samples.load(Ordering::Relaxed);
        daemon.stop.store(true, Ordering::Relaxed);
        let _ = shutdown.shutdown(std::net::Shutdown::Both);
        flood.join().unwrap();
        drain.join().unwrap();
        daemon.shutdown();
        assert!(
            samples >= 3,
            "initial reading plus two ticks, got {samples}"
        );
    }
}
