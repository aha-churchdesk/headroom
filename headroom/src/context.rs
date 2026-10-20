//! Where a command runs: its repository, worktree and our own executable.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use headroom_core::classify::basename;

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
}
