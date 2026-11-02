//! The worktrees of the repositories Headroom has queued commands in, and
//! which of them are merged and safe to remove.
//!
//! Merged means the branch is in the default branch's history, or its
//! head is part of a merged GitHub pull request (a squash merge leaves no
//! trace in history; `gh` knows). A remote branch that is gone is only a
//! hint. Removal re-checks everything first, never forces, and leaves
//! anything with changes, a lock, or a process working in it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// A worktree with something working in it belongs to someone: an agent
/// session, a shell, an editor.
pub type Procs = [(String, String)];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Merged {
    /// In the default branch's history.
    Branch,
    /// Its head is in this merged pull request.
    Pr(u64),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Worktree {
    pub path: String,
    pub branch: Option<String>,
    pub head: String,
    /// The repository's own checkout, which is never removed.
    pub main: bool,
    pub locked: bool,
    /// Its folder is gone; git still lists it until pruned.
    pub missing: bool,
    pub merged: Option<Merged>,
    /// Its upstream branch was deleted, as of the last fetch.
    pub remote_gone: bool,
    /// Unix seconds of its newest commit.
    pub last_commit: Option<u64>,
    /// Changed and untracked files. Only counted for merged worktrees, the
    /// ones that could be removed; `None` when not counted.
    pub changes: Option<u32>,
    /// Processes working inside it.
    pub in_use: Vec<String>,
}

impl Worktree {
    /// Why it may not be removed, if anything stops it.
    pub fn blocker(&self) -> Option<String> {
        if self.main {
            return Some("the main checkout".into());
        }
        if self.locked {
            return Some("locked".into());
        }
        if self.merged.is_none() && !self.missing {
            return Some("not merged".into());
        }
        if !self.in_use.is_empty() {
            return Some(format!("in use by {}", self.in_use.join(", ")));
        }
        match self.changes {
            _ if self.missing => None,
            Some(0) => None,
            Some(n) => Some(format!(
                "{n} uncommitted file{}",
                if n == 1 { "" } else { "s" }
            )),
            None => Some("changes not checked".into()),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Repo {
    pub path: String,
    /// The branch merges are measured against, such as `origin/main`.
    pub default: Option<String>,
    /// Whether merged pull requests could be read; without them squash
    /// merges look unmerged.
    pub prs_checked: bool,
    pub worktrees: Vec<Worktree>,
}

const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const GH_TIMEOUT: Duration = Duration::from_secs(30);
/// Worktrees whose changes are counted at once.
const PARALLEL: usize = 8;

/// Run `program` in `dir`; its output when it succeeds within `timeout`.
fn run(program: &str, dir: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut child = Command::new(program)
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;
    // Read while waiting: a full pipe would stall the child.
    let mut stdout = child.stdout.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        out
    });
    let end = Instant::now() + timeout;
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break s;
        }
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{program} {} timed out", args.join(" ")));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let out = reader.join().unwrap_or_default();
    if !status.success() {
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut err);
        }
        return Err(format!("{program} {}: {}", args.join(" "), err.trim()));
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    run("git", dir, args, GIT_TIMEOUT)
}

/// `git worktree list --porcelain`, the first entry being the main checkout.
pub fn parse_list(porcelain: &str) -> Vec<Worktree> {
    let mut out: Vec<Worktree> = Vec::new();
    for line in porcelain.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            out.push(Worktree {
                path: path.to_string(),
                main: out.is_empty(),
                ..Worktree::default()
            });
            continue;
        }
        let Some(w) = out.last_mut() else { continue };
        if let Some(head) = line.strip_prefix("HEAD ") {
            w.head = head.to_string();
        } else if let Some(b) = line.strip_prefix("branch ") {
            w.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
        } else if line == "locked" || line.starts_with("locked ") {
            w.locked = true;
        } else if line == "prunable" || line.starts_with("prunable ") {
            w.missing = true;
        } else if line == "bare" {
            // A bare repository's entry is no checkout to remove.
            w.main = true;
        }
    }
    out
}

/// The branch merges count against: the remote's default, else a usual name.
fn default_branch(repo: &Path) -> Option<String> {
    if let Ok(s) = git(
        repo,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    ) {
        return Some(s.trim().to_string());
    }
    ["origin/main", "origin/master", "main", "master"]
        .into_iter()
        .find(|b| git(repo, &["rev-parse", "--verify", "--quiet", b]).is_ok())
        .map(str::to_string)
}

/// Per local branch: its newest commit and whether its upstream is gone.
fn branches(repo: &Path) -> HashMap<String, (Option<u64>, bool)> {
    let Ok(out) = git(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname:short)%09%(committerdate:unix)%09%(upstream:track)",
            "refs/heads",
        ],
    ) else {
        return HashMap::new();
    };
    out.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let name = f.next()?.to_string();
            let at = f.next().and_then(|t| t.parse().ok());
            let gone = f.next().is_some_and(|t| t.contains("gone"));
            Some((name, (at, gone)))
        })
        .collect()
}

fn is_ancestor(repo: &Path, a: &str, b: &str) -> bool {
    git(repo, &["merge-base", "--is-ancestor", a, b]).is_ok()
}

/// Merged pull requests: head branch, head commit, number. `None` when
/// `gh` can't say (not installed, signed out, not a GitHub remote).
fn merged_prs(repo: &Path) -> Option<Vec<(String, String, u64)>> {
    let out = run(
        "gh",
        repo,
        &[
            "pr",
            "list",
            "--state",
            "merged",
            "--limit",
            "1000",
            "--json",
            "number,headRefName,headRefOid",
        ],
        GH_TIMEOUT,
    )
    .ok()?;
    let prs: Vec<Value> = serde_json::from_str(&out).ok()?;
    Some(
        prs.iter()
            .filter_map(|p| {
                Some((
                    p["headRefName"].as_str()?.to_string(),
                    p["headRefOid"].as_str()?.to_string(),
                    p["number"].as_u64()?,
                ))
            })
            .collect(),
    )
}

/// The merged pull request of `branch` that holds `head`: its head commit
/// is `head` or comes after it, so nothing on the branch is left out.
fn merged_by_pr(
    branch: &str,
    head: &str,
    prs: &[(String, String, u64)],
    is_ancestor: impl Fn(&str, &str) -> bool,
) -> Option<u64> {
    prs.iter()
        .filter(|(name, _, _)| name == branch)
        .find(|(_, oid, _)| oid == head || is_ancestor(head, oid))
        .map(|(_, _, n)| *n)
}

/// Changed and untracked files in a worktree. Ignored files, such as
/// `node_modules`, don't count: git removes those with the folder.
fn changes(path: &Path) -> Option<u32> {
    git(path, &["status", "--porcelain"])
        .ok()
        .map(|s| s.lines().count() as u32)
}

/// The processes working in `path` or below it.
pub fn users_of(path: &str, procs: &Procs) -> Vec<String> {
    let inside = |cwd: &str| {
        cwd == path
            || cwd
                .strip_prefix(path)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    let mut names: Vec<String> = procs
        .iter()
        .filter(|(cwd, _)| inside(cwd))
        .map(|(_, name)| name.clone())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// One repository's worktrees, merged or not. `None` for a folder that is
/// no repository, or one with no worktrees besides its own checkout.
pub fn scan_repo(repo: &Path, procs: &Procs) -> Option<Repo> {
    let mut worktrees = parse_list(&git(repo, &["worktree", "list", "--porcelain"]).ok()?);
    if worktrees.len() < 2 {
        return None;
    }
    let default = default_branch(repo);
    let merged: HashSet<String> = default
        .as_deref()
        .and_then(|d| {
            git(
                repo,
                &["branch", "--merged", d, "--format=%(refname:short)"],
            )
            .ok()
        })
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default();
    let info = branches(repo);
    let prs = merged_prs(repo);
    for w in worktrees.iter_mut() {
        w.in_use = users_of(&w.path, procs);
        if let Some(b) = &w.branch {
            (w.last_commit, w.remote_gone) = info.get(b).copied().unwrap_or((None, false));
        }
        if w.main {
            continue;
        }
        w.merged = match (&w.branch, &default) {
            (Some(b), _) if merged.contains(b) => Some(Merged::Branch),
            (None, Some(d)) if !w.head.is_empty() && is_ancestor(repo, &w.head, d) => {
                Some(Merged::Branch)
            }
            _ => None,
        };
        // A squash merge: the branch's head is in a merged pull request
        // of that name, so nothing on it is left unmerged.
        if w.merged.is_none()
            && let (Some(b), Some(prs)) = (&w.branch, &prs)
        {
            w.merged =
                merged_by_pr(b, &w.head, prs, |a, o| is_ancestor(repo, a, o)).map(Merged::Pr);
        }
    }
    // Only what could be removed needs its files counted, in parallel:
    // `git status` takes a while in a large checkout.
    let candidates: Vec<usize> = worktrees
        .iter()
        .enumerate()
        .filter(|(_, w)| {
            !w.main && !w.locked && !w.missing && w.merged.is_some() && w.in_use.is_empty()
        })
        .map(|(i, _)| i)
        .collect();
    for chunk in candidates.chunks(PARALLEL) {
        let counted: Vec<(usize, Option<u32>)> = std::thread::scope(|s| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|&i| {
                    let path = worktrees[i].path.clone();
                    s.spawn(move || (i, changes(Path::new(&path))))
                })
                .collect();
            handles.into_iter().filter_map(|h| h.join().ok()).collect()
        });
        for (i, n) in counted {
            worktrees[i].changes = n;
        }
    }
    Some(Repo {
        path: repo.display().to_string(),
        default,
        prs_checked: prs.is_some(),
        worktrees,
    })
}

pub fn scan(repos: &[String], procs: &Procs) -> Vec<Repo> {
    repos
        .iter()
        .filter_map(|r| scan_repo(Path::new(r), procs))
        .collect()
}

/// Remove one worktree after checking again that it may go, then its
/// branch: `-d` when it is in the default branch, `-D` when a merged pull
/// request holds its every commit. A missing folder is pruned instead.
pub fn remove(repo: &str, path: &str, procs: &Procs) -> Result<String, String> {
    let fresh = scan_repo(Path::new(repo), procs).ok_or("the repository has no worktrees")?;
    let w = fresh
        .worktrees
        .iter()
        .find(|w| w.path == path)
        .ok_or("no longer a worktree")?;
    if let Some(why) = w.blocker() {
        return Err(format!("not removed: {why}"));
    }
    let dir = Path::new(repo);
    if w.missing {
        git(dir, &["worktree", "prune"])?;
    } else {
        git(dir, &["worktree", "remove", path])?;
    }
    let Some(branch) = &w.branch else {
        return Ok("worktree removed".into());
    };
    let flag = match w.merged {
        Some(Merged::Pr(_)) => "-D",
        _ => "-d",
    };
    Ok(match git(dir, &["branch", flag, branch]) {
        Ok(_) => format!("worktree and branch {branch} removed"),
        Err(e) => format!("worktree removed; branch {branch} kept ({e})"),
    })
}

pub fn json(repos: &[Repo], wall: u64) -> Value {
    json!(
        repos
            .iter()
            .map(|r| json!({
                "repo": r.path,
                "default": r.default,
                "prs_checked": r.prs_checked,
                "worktrees": r.worktrees.iter().map(|w| json!({
                    "path": w.path,
                    "branch": w.branch,
                    "main": w.main,
                    "locked": w.locked,
                    "missing": w.missing,
                    "merged": match w.merged {
                        Some(Merged::Branch) => json!("branch"),
                        Some(Merged::Pr(_)) => json!("pr"),
                        None => Value::Null,
                    },
                    "pr": match w.merged { Some(Merged::Pr(n)) => json!(n), _ => Value::Null },
                    "remote_gone": w.remote_gone,
                    "last_commit_ago_s": w.last_commit.map(|t| wall.saturating_sub(t)),
                    "changes": w.changes,
                    "in_use": w.in_use,
                    "blocker": w.blocker(),
                })).collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>()
    )
}

/// Every process we may inspect that works in a folder: its folder and
/// program name. Our own is left out.
pub fn processes() -> Vec<(String, String)> {
    let me = std::process::id() as i32;
    crate::sample::all_pids()
        .into_iter()
        .filter(|&pid| pid != me)
        .filter_map(|pid| {
            let cwd = crate::sample::cwd(pid).filter(|c| c != "/")?;
            let args = crate::sample::args(pid)?;
            let program = args.first()?;
            Some((
                cwd,
                program.rsplit('/').next().unwrap_or(program).to_string(),
            ))
        })
        .collect()
}

#[derive(Debug, Default)]
struct State {
    scanning: bool,
    /// Unix seconds of the last scan that finished.
    scanned_at: Option<u64>,
    repos: Vec<Repo>,
    /// Worktrees waiting to be removed or being removed, by path.
    removing: HashSet<String>,
    /// What happened to each removal asked for, by path: done, and what.
    results: BTreeMap<String, (bool, String)>,
}

/// The Worktrees page's work, off the main thread: a scan of a large
/// checkout takes seconds and removing a `node_modules` folder longer.
/// Removals run one at a time, so git never races itself over a repository.
#[derive(Clone, Default)]
pub struct Tab {
    state: Arc<Mutex<State>>,
    one_at_a_time: Arc<Mutex<()>>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Tab {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Look at `repos` again, unless a scan is already running.
    pub fn scan(&self, repos: Vec<String>) {
        {
            let mut s = self.lock();
            if s.scanning {
                return;
            }
            s.scanning = true;
        }
        let tab = self.clone();
        std::thread::spawn(move || {
            let found = scan(&repos, &processes());
            let mut s = tab.lock();
            s.repos = found;
            s.scanning = false;
            s.scanned_at = Some(now());
        });
    }

    /// Remove these worktrees of `repo`, each checked again first.
    pub fn remove(&self, repo: String, paths: Vec<String>) {
        let paths: Vec<String> = {
            let mut s = self.lock();
            paths
                .into_iter()
                .filter(|p| s.removing.insert(p.clone()))
                .collect()
        };
        if paths.is_empty() {
            return;
        }
        let tab = self.clone();
        std::thread::spawn(move || {
            let _one = tab.one_at_a_time.lock().unwrap_or_else(|p| p.into_inner());
            for path in paths {
                let result = remove(&repo, &path, &processes());
                let mut s = tab.lock();
                s.removing.remove(&path);
                if result.is_ok() {
                    for r in s.repos.iter_mut() {
                        r.worktrees.retain(|w| w.path != path);
                    }
                }
                let entry = match result {
                    Ok(m) => (true, m),
                    Err(e) => (false, e),
                };
                s.results.insert(path, entry);
            }
        });
    }

    /// Every worktree of `repo` the last scan found removable.
    pub fn removable(&self, repo: &str) -> Vec<String> {
        self.lock()
            .repos
            .iter()
            .filter(|r| r.path == repo)
            .flat_map(|r| &r.worktrees)
            .filter(|w| w.blocker().is_none())
            .map(|w| w.path.clone())
            .collect()
    }

    pub fn json(&self, wall: u64) -> Value {
        let s = self.lock();
        json!({
            "scanning": s.scanning,
            "scanned_ago_s": s.scanned_at.map(|t| wall.saturating_sub(t)),
            "repos": json(&s.repos, wall),
            "removing": s.removing.iter().collect::<Vec<_>>(),
            "results": s.results.iter().map(|(p, (ok, m))| (p.clone(), json!({ "ok": ok, "message": m })))
                .collect::<serde_json::Map<String, Value>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn the_list_names_each_worktree_and_what_git_says_of_it() {
        let text = "worktree /r\nHEAD aaa\nbranch refs/heads/main\n\n\
                    worktree /r.worktrees/a\nHEAD bbb\nbranch refs/heads/sc-1/a\n\n\
                    worktree /r.worktrees/b\nHEAD ccc\ndetached\nlocked agent busy\n\n\
                    worktree /r.worktrees/c\nHEAD ddd\nbranch refs/heads/c\nprunable gitdir file points to non-existent location\n";
        let w = parse_list(text);
        assert_eq!(w.len(), 4);
        assert!(w[0].main && !w[1].main);
        assert_eq!(w[1].branch.as_deref(), Some("sc-1/a"));
        assert_eq!(w[1].head, "bbb");
        assert_eq!((w[2].branch.as_deref(), w[2].locked), (None, true));
        assert!(w[3].missing);
        assert!(parse_list("").is_empty());
    }

    #[test]
    fn a_squash_merged_branch_is_merged_only_if_its_pull_request_holds_its_head() {
        let prs = vec![
            ("sc-1".to_string(), "aaa".to_string(), 10),
            ("sc-2".to_string(), "ccc".to_string(), 20),
            ("sc-2".to_string(), "ddd".to_string(), 21),
        ];
        let never = |_: &str, _: &str| false;
        assert_eq!(merged_by_pr("sc-1", "aaa", &prs, never), Some(10));
        // A commit made after the pull request merged is not in it.
        assert_eq!(merged_by_pr("sc-1", "bbb", &prs, never), None);
        // An earlier commit the pull request went on from is.
        assert_eq!(
            merged_by_pr("sc-1", "old", &prs, |a, o| (a, o) == ("old", "aaa")),
            Some(10)
        );
        // A branch name reused: the pull request with its head counts.
        assert_eq!(merged_by_pr("sc-2", "ddd", &prs, never), Some(21));
        assert_eq!(
            merged_by_pr("sc-3", "aaa", &prs, never),
            None,
            "another branch's commit"
        );
        assert_eq!(merged_by_pr("sc-1", "aaa", &[], never), None);
    }

    #[test]
    fn processes_in_a_worktree_or_below_use_it_but_a_neighbour_does_not() {
        let procs = vec![
            ("/r.worktrees/a".to_string(), "zsh".to_string()),
            ("/r.worktrees/a/web".to_string(), "node".to_string()),
            ("/r.worktrees/a/web".to_string(), "node".to_string()),
            ("/r.worktrees/ab".to_string(), "claude".to_string()),
        ];
        assert_eq!(users_of("/r.worktrees/a", &procs), ["node", "zsh"]);
        assert!(users_of("/r.worktrees/b", &procs).is_empty());
        assert!(users_of("/r.worktrees/a", &[]).is_empty());
    }

    #[test]
    fn only_a_merged_clean_unused_worktree_may_go() {
        let ok = Worktree {
            path: "/r.worktrees/a".into(),
            merged: Some(Merged::Branch),
            changes: Some(0),
            ..Worktree::default()
        };
        assert_eq!(ok.blocker(), None);
        let with = |f: fn(&mut Worktree)| {
            let mut w = ok.clone();
            f(&mut w);
            w.blocker()
        };
        assert_eq!(
            with(|w| w.main = true).as_deref(),
            Some("the main checkout")
        );
        assert_eq!(with(|w| w.locked = true).as_deref(), Some("locked"));
        assert_eq!(with(|w| w.merged = None).as_deref(), Some("not merged"));
        assert_eq!(
            with(|w| w.in_use = vec!["claude".into(), "zsh".into()]).as_deref(),
            Some("in use by claude, zsh")
        );
        assert_eq!(
            with(|w| w.changes = Some(1)).as_deref(),
            Some("1 uncommitted file")
        );
        assert_eq!(
            with(|w| w.changes = Some(3)).as_deref(),
            Some("3 uncommitted files")
        );
        assert_eq!(
            with(|w| w.changes = None).as_deref(),
            Some("changes not checked")
        );
        // A missing folder has nothing in it to lose: it is only pruned.
        assert_eq!(
            with(|w| {
                w.missing = true;
                w.merged = None;
                w.changes = None;
            }),
            None
        );
    }

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    /// A repository on `main` with worktrees: `merged` (in main), `open`
    /// (a commit main lacks) and `dirty` (merged, with a new file).
    fn fixture(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("hr-wt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // git reports real paths: /private/var, not /var.
        let root = root.canonicalize().unwrap();
        let repo = root.join("r");
        std::fs::create_dir_all(&repo).unwrap();
        sh(&repo, &["init", "-q", "-b", "main"]);
        sh(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
        for b in ["merged", "open", "dirty"] {
            let wt = root.join(b);
            sh(
                &repo,
                &["worktree", "add", "-q", "-b", b, wt.to_str().unwrap()],
            );
        }
        sh(
            &root.join("open"),
            &["commit", "-q", "--allow-empty", "-m", "work"],
        );
        std::fs::write(root.join("dirty/new.txt"), "x").unwrap();
        (root, repo)
    }

    fn find<'a>(r: &'a Repo, name: &str) -> &'a Worktree {
        r.worktrees
            .iter()
            .find(|w| w.branch.as_deref() == Some(name))
            .unwrap()
    }

    #[test]
    fn a_scan_tells_merged_from_open_and_counts_changes_only_where_it_matters() {
        let (root, repo) = fixture("scan");
        let r = scan_repo(&repo, &[]).unwrap();
        assert_eq!(r.default.as_deref(), Some("main"));
        assert_eq!(r.worktrees.len(), 4);
        assert!(r.worktrees[0].main);
        assert_eq!(find(&r, "merged").merged, Some(Merged::Branch));
        assert_eq!(find(&r, "merged").blocker(), None);
        assert_eq!(find(&r, "open").merged, None);
        assert_eq!(
            find(&r, "open").changes,
            None,
            "not counted: it can't go anyway"
        );
        assert_eq!(find(&r, "dirty").changes, Some(1));
        assert!(find(&r, "merged").last_commit.is_some());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_repository_with_no_worktrees_or_none_at_all_is_left_out() {
        let root = std::env::temp_dir().join(format!("hr-wt-plain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        assert!(scan_repo(&root, &[]).is_none(), "not a repository");
        sh(&root, &["init", "-q", "-b", "main"]);
        sh(&root, &["commit", "-q", "--allow-empty", "-m", "base"]);
        assert!(scan_repo(&root, &[]).is_none(), "only its own checkout");
        assert!(scan(&[root.display().to_string(), "/nonexistent".into()], &[]).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn removing_takes_the_worktree_and_its_branch_once() {
        let (root, repo) = fixture("remove");
        let (repo_s, wt) = (
            repo.display().to_string(),
            root.join("merged").display().to_string(),
        );
        let said = remove(&repo_s, &wt, &[]).unwrap();
        assert_eq!(said, "worktree and branch merged removed");
        assert!(!Path::new(&wt).exists());
        assert!(git(&repo, &["rev-parse", "--verify", "--quiet", "merged"]).is_err());
        // Pressed again: it is gone, and nothing else is touched.
        assert_eq!(
            remove(&repo_s, &wt, &[]).unwrap_err(),
            "no longer a worktree"
        );
        assert!(root.join("open").exists() && root.join("dirty").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn removal_refuses_open_changed_or_busy_worktrees_and_leaves_them_whole() {
        let (root, repo) = fixture("refuse");
        let repo_s = repo.display().to_string();
        let path = |b: &str| root.join(b).display().to_string();
        assert_eq!(
            remove(&repo_s, &path("open"), &[]).unwrap_err(),
            "not removed: not merged"
        );
        assert_eq!(
            remove(&repo_s, &path("dirty"), &[]).unwrap_err(),
            "not removed: 1 uncommitted file"
        );
        let busy = vec![(path("merged"), "claude".to_string())];
        assert_eq!(
            remove(&repo_s, &path("merged"), &busy).unwrap_err(),
            "not removed: in use by claude"
        );
        assert_eq!(
            remove(&repo_s, &repo_s, &[]).unwrap_err(),
            "not removed: the main checkout"
        );
        for b in ["open", "dirty", "merged"] {
            assert!(root.join(b).exists(), "{b} kept");
        }
        assert!(root.join("dirty/new.txt").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_worktree_whose_folder_was_deleted_is_pruned() {
        let (root, repo) = fixture("prune");
        std::fs::remove_dir_all(root.join("open")).unwrap();
        let r = scan_repo(&repo, &[]).unwrap();
        let w = find(&r, "open");
        assert!(w.missing);
        assert_eq!(w.blocker(), None);
        let said = remove(&repo.display().to_string(), &w.path, &[]).unwrap();
        // Its branch has a commit main lacks: git keeps it.
        assert!(
            said.starts_with("worktree removed; branch open kept"),
            "{said}"
        );
        assert_eq!(scan_repo(&repo, &[]).unwrap().worktrees.len(), 3);
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn settle(tab: &Tab) -> Value {
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            let v = tab.json(now());
            let busy = v["scanning"] == true || !v["removing"].as_array().unwrap().is_empty();
            if !busy || Instant::now() > end {
                return v;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn the_tab_scans_then_removes_what_may_go_once_even_if_asked_twice() {
        let (root, repo) = fixture("tab");
        let repo_s = repo.display().to_string();
        let tab = Tab::default();
        assert_eq!(
            tab.json(0)["scanned_ago_s"],
            Value::Null,
            "nothing scanned yet"
        );
        tab.scan(vec![repo_s.clone()]);
        let v = settle(&tab);
        assert_eq!(v["repos"][0]["worktrees"].as_array().unwrap().len(), 4);
        let merged = root.join("merged").display().to_string();
        assert_eq!(tab.removable(&repo_s), std::slice::from_ref(&merged));
        assert!(tab.removable("/elsewhere").is_empty());
        // The same worktree twice in one ask, and asked again: removed once.
        tab.remove(repo_s.clone(), vec![merged.clone(), merged.clone()]);
        tab.remove(repo_s.clone(), vec![merged.clone()]);
        let v = settle(&tab);
        let result = &v["results"][&merged];
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(
            v["repos"][0]["worktrees"].as_array().unwrap().len(),
            3,
            "gone from the list"
        );
        // Asked after it is gone: says so, and nothing else changes.
        tab.remove(repo_s.clone(), vec![merged.clone()]);
        let v = settle(&tab);
        assert_eq!(v["results"][&merged]["message"], "no longer a worktree");
        assert!(root.join("open").exists() && root.join("dirty").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_page_gets_why_each_worktree_stays() {
        let r = Repo {
            path: "/r".into(),
            default: Some("origin/main".into()),
            prs_checked: true,
            worktrees: vec![Worktree {
                path: "/r.worktrees/a".into(),
                branch: Some("a".into()),
                merged: Some(Merged::Pr(42)),
                changes: Some(2),
                last_commit: Some(900),
                ..Worktree::default()
            }],
        };
        let v = json(&[r], 1_000);
        let w = &v[0]["worktrees"][0];
        assert_eq!(
            (w["merged"].as_str(), w["pr"].as_u64()),
            (Some("pr"), Some(42))
        );
        assert_eq!(w["blocker"], "2 uncommitted files");
        assert_eq!(w["last_commit_ago_s"], 100);
        assert_eq!(json(&[], 0), json!([]));
    }
}
