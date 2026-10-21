//! Which processes belong to a job.
//!
//! A job starts as one process group, but tools move their children out of
//! it: turbo runs each task in a group of its own, and anything that opens a
//! pseudo-terminal (lefthook in a git hook) starts a new session. A job is
//! its group plus every descendant of a process in it, whatever group the
//! descendant is in. A child whose parent has already exited is reparented
//! to launchd and can't be traced back; it still counts while it stays in
//! the job's own group.

use std::collections::{BTreeSet, HashMap};

/// One process: pid, parent pid, process group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
}

/// The pids of the job whose process group is `pgid`, in ascending order.
pub fn job_pids(procs: &[Proc], pgid: i32) -> Vec<i32> {
    let roots: Vec<i32> = procs
        .iter()
        .filter(|p| p.pgid == pgid)
        .map(|p| p.pid)
        .collect();
    descendant_pids(procs, &roots)
}

/// These processes and their descendants, regardless of process group.
pub fn descendant_pids(procs: &[Proc], roots: &[i32]) -> Vec<i32> {
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for p in procs {
        children.entry(p.ppid).or_default().push(p.pid);
    }
    let mut seen: BTreeSet<i32> = BTreeSet::new();
    let mut todo = roots.to_vec();
    while let Some(pid) = todo.pop() {
        // launchd and the kernel are everyone's ancestors, never a job's.
        if pid <= 1 || !seen.insert(pid) {
            continue;
        }
        todo.extend(children.get(&pid).into_iter().flatten());
    }
    seen.into_iter().collect()
}

/// The process groups a job's processes are in: its own first, then the
/// ones its descendants moved to. These are what freezing or killing the job
/// has to reach.
pub fn job_groups(procs: &[Proc], pgid: i32) -> Vec<i32> {
    let pids: BTreeSet<i32> = job_pids(procs, pgid).into_iter().collect();
    let mut groups: Vec<i32> = vec![pgid];
    let others: BTreeSet<i32> = procs
        .iter()
        .filter(|p| pids.contains(&p.pid) && p.pgid != pgid && p.pgid > 1)
        .map(|p| p.pgid)
        .collect();
    groups.extend(others);
    groups
}

/// The job, among `jobs` (id and process group), whose processes include
/// `pid`.
pub fn job_of<T: Copy>(procs: &[Proc], jobs: &[(T, i32)], pid: i32) -> Option<T> {
    jobs.iter()
        .find(|(_, pgid)| job_pids(procs, *pgid).contains(&pid))
        .map(|(id, _)| *id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: i32, ppid: i32, pgid: i32) -> Proc {
        Proc { pid, ppid, pgid }
    }

    /// `headroom run` (10) starts turbo as a group of its own (20); turbo
    /// starts each task in a new group (30, 40). Something unrelated (50)
    /// runs beside it.
    fn turbo() -> Vec<Proc> {
        vec![
            p(1, 0, 1),
            p(10, 5, 10),
            p(20, 10, 20),
            p(21, 20, 20),
            p(30, 21, 30),
            p(31, 30, 30),
            p(40, 21, 40),
            p(50, 1, 50),
        ]
    }

    #[test]
    fn a_job_includes_children_that_moved_to_their_own_group() {
        assert_eq!(job_pids(&turbo(), 20), [20, 21, 30, 31, 40]);
        assert_eq!(job_groups(&turbo(), 20), [20, 30, 40]);
    }

    #[test]
    fn a_job_without_subgroups_is_just_its_group() {
        let procs = [p(1, 0, 1), p(20, 10, 20), p(21, 20, 20)];
        assert_eq!(job_pids(&procs, 20), [20, 21]);
        assert_eq!(job_groups(&procs, 20), [20]);
    }

    #[test]
    fn a_job_that_has_exited_has_no_processes_but_keeps_its_own_group() {
        assert!(job_pids(&turbo(), 99).is_empty());
        assert_eq!(job_groups(&turbo(), 99), [99]);
        assert!(job_pids(&[], 20).is_empty());
    }

    #[test]
    fn an_orphan_counts_while_it_is_in_the_group_and_launchd_never_does() {
        // 22 lost its parent and was reparented to launchd, still in 20.
        // 60 is a child of launchd in another group: not the job's.
        let mut procs = turbo();
        procs.push(p(22, 1, 20));
        procs.push(p(60, 1, 60));
        let pids = job_pids(&procs, 20);
        assert!(pids.contains(&22));
        assert!(!pids.contains(&1) && !pids.contains(&60) && !pids.contains(&50));
        // A group whose leader is launchd's child doesn't pull launchd's
        // other children in.
        assert_eq!(job_pids(&procs, 1), [] as [i32; 0]);
    }

    #[test]
    fn a_task_belongs_to_the_job_that_started_it() {
        let jobs = [(7u64, 20), (8u64, 50)];
        // turbo's task 31, in a group of its own, is part of job 7.
        assert_eq!(job_of(&turbo(), &jobs, 31), Some(7));
        assert_eq!(job_of(&turbo(), &jobs, 50), Some(8));
        // `headroom run` itself (10) is above the job, not in it.
        assert_eq!(job_of(&turbo(), &jobs, 10), None);
        assert_eq!(job_of(&turbo(), &[] as &[(u64, i32)], 31), None);
        assert_eq!(job_of(&turbo(), &jobs, 999), None);
    }

    #[test]
    fn a_parent_loop_from_a_racy_listing_ends() {
        // Pids reused between reads can make a cycle; it must not hang.
        let procs = [p(20, 21, 20), p(21, 20, 20)];
        assert_eq!(job_pids(&procs, 20), [20, 21]);
    }
}
