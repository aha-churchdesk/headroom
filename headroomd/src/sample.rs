//! Reading the machine: memory, swap, load, and each job's process group.
//!
//! Memory "used" matches Activity Monitor: app memory (anonymous pages that
//! can't be purged) plus wired plus compressed. Free pages alone would be
//! misleading, because macOS fills spare memory with file cache that it
//! drops the moment someone needs it.
//!
//! Per job, the figure is `phys_footprint` summed over the process group:
//! what Activity Monitor's Memory column shows. RSS would double-count
//! shared pages across a test runner's workers.

#![allow(deprecated)] // libc points mach calls at the mach2 crate; these are stable.

use std::collections::HashMap;
use std::ffi::CString;
use std::time::Instant;

use headroom_core::model::{Bytes, Pressure, Snapshot};
use headroom_core::scheduler::Usage;

use crate::view::{AppUsage, ProcInfo};

const PROC_ALL_PIDS: u32 = 1;
const PROC_PGRP_ONLY: u32 = 2;

fn sysctl<T: Copy + Default>(name: &str) -> Option<T> {
    let name = CString::new(name).ok()?;
    let mut v = T::default();
    let mut len = std::mem::size_of::<T>();
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut v as *mut T).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && len == std::mem::size_of::<T>()).then_some(v)
}

/// `kern.memorystatus_vm_pressure_level`: 1 normal, 2 warn, 4 critical.
pub fn pressure_from_level(level: i32) -> Pressure {
    match level {
        2 => Pressure::Warn,
        4 => Pressure::Critical,
        _ => Pressure::Normal,
    }
}

/// Activity Monitor's "Memory Used", from page counts.
pub fn used_bytes(internal: u64, purgeable: u64, wired: u64, compressed: u64, page: u64) -> Bytes {
    (internal.saturating_sub(purgeable) + wired + compressed) * page
}

pub struct Sampler {
    host: libc::mach_port_t,
    page: u64,
    total: Bytes,
    cores: u32,
    /// (numer, denom): mach absolute time to nanoseconds.
    timebase: (u64, u64),
    prev_ticks: Option<[u32; 4]>,
    /// Per pid: CPU nanoseconds at the last sample.
    prev_cpu: HashMap<i32, u64>,
    prev_at: Option<Instant>,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    pub fn new() -> Self {
        let mut tb = libc::mach_timebase_info { numer: 1, denom: 1 };
        unsafe { libc::mach_timebase_info(&mut tb) };
        // Every core, efficiency ones too: agents' load spreads over all of
        // them, and counting only performance cores held the queue whenever
        // a few sessions were busy.
        let cores = sysctl::<i32>("hw.physicalcpu").unwrap_or(0).max(0) as u32;
        Self {
            // One send right for the process's life, not one per sample.
            host: unsafe { libc::mach_host_self() },
            page: unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64,
            total: sysctl::<u64>("hw.memsize").unwrap_or(0),
            cores,
            timebase: (tb.numer.max(1) as u64, tb.denom.max(1) as u64),
            prev_ticks: None,
            prev_cpu: HashMap::new(),
            prev_at: None,
        }
    }

    pub fn system(&mut self) -> Snapshot {
        let mut vm: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        let rc = unsafe {
            libc::host_statistics64(
                self.host,
                libc::HOST_VM_INFO64,
                (&mut vm as *mut libc::vm_statistics64).cast(),
                &mut count,
            )
        };
        let used = if rc == 0 {
            used_bytes(
                vm.internal_page_count as u64,
                vm.purgeable_count as u64,
                vm.wire_count as u64,
                vm.compressor_page_count as u64,
                self.page,
            )
            .min(self.total)
        } else {
            0
        };
        let swap = sysctl::<libc::xsw_usage>("vm.swapusage").map_or(0, |s| s.xsu_used);
        let mut load = [0f64; 3];
        let load1 = if unsafe { libc::getloadavg(load.as_mut_ptr(), 3) } > 0 {
            load[0]
        } else {
            0.0
        };
        Snapshot {
            pressure: pressure_from_level(
                sysctl::<i32>("kern.memorystatus_vm_pressure_level").unwrap_or(1),
            ),
            mem_total: self.total,
            mem_used: used,
            mem_available: self.total.saturating_sub(used),
            swap_used: swap,
            load1,
            cores: self.cores,
            cpu_pct: self.cpu_pct(),
        }
    }

    fn cpu_pct(&mut self) -> f64 {
        let mut info: libc::host_cpu_load_info = unsafe { std::mem::zeroed() };
        let mut count = (std::mem::size_of::<libc::host_cpu_load_info>()
            / std::mem::size_of::<libc::integer_t>())
            as libc::mach_msg_type_number_t;
        let rc = unsafe {
            libc::host_statistics(
                self.host,
                libc::HOST_CPU_LOAD_INFO,
                (&mut info as *mut libc::host_cpu_load_info).cast(),
                &mut count,
            )
        };
        if rc != 0 {
            return 0.0;
        }
        // user, system, idle, nice
        let now = info.cpu_ticks;
        let pct = match self.prev_ticks {
            Some(p) => {
                let d: Vec<u64> = now
                    .iter()
                    .zip(p)
                    .map(|(n, p)| n.wrapping_sub(p) as u64)
                    .collect();
                let total: u64 = d.iter().sum();
                if total == 0 {
                    0.0
                } else {
                    (d[0] + d[1] + d[3]) as f64 / total as f64 * 100.0
                }
            }
            None => 0.0,
        };
        self.prev_ticks = Some(now);
        pct
    }

    /// Footprint, CPU and process count for each process group. A group
    /// with no processes left is absent from the result.
    pub fn groups(&mut self, pgids: &[i32]) -> HashMap<i32, Usage> {
        let now = Instant::now();
        let elapsed_ns = self
            .prev_at
            .map_or(0, |t| now.duration_since(t).as_nanos() as u64);
        let mut cpu_now = HashMap::new();
        let mut out = HashMap::new();
        for &pgid in pgids {
            let mut u = Usage::default();
            let mut cpu_delta = 0u64;
            for pid in pids_in_group(pgid) {
                let mut ri: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
                let rc = unsafe {
                    libc::proc_pid_rusage(
                        pid,
                        libc::RUSAGE_INFO_V4,
                        (&mut ri as *mut libc::rusage_info_v4).cast(),
                    )
                };
                if rc != 0 {
                    continue; // exited between listing and reading
                }
                u.procs += 1;
                u.footprint += ri.ri_phys_footprint;
                let ns = (ri.ri_user_time + ri.ri_system_time) * self.timebase.0 / self.timebase.1;
                if let Some(prev) = self.prev_cpu.get(&pid) {
                    cpu_delta += ns.saturating_sub(*prev);
                }
                cpu_now.insert(pid, ns);
            }
            if u.procs > 0 {
                u.cpu_pct = if elapsed_ns == 0 {
                    0.0
                } else {
                    cpu_delta as f64 / elapsed_ns as f64 * 100.0
                };
                out.insert(pgid, u);
            }
        }
        self.prev_cpu = cpu_now;
        self.prev_at = Some(now);
        out
    }
}

/// The app a process belongs to: the outermost `.app` in its path, so
/// Chrome's helpers count as Chrome; otherwise the executable's name.
pub fn app_name(path: &str) -> String {
    match path.split('/').find(|c| c.ends_with(".app")) {
        Some(app) => app.trim_end_matches(".app").to_string(),
        None => path.rsplit('/').next().unwrap_or(path).to_string(),
    }
}

/// Processes listed per app on the "Other apps" page.
const PER_APP: usize = 6;
/// Ancestors walked looking for what started a process.
const MAX_DEPTH: usize = 10;

fn footprint(pid: i32) -> Option<Bytes> {
    let mut ri: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V4,
            (&mut ri as *mut libc::rusage_info_v4).cast(),
        )
    };
    (rc == 0).then_some(ri.ri_phys_footprint)
}

fn exe_path(pid: i32) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

fn ppid(pid: i32) -> Option<i32> {
    let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdshortinfo).cast(),
            size,
        )
    };
    (n == size).then_some(info.pbsi_ppid as i32)
}

/// The process's working directory. Only for processes we may inspect.
pub fn cwd(pid: i32) -> Option<String> {
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&mut info as *mut libc::proc_vnodepathinfo).cast(),
            size,
        )
    };
    if n != size {
        return None;
    }
    // libc spells MAXPATHLEN chars as 32 × 32.
    let bytes: Vec<u8> = info
        .pvi_cdir
        .vip_path
        .iter()
        .flatten()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())
}

/// The process's arguments, from `kern.procargs2`: argc, the executable
/// path, NUL padding, then argc NUL-terminated arguments (then the
/// environment, which is not read). Only for processes we may inspect.
pub fn args(pid: i32) -> Option<Vec<String>> {
    let max: libc::c_int = sysctl("kern.argmax")?;
    let mut buf = vec![0u8; max as usize];
    let mut len = buf.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len < 4 {
        return None;
    }
    parse_procargs(&buf[..len])
}

fn parse_procargs(buf: &[u8]) -> Option<Vec<String>> {
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?);
    let mut rest = &buf[4..];
    // The executable path, then padding.
    let end = rest.iter().position(|&b| b == 0)?;
    rest = &rest[end..];
    let start = rest.iter().position(|&b| b != 0)?;
    rest = &rest[start..];
    let args: Vec<String> = rest
        .split(|&b| b == 0)
        .take(argc.max(0) as usize)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    (!args.is_empty()).then_some(args)
}

/// Up to and including the outermost `.app`, or the path itself.
pub fn bundle_path(path: &str) -> String {
    match path.find(".app/") {
        Some(i) => path[..i + 4].to_string(),
        None => path.to_string(),
    }
}

/// A process's name for people: its app, or the name it was started as.
/// Claude Code's executable is a file named after its version (2.1.304),
/// and /bin/sh is bash underneath; argv[0] says "claude" and "sh".
fn display_name(pid: i32, path: &str) -> String {
    if path.contains(".app/") {
        return app_name(path);
    }
    args(pid)
        .and_then(|a| a.into_iter().next())
        .map(|a0| {
            a0.trim_start_matches('-')
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_string()
        })
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| app_name(path))
}

/// What started a process: its parent's name, the grandparent's and so on,
/// up to the first app (a Terminal, an editor, Claude) or launchd. Repeats
/// are folded: `node › node › zsh` reads `node › zsh`.
pub fn started_by(pid: i32) -> Vec<String> {
    let mut chain: Vec<String> = Vec::new();
    let mut p = ppid(pid);
    while let Some(parent) = p.filter(|_| chain.len() < MAX_DEPTH) {
        if parent <= 1 {
            chain.push("launchd".into());
            break;
        }
        let Some(path) = exe_path(parent) else { break };
        let name = display_name(parent, &path);
        if chain.last() != Some(&name) {
            chain.push(name);
        }
        if path.contains(".app/") {
            break;
        }
        p = ppid(parent);
    }
    chain
}

fn proc_info(pid: i32, bytes: Bytes, path: &str) -> ProcInfo {
    let cmd = args(pid).map_or_else(|| path.to_string(), |a| headroom_core::shell::join(&a));
    ProcInfo {
        pid,
        bytes,
        cmd,
        cwd: cwd(pid).filter(|c| c != "/"),
        started_by: started_by(pid),
    }
}

/// Every process id on the machine.
pub fn all_pids() -> Vec<i32> {
    list_pids(PROC_ALL_PIDS, 0)
}

/// Memory by app among `pids`, largest first, leaving out the process
/// groups Headroom manages. Processes we may not inspect (other users',
/// the kernel's) are skipped. The largest apps get their largest processes
/// listed in detail.
pub fn top_apps(pids: &[i32], skip_pgids: &[i32], limit: usize) -> Vec<AppUsage> {
    // (pid, footprint, executable path)
    type Proc = (i32, Bytes, String);
    let mut by_app: HashMap<String, Vec<Proc>> = HashMap::new();
    for &pid in pids {
        if skip_pgids.contains(&unsafe { libc::getpgid(pid) }) {
            continue;
        }
        let (Some(bytes), Some(path)) = (footprint(pid), exe_path(pid)) else {
            continue;
        };
        by_app
            .entry(app_name(&path))
            .or_default()
            .push((pid, bytes, path));
    }
    let mut v: Vec<(String, Vec<Proc>)> = by_app.into_iter().collect();
    let total = |p: &[Proc]| p.iter().map(|x| x.1).sum::<Bytes>();
    v.sort_by(|a, b| total(&b.1).cmp(&total(&a.1)).then_with(|| a.0.cmp(&b.0)));
    v.truncate(limit);
    v.into_iter()
        .map(|(name, mut procs)| {
            procs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            AppUsage {
                bytes: total(&procs),
                procs: procs.len() as u32,
                path: bundle_path(&procs[0].2),
                top: procs
                    .iter()
                    .take(PER_APP)
                    .map(|(pid, bytes, path)| proc_info(*pid, *bytes, path))
                    .collect(),
                name,
            }
        })
        .collect()
}

fn pids_in_group(pgid: i32) -> Vec<i32> {
    list_pids(PROC_PGRP_ONLY, pgid as u32)
}

fn list_pids(kind: u32, arg: u32) -> Vec<i32> {
    // Ask for the size first; the list can grow between the two calls, so
    // leave room.
    let need = unsafe { libc::proc_listpids(kind, arg, std::ptr::null_mut(), 0) };
    let mut buf = vec![0i32; (need.max(0) as usize / 4) + 64];
    let bytes = unsafe {
        libc::proc_listpids(
            kind,
            arg,
            buf.as_mut_ptr().cast(),
            (buf.len() * 4) as libc::c_int,
        )
    };
    if bytes <= 0 {
        return vec![];
    }
    buf.truncate(bytes as usize / 4);
    buf.retain(|&p| p > 0);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    #[test]
    fn pressure_levels_map_and_unknown_is_normal() {
        assert_eq!(pressure_from_level(1), Pressure::Normal);
        assert_eq!(pressure_from_level(2), Pressure::Warn);
        assert_eq!(pressure_from_level(4), Pressure::Critical);
        assert_eq!(pressure_from_level(0), Pressure::Normal);
        assert_eq!(pressure_from_level(99), Pressure::Normal);
    }

    #[test]
    fn used_counts_app_wired_and_compressed_but_not_purgeable() {
        assert_eq!(used_bytes(100, 10, 20, 30, 16384), 140 * 16384);
        assert_eq!(
            used_bytes(5, 10, 0, 0, 16384),
            0,
            "more purgeable than internal clamps at zero"
        );
    }

    #[test]
    fn system_snapshot_is_plausible_on_this_machine() {
        let s = Sampler::new().system();
        assert!(s.mem_total > 1 << 30);
        assert!(s.mem_used > 0 && s.mem_used <= s.mem_total);
        assert_eq!(s.mem_available, s.mem_total - s.mem_used);
        // All of them, efficiency cores too. A virtual machine (CI) may
        // have performance cores only.
        let perf = sysctl::<i32>("hw.perflevel0.physicalcpu").unwrap_or(0) as u32;
        assert_eq!(s.cores, sysctl::<i32>("hw.physicalcpu").unwrap() as u32);
        assert!(s.cores > 0 && s.cores >= perf, "{} cores, {perf} performance", s.cores);
    }

    #[test]
    fn measures_a_real_process_group_and_forgets_it_when_gone() {
        let mut child = Command::new("/bin/sleep")
            .arg("5")
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id() as i32;
        let mut s = Sampler::new();
        let g = s.groups(&[pgid]);
        let u = g.get(&pgid).expect("group measured");
        assert_eq!(u.procs, 1);
        assert!(u.footprint > 0);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(s.groups(&[pgid]).is_empty());
    }

    #[test]
    fn helpers_count_toward_their_app() {
        assert_eq!(
            app_name(
                "/Applications/Google Chrome.app/Contents/Frameworks/Google Chrome Framework.framework/Helpers/Google Chrome Helper (Renderer).app/Contents/MacOS/x"
            ),
            "Google Chrome"
        );
        assert_eq!(app_name("/usr/sbin/mDNSResponder"), "mDNSResponder");
        assert_eq!(app_name("node"), "node");
    }

    #[test]
    fn top_apps_finds_this_test_and_respects_the_skip_list() {
        let apps = top_apps(&all_pids(), &[], 1000);
        assert!(!apps.is_empty());
        assert!(
            apps.windows(2).all(|w| w[0].bytes >= w[1].bytes),
            "largest first"
        );
        // Only this test's own group, so other processes coming and going
        // between two walks of the whole machine can't change the answer.
        let me = unsafe { libc::getpgid(0) };
        let mine = pids_in_group(me);
        assert!(!top_apps(&mine, &[], 1000).is_empty());
        assert!(top_apps(&mine, &[me], 1000).is_empty());
        assert_eq!(top_apps(&all_pids(), &[], 3).len(), 3);
        assert!(
            apps.iter()
                .all(|a| !a.top.is_empty() && a.top.len() <= PER_APP)
        );
    }

    #[test]
    fn a_process_says_what_it_runs_where_and_who_started_it() {
        let dir = std::env::temp_dir().join(format!("hr-procinfo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .current_dir(&dir)
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let apps = top_apps(&[pid], &[], 10);
        assert_eq!(apps.len(), 1);
        let (app, p) = (&apps[0], &apps[0].top[0]);
        assert_eq!(
            (app.name.as_str(), app.path.as_str()),
            ("sleep", "/bin/sleep")
        );
        assert_eq!((p.pid, p.cmd.as_str()), (pid, "/bin/sleep 30"));
        let real = std::fs::canonicalize(&dir).unwrap();
        assert_eq!(p.cwd.as_deref(), real.to_str());
        let me = app_name(&std::env::current_exe().unwrap().display().to_string());
        assert_eq!(p.started_by.first(), Some(&me), "{:?}", p.started_by);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(top_apps(&[pid], &[], 10).is_empty(), "gone");
    }

    #[test]
    fn other_users_processes_give_what_they_can() {
        // launchd: its memory and arguments are not ours to read.
        assert_eq!(args(1), None);
        assert_eq!(cwd(1), None);
        assert_eq!(args(999_999), None);
        assert!(started_by(999_999).is_empty());
    }

    #[test]
    fn procargs_are_parsed_past_the_executable_and_padding() {
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend(b"/usr/bin/node\0\0\0\0node\0a b.js\0PATH=/bin\0");
        assert_eq!(
            parse_procargs(&buf),
            Some(vec!["node".into(), "a b.js".into()])
        );
        assert_eq!(parse_procargs(&[1, 0]), None, "too short");
        let mut empty = 0i32.to_ne_bytes().to_vec();
        empty.extend(b"/x\0\0y\0");
        assert_eq!(parse_procargs(&empty), None);
    }

    #[test]
    fn bundle_paths_stop_at_the_outermost_app() {
        assert_eq!(
            bundle_path(
                "/Applications/Google Chrome.app/Contents/Frameworks/x.app/Contents/MacOS/x"
            ),
            "/Applications/Google Chrome.app"
        );
        assert_eq!(
            bundle_path("/opt/homebrew/bin/node"),
            "/opt/homebrew/bin/node"
        );
    }

    #[test]
    fn a_group_that_never_existed_is_absent() {
        assert!(Sampler::new().groups(&[999_999]).is_empty());
    }
}
