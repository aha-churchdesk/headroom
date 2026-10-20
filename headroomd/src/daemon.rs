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

use headroom_core::model::Snapshot;
use headroom_core::protocol::{ClientMsg, DaemonMsg};
use headroom_core::scheduler::{Effect, Scheduler, Sig, Usage};
use headroom_core::store::Store;

use crate::sample::Sampler;
use crate::server::{self, Conns, Event};
use crate::single;

pub const TICK: Duration = Duration::from_secs(1);

fn wall() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn signal(pgid: i32, sig: Sig) {
    let s = match sig {
        Sig::Stop => libc::SIGSTOP,
        Sig::Cont => libc::SIGCONT,
        Sig::Term => libc::SIGTERM,
        Sig::Kill => libc::SIGKILL,
    };
    // Never a pgid of 0 or 1: killpg(0) would hit our own group.
    if pgid > 1 {
        unsafe { libc::killpg(pgid, s) };
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
    let start = Instant::now();
    let ms = || start.elapsed().as_millis() as u64;

    // A first reading before any client arrives, so the first job is
    // admitted against real numbers.
    let fx = sched.tick(sampler.system(), &HashMap::new(), 0);
    let conns: Conns = Arc::new(Mutex::new(HashMap::new()));
    apply(&store, &sched, &conns, fx);

    let (tx, rx) = mpsc::channel();
    server::spawn(listener, tx, conns.clone());
    let mut next_tick = Instant::now() + TICK;

    while !stop.load(Ordering::Relaxed) {
        let wait = next_tick
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(200));
        let fx = match rx.recv_timeout(wait) {
            Ok(Event::Msg(c, msg)) => match msg {
                ClientMsg::Enqueue(spec) => sched.enqueue(c, spec, ms()),
                ClientMsg::Started { pgid, .. } => sched.started(c, pgid),
                ClientMsg::Exited { code, signal } => sched.exited(c, code, signal, ms(), wall()),
                ClientMsg::List => vec![Effect::Send(c, DaemonMsg::Jobs(sched.list()))],
                ClientMsg::Control(ctl) => sched.control(c, ctl, ms(), wall()),
            },
            Ok(Event::Closed(c)) => {
                conns.lock().unwrap_or_else(|p| p.into_inner()).remove(&c);
                sched.disconnected(c, ms(), wall())
            }
            Err(RecvTimeoutError::Timeout) => {
                if Instant::now() < next_tick {
                    continue;
                }
                next_tick += TICK;
                let snap = sampler.system();
                let usage = sampler.groups(&sched.pgids());
                sched.tick(snap, &usage, ms())
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        apply(&store, &sched, &conns, fx);
    }
    let _ = fs::remove_file(&sock);
    Ok(())
}

fn apply(store: &Store, sched: &Scheduler, conns: &Conns, fx: Vec<Effect>) {
    let (mut profiles, mut history, mut config) = (false, false, false);
    for e in fx {
        match e {
            Effect::Send(c, m) => server::send(conns, c, &m),
            Effect::Signal { pgid, sig } => signal(pgid, sig),
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
    }
    if config {
        let _ = store.save_config(&sched.cfg);
    }
}
