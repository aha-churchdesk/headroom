//! Where a command runs: its repository, worktree and our own executable.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use headroom_core::classify::basename;
use headroom_core::tweaks::{self, Probe, Tool, Tunable};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitContext {
    /// The main checkout, shared by all its worktrees, so they share profiles.
    pub repo: String,
    /// Where in the checkout the command runs; empty at the top.
    pub subdir: String,
    /// The worktree's directory name, shown in the queue.
    pub label: String,
}

/// Read the repository around `cwd`. Outside a repository the directory
/// stands in for one.
pub fn git_context(cwd: &Path) -> GitContext {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let fallback = || GitContext {
        repo: cwd.display().to_string(),
        subdir: String::new(),
        label: basename(&cwd.display().to_string()).to_string(),
    };
    let out = Command::new("git")
        .arg("-C")
        .arg(&cwd)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Some(out) = out.ok().filter(|o| o.status.success()) else {
        return fallback();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let (Some(top), Some(common)) = (lines.next(), lines.next()) else {
        return fallback();
    };
    let top = PathBuf::from(top);
    let common = PathBuf::from(common);
    // `<repo>/.git` for a normal checkout and for its worktrees alike.
    let repo = if common.file_name().is_some_and(|n| n == ".git") {
        common.parent().map(Path::to_path_buf).unwrap_or(common)
    } else {
        common
    };
    let subdir = cwd
        .strip_prefix(&top)
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    GitContext {
        repo: repo.display().to_string(),
        subdir,
        label: basename(&top.display().to_string()).to_string(),
    }
}

/// Does the repository at `cwd` (or `cwd/dir`, for `git -C dir`) have git
/// hook `hook`? git says where the hook lives, which follows worktrees and
/// `core.hooksPath` (husky). Like git, a file that isn't executable (the
/// `.sample`s) doesn't count.
pub fn git_hook_installed(cwd: &Path, dir: Option<&str>, hook: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let at = dir.map_or_else(|| cwd.to_path_buf(), |d| cwd.join(d));
    let out = Command::new("git")
        .arg("-C")
        .arg(&at)
        .args(["rev-parse", "--path-format=absolute", "--git-path"])
        .arg(format!("hooks/{hook}"))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Some(out) = out.ok().filter(|o| o.status.success()) else {
        return false;
    };
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `dir` and its parents up to the checkout's top (the first with `.git`).
fn up_to_top(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for d in dir.ancestors() {
        out.push(d.to_path_buf());
        if d.join(".git").exists() {
            break;
        }
    }
    out
}

/// The nearest package.json's text for script `name`.
fn npm_script(cwd: &Path, name: &str) -> Option<String> {
    let text = up_to_top(cwd)
        .iter()
        .find_map(|d| std::fs::read_to_string(d.join("package.json")).ok())?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    json.get("scripts")?.get(name)?.as_str().map(String::from)
}

/// Can this command be told to use fewer workers? Reads the npm script it
/// runs and the tool's config files between `cwd` and the checkout's top.
pub fn tunable(line: &str, cwd: &Path) -> Option<Tunable> {
    let script = tweaks::script_name(line).and_then(|n| npm_script(cwd, &n));
    let dirs = up_to_top(cwd);
    let configs = |tool: Tool| {
        dirs.iter()
            .flat_map(|d| tool.config_files().iter().map(move |f| d.join(f)))
            .filter_map(|p| std::fs::read_to_string(p).ok())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
    tweaks::detect(&Probe {
        line,
        script: script.as_deref(),
        configs: &configs,
        env_set: &|v| std::env::var_os(v).is_some_and(|x| !x.is_empty()),
        cores,
    })
}

/// The path agents' hooks should call. Homebrew runs us from a versioned
/// Cellar path that disappears on upgrade, so prefer its stable link.
pub fn stable_exe() -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok());
    let Some(exe) = exe else {
        return "headroom".into();
    };
    stable_from(&exe, |p| p.exists())
}

pub fn stable_from(exe: &Path, exists: impl Fn(&Path) -> bool) -> String {
    let s = exe.display().to_string();
    if let Some((prefix, _)) = s.split_once("/Cellar/headroom/") {
        let link = PathBuf::from(prefix).join("bin/headroom");
        if exists(&link) {
            return link.display().to_string();
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("headroom-ctx-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p.canonicalize().unwrap()
    }

    #[test]
    fn worktrees_share_the_main_checkout_as_repo() {
        let root = tmp("wt");
        let main = root.join("app");
        std::fs::create_dir_all(main.join("web")).unwrap();
        sh(&main, &["init", "-q"]);
        sh(
            &main,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "x",
            ],
        );
        sh(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                root.join("app.worktrees/sc-1").to_str().unwrap(),
            ],
        );
        std::fs::create_dir_all(root.join("app.worktrees/sc-1/web")).unwrap();

        let a = git_context(&main.join("web"));
        let b = git_context(&root.join("app.worktrees/sc-1/web"));
        assert_eq!(a.repo, main.display().to_string());
        assert_eq!(b.repo, a.repo);
        assert_eq!((a.label.as_str(), a.subdir.as_str()), ("app", "web"));
        assert_eq!((b.label.as_str(), b.subdir.as_str()), ("sc-1", "web"));
    }

    #[test]
    fn outside_a_repository_the_directory_stands_in() {
        let d = tmp("plain");
        let c = git_context(&d);
        assert_eq!(c.repo, d.display().to_string());
        assert_eq!(c.subdir, "");
        assert!(c.label.starts_with("headroom-ctx-plain"));
    }

    #[test]
    fn homebrew_cellar_paths_resolve_to_the_stable_link() {
        let cellar = Path::new("/opt/homebrew/Cellar/headroom/0.1.0/bin/headroom");
        assert_eq!(stable_from(cellar, |_| true), "/opt/homebrew/bin/headroom");
        assert_eq!(stable_from(cellar, |_| false), cellar.display().to_string());
        let dev = Path::new("/Users/me/src/headroom/target/release/headroom");
        assert_eq!(stable_from(dev, |_| true), dev.display().to_string());
    }

    fn hook(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a_git_hook_counts_when_git_would_run_it() {
        let repo = tmp("hooks");
        sh(&repo, &["init", "-q"]);
        // A fresh repository has only samples.
        assert!(!git_hook_installed(&repo, None, "pre-commit"));
        hook(&repo.join(".git/hooks/pre-commit"), 0o755);
        assert!(git_hook_installed(&repo, None, "pre-commit"));
        assert!(!git_hook_installed(&repo, None, "pre-push"));
        // Not executable: git skips it, so do we.
        hook(&repo.join(".git/hooks/pre-push"), 0o644);
        assert!(!git_hook_installed(&repo, None, "pre-push"));
        // From a subdirectory, and through `git -C`.
        std::fs::create_dir_all(repo.join("web")).unwrap();
        assert!(git_hook_installed(&repo.join("web"), None, "pre-commit"));
        assert!(git_hook_installed(
            repo.parent().unwrap(),
            Some(repo.file_name().unwrap().to_str().unwrap()),
            "pre-commit"
        ));
    }

    #[test]
    fn hooks_are_found_from_worktrees_and_through_hooks_path() {
        let repo = tmp("hooks-wt");
        sh(&repo, &["init", "-q", "-b", "main"]);
        sh(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "x",
            ],
        );
        hook(&repo.join(".git/hooks/pre-push"), 0o755);
        let wt = repo.with_file_name(format!(
            "{}-wt",
            repo.file_name().unwrap().to_str().unwrap()
        ));
        let _ = std::fs::remove_dir_all(&wt);
        sh(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        assert!(
            git_hook_installed(&wt, None, "pre-push"),
            "a worktree shares the main hooks"
        );
        // husky: core.hooksPath moves them, and the default ones stop counting.
        sh(&repo, &["config", "core.hooksPath", ".husky/_"]);
        assert!(!git_hook_installed(&repo, None, "pre-push"));
        hook(&repo.join(".husky/_/pre-commit"), 0o755);
        assert!(git_hook_installed(&repo, None, "pre-commit"));
        // A relative hooksPath is per checkout: until the worktree has its
        // own (husky writes it on install), git runs nothing there either.
        assert!(!git_hook_installed(&wt, None, "pre-commit"));
        hook(&wt.join(".husky/_/pre-commit"), 0o755);
        assert!(git_hook_installed(&wt, None, "pre-commit"));
    }

    #[test]
    fn outside_a_repository_there_are_no_hooks() {
        let dir = tmp("no-repo");
        assert!(!git_hook_installed(&dir, None, "pre-commit"));
        assert!(!git_hook_installed(
            &dir.join("missing"),
            None,
            "pre-commit"
        ));
    }

    fn checkout(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("hr-tune-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("apps/web")).unwrap();
        root
    }

    #[test]
    fn finds_vitest_behind_the_nearest_package_json_script() {
        let root = checkout("script");
        std::fs::write(
            root.join("apps/web/package.json"),
            r#"{"scripts":{"test":"vitest run","lint":"eslint ."}}"#,
        )
        .unwrap();
        let web = root.join("apps/web");
        assert_eq!(
            tunable("pnpm test", &web).map(|t| t.tool),
            Some(Tool::Vitest)
        );
        assert_eq!(tunable("pnpm lint", &web), None);
        assert_eq!(tunable("pnpm missing", &web), None);
    }

    #[test]
    fn a_worker_setting_in_a_config_file_up_to_the_checkout_top_wins() {
        let root = checkout("config");
        let web = root.join("apps/web");
        assert!(tunable("vitest run", &web).is_some());
        std::fs::write(root.join("vitest.config.ts"), "test: { maxWorkers: 4 }").unwrap();
        assert_eq!(tunable("vitest run", &web), None);
    }

    #[test]
    fn files_above_the_checkout_are_not_read() {
        let root = checkout("above");
        let outer = root
            .parent()
            .unwrap()
            .join(format!("hr-tune-outer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outer);
        std::fs::create_dir_all(&outer).unwrap();
        let inner = outer.join("repo");
        std::fs::create_dir_all(inner.join(".git")).unwrap();
        std::fs::write(outer.join("vitest.config.ts"), "maxWorkers: 1").unwrap();
        assert!(tunable("vitest run", &inner).is_some());
    }

    #[test]
    fn a_broken_package_json_means_no_script_not_a_crash() {
        let root = checkout("broken");
        std::fs::write(root.join("package.json"), "{ nope").unwrap();
        assert_eq!(tunable("pnpm test", &root), None);
    }
}
