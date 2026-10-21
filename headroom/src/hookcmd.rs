//! `headroom hook`: answer an agent's hook, or install, remove and check hooks.

use std::io::Read;
use std::path::{Path, PathBuf};

use headroom_core::hookio::{self, Agent};
use headroom_core::hooksettings::{self, Change, Scope, Status};
use headroom_core::store::Store;

use crate::context::{git_hook_installed, stable_exe};

/// The hook itself: payload on stdin, answer (or nothing) on stdout.
/// Always exits 0: a hook failure must never stop the agent's command.
pub fn answer(agent: Agent) -> i32 {
    let mut payload = String::new();
    if std::io::stdin().read_to_string(&mut payload).is_err() {
        return 0;
    }
    let cfg = Store::open().map(|s| s.load_config()).unwrap_or_default();
    // The agent's cwd, or ours: hooks run in the agent's directory.
    let hooked = |cwd: Option<&str>, dir: Option<&str>, hook: &str| {
        let base = cwd
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        git_hook_installed(&base, dir, hook)
    };
    let out = hookio::handle(agent, &payload, &stable_exe(), &cfg, &hooked);
    if !out.is_empty() {
        println!("{out}");
    }
    0
}

pub struct Target {
    pub agents: Vec<Agent>,
    pub scope: Scope,
    pub dry_run: bool,
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn tilde(p: &Path, home: &Path) -> String {
    match p.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

/// What to do after an install, per agent.
fn next_step(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "new and running sessions pick it up (restart one if it doesn't)",
        Agent::Codex => "Codex asks you to review new hooks: approve it there before it runs",
        Agent::Pi => "run /reload in pi, or restart it",
    }
}

pub fn manage(action: &str, t: &Target) -> i32 {
    let home = home();
    let project = std::env::current_dir().unwrap_or_default();
    let exe = stable_exe();
    let mut failed = false;
    for &agent in &t.agents {
        let name = agent.name();
        if !hooksettings::agent_present(agent, &home) {
            println!("{name:<7}not found on this Mac, skipped");
            continue;
        }
        let Some(path) = hooksettings::settings_path(agent, t.scope, &home, &project) else {
            println!("{name:<7}has no {:?} scope, skipped", t.scope);
            continue;
        };
        let shown = tilde(&path, &home);
        let result = match action {
            "install" => hooksettings::install(agent, &path, &exe, t.dry_run).map(Some),
            "uninstall" => hooksettings::uninstall(agent, &path, t.dry_run).map(Some),
            _ => {
                match hooksettings::status(agent, &path, &exe) {
                    Ok(Status::Installed) => println!("{name:<7}installed      {shown}"),
                    Ok(Status::NotInstalled) => println!("{name:<7}not installed  {shown}"),
                    Ok(Status::Stale { exe: old }) => {
                        println!(
                            "{name:<7}stale          {shown} points at {old}; run `headroom hook install`"
                        )
                    }
                    Err(e) => {
                        println!("{name:<7}error          {e}");
                        failed = true;
                    }
                }
                Ok(None)
            }
        };
        match result {
            Ok(Some(r)) => {
                let verb = match (r.change, t.dry_run) {
                    (Change::Unchanged, _) => "unchanged",
                    (Change::Added, false) => "installed",
                    (Change::Updated, false) => "updated",
                    (Change::Removed, false) => "removed",
                    (Change::Added, true) => "would install",
                    (Change::Updated, true) => "would update",
                    (Change::Removed, true) => "would remove",
                };
                println!("{name:<7}{verb:<15}{shown}");
                if t.dry_run && r.change != Change::Unchanged {
                    print!(
                        "{}",
                        diff(
                            r.before.as_deref().unwrap_or(""),
                            r.after.as_deref().unwrap_or("")
                        )
                    );
                }
                if !t.dry_run
                    && action == "install"
                    && matches!(r.change, Change::Added | Change::Updated)
                {
                    println!("       → {}", next_step(agent));
                }
            }
            Ok(None) => {}
            Err(e) => {
                println!("{name:<7}error          {e}");
                failed = true;
            }
        }
    }
    i32::from(failed)
}

/// A line diff, `-`/`+` for changes and two lines of context around them.
pub fn diff(before: &str, after: &str) -> String {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    // Longest common subsequence table, filled from the end.
    let mut l = vec![vec![0u32; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            l[i][j] = if a[i] == b[j] {
                l[i + 1][j + 1] + 1
            } else {
                l[i + 1][j].max(l[i][j + 1])
            };
        }
    }
    let mut ops: Vec<(char, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            ops.push((' ', a[i]));
            (i, j) = (i + 1, j + 1);
        } else if j < b.len() && (i == a.len() || l[i][j + 1] >= l[i + 1][j]) {
            ops.push(('+', b[j]));
            j += 1;
        } else {
            ops.push(('-', a[i]));
            i += 1;
        }
    }
    let near = |k: usize| {
        ops[k.saturating_sub(2)..(k + 3).min(ops.len())]
            .iter()
            .any(|(c, _)| *c != ' ')
    };
    let mut out = String::new();
    let mut gap = false;
    for (k, (c, line)) in ops.iter().enumerate() {
        if *c != ' ' || near(k) {
            if gap {
                out.push_str("       …\n");
                gap = false;
            }
            out.push_str(&format!("       {c} {line}\n"));
        } else {
            gap = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_shows_changes_with_context_and_elides_the_rest() {
        let before = "a\nb\nc\nd\ne\nf\ng\n";
        let after = "a\nb\nc\nd\ne\nf\nNEW\ng\n";
        let d = diff(before, after);
        assert_eq!(
            d,
            "       …\n         e\n         f\n       + NEW\n         g\n"
        );
    }

    #[test]
    fn diff_of_identical_text_is_empty() {
        assert_eq!(diff("x\ny\n", "x\ny\n"), "");
    }

    #[test]
    fn diff_from_nothing_is_all_additions() {
        assert_eq!(diff("", "a\nb\n"), "       + a\n       + b\n");
    }

    #[test]
    fn tilde_shortens_home_paths_only() {
        let h = Path::new("/Users/me");
        assert_eq!(
            tilde(Path::new("/Users/me/.claude/settings.json"), h),
            "~/.claude/settings.json"
        );
        assert_eq!(tilde(Path::new("/etc/x"), h), "/etc/x");
    }
}
