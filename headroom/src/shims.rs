//! PATH shims: `pnpm`, `cargo`, `turbo` and friends as links to `headroom`,
//! so heavy commands queue from terminals, git hooks and agents without a
//! hook of their own.
//!
//! Called as one of those names, `headroom` classifies the whole command. A
//! heavy one runs through the queue as `headroom run`; anything else, and
//! anything inside a job (`HEADROOM_JOB`), execs the real program: the next
//! one on PATH that isn't a link back to Headroom.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use headroom_core::classify::{Class, classify_in};
use headroom_core::shell;
use headroom_core::store::Store;

use crate::context::git_hook_installed;
use crate::run::{self, JOB_ENV, RunOpts};

/// The programs shimmed: package managers and the heavy tools they run.
pub const NAMES: &[&str] = &[
    "pnpm",
    "npm",
    "npx",
    "yarn",
    "bun",
    "bunx",
    "vitest",
    "jest",
    "playwright",
    "cargo",
    "tsc",
    "turbo",
    "nx",
    "xcodebuild",
    "docker",
];

/// The shim this process was started as, if any.
pub fn called_as(argv0: &str) -> Option<&'static str> {
    let name = argv0.rsplit('/').next().unwrap_or(argv0);
    NAMES.iter().copied().find(|n| *n == name)
}

pub fn default_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".local/share/headroom/shims")
}

fn executable(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// A file that resolves to a Headroom binary: one of our shims, wherever it
/// lives. Skipping these (not only our own directory) is what stops a shim
/// from finding itself, or another copy of Headroom, forever.
fn is_headroom(p: &Path) -> bool {
    p.canonicalize().is_ok_and(|c| {
        c.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == "headroom" || n.starts_with("headroom-"))
    })
}

/// The real `name`: the first on `path` that isn't Headroom.
pub fn find_real(name: &str, path: &str) -> Option<PathBuf> {
    path.split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(name))
        .find(|p| executable(p) && !is_headroom(p))
}

fn exec(name: &str, real: &Path, args: &[String]) -> i32 {
    let e = Command::new(real).arg0(name).args(args).exec();
    eprintln!("headroom: can't run {}: {e}", real.display());
    126
}

/// Run as shim `name` with `args`.
pub fn shim(name: &str, args: &[String]) -> i32 {
    let path = std::env::var("PATH").unwrap_or_default();
    let Some(real) = find_real(name, &path) else {
        eprintln!("headroom: {name}: not found on PATH, apart from Headroom's shims");
        return 127;
    };
    if std::env::var_os(JOB_ENV).is_some() {
        return exec(name, &real, args);
    }
    let cfg = Store::open().map(|s| s.load_config()).unwrap_or_default();
    let mut words = vec![name.to_string()];
    words.extend(args.iter().cloned());
    let cwd = std::env::current_dir().unwrap_or_default();
    let class = classify_in(&shell::join(&words), &cfg, &|d, h| {
        git_hook_installed(&cwd, d, h)
    });
    if !matches!(class, Class::Heavy { .. }) {
        return exec(name, &real, args);
    }
    let mut argv = vec![real.display().to_string()];
    argv.extend(args.iter().cloned());
    run::run(RunOpts {
        argv,
        ..RunOpts::default()
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Created,
    Updated,
    Unchanged,
    /// Something that isn't our link is there; left alone.
    Kept,
}

/// Link every name in `dir` to `exe`. Run again to repoint them after the
/// binary moves.
pub fn install(dir: &Path, exe: &Path) -> io::Result<Vec<(&'static str, Step)>> {
    std::fs::create_dir_all(dir)?;
    let mut out = Vec::new();
    for &name in NAMES {
        let link = dir.join(name);
        let step = match std::fs::symlink_metadata(&link) {
            Err(_) => Step::Created,
            Ok(m) if !m.file_type().is_symlink() => Step::Kept,
            Ok(_) if std::fs::read_link(&link)? == exe => Step::Unchanged,
            Ok(_) if is_headroom(&link) || !link.exists() => {
                std::fs::remove_file(&link)?;
                Step::Updated
            }
            Ok(_) => Step::Kept,
        };
        if matches!(step, Step::Created | Step::Updated) {
            std::os::unix::fs::symlink(exe, &link)?;
        }
        out.push((name, step));
    }
    Ok(out)
}

/// Remove our links from `dir`, and `dir` once it's empty. Returns how many
/// went.
pub fn uninstall(dir: &Path) -> io::Result<usize> {
    let mut n = 0;
    for &name in NAMES {
        let link = dir.join(name);
        let ours = std::fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink())
            && (is_headroom(&link) || !link.exists());
        if ours {
            std::fs::remove_file(&link)?;
            n += 1;
        }
    }
    let _ = std::fs::remove_dir(dir);
    Ok(n)
}

#[derive(Debug, PartialEq, Eq)]
pub enum OnPath {
    /// Not on PATH: the shims do nothing.
    Missing,
    /// On PATH, but these names are found earlier, in these directories.
    Behind(Vec<(&'static str, PathBuf)>),
    First,
}

/// Where `dir` sits on `path`, for the names that exist somewhere on it.
pub fn on_path(dir: &Path, path: &str) -> OnPath {
    let same = |d: &str| {
        Path::new(d) == dir || Path::new(d).canonicalize().ok() == dir.canonicalize().ok()
    };
    let dirs: Vec<&str> = path.split(':').filter(|d| !d.is_empty()).collect();
    let Some(at) = dirs.iter().position(|d| same(d)) else {
        return OnPath::Missing;
    };
    let behind: Vec<_> = NAMES
        .iter()
        .filter_map(|&name| {
            dirs[..at]
                .iter()
                .map(|d| Path::new(d).join(name))
                .find(|p| executable(p) && !is_headroom(p))
                .map(|p| (name, p.parent().unwrap_or(Path::new("/")).to_path_buf()))
        })
        .collect();
    if behind.is_empty() {
        OnPath::First
    } else {
        OnPath::Behind(behind)
    }
}

/// `headroom shims install|uninstall|status [--dir DIR]`.
pub fn manage(action: &str, dir: &Path, exe: &Path) -> i32 {
    let path = std::env::var("PATH").unwrap_or_default();
    let export = format!("export PATH=\"{}:$PATH\"", dir.display());
    match action {
        "install" => {
            let steps = match install(dir, exe) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("headroom: can't install shims in {}: {e}", dir.display());
                    return 1;
                }
            };
            let count = |want: Step| steps.iter().filter(|(_, s)| *s == want).count();
            println!(
                "shims in {}: {} created, {} repointed, {} unchanged",
                dir.display(),
                count(Step::Created),
                count(Step::Updated),
                count(Step::Unchanged)
            );
            for (name, _) in steps.iter().filter(|(_, s)| *s == Step::Kept) {
                println!("  kept {name}: something else is there, not ours");
            }
            match on_path(dir, &path) {
                OnPath::First => println!("already first on your PATH"),
                _ => {
                    println!(
                        "they work once the directory comes first on PATH. Add this to your shell config:"
                    );
                    println!("  {export}");
                }
            }
            0
        }
        "uninstall" => match uninstall(dir) {
            Ok(n) => {
                println!("removed {n} shims from {}", dir.display());
                if !matches!(on_path(dir, &path), OnPath::Missing) {
                    println!("you can drop it from PATH in your shell config too");
                }
                0
            }
            Err(e) => {
                eprintln!("headroom: {e}");
                1
            }
        },
        "status" => {
            let installed = NAMES.iter().filter(|n| is_headroom(&dir.join(n))).count();
            println!("{installed} of {} shims in {}", NAMES.len(), dir.display());
            match on_path(dir, &path) {
                OnPath::First => println!("first on PATH: heavy commands queue"),
                OnPath::Missing => {
                    println!("not on PATH: the shims do nothing. Add this to your shell config:");
                    println!("  {export}");
                }
                OnPath::Behind(v) => {
                    println!("on PATH, but these are found first and skip the queue:");
                    for (name, d) in v {
                        println!("  {name} in {}", d.display());
                    }
                    println!("move the shims to the front: {export}");
                }
            }
            0
        }
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("hr-shims-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p.canonicalize().unwrap()
    }

    fn file(p: &Path, mode: u32) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A stand-in for the headroom binary: its name is what marks it.
    fn fake_headroom(root: &Path) -> PathBuf {
        let exe = root.join("app/headroom");
        file(&exe, 0o755);
        exe
    }

    fn path(dirs: &[&Path]) -> String {
        dirs.iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(":")
    }

    #[test]
    fn shim_names_come_from_argv0() {
        assert_eq!(
            called_as("/Users/me/.local/share/headroom/shims/pnpm"),
            Some("pnpm")
        );
        assert_eq!(called_as("cargo"), Some("cargo"));
        assert_eq!(
            called_as("/Applications/Headroom.app/Contents/MacOS/headroom"),
            None
        );
        assert_eq!(called_as("git"), None);
        assert_eq!(called_as(""), None);
    }

    #[test]
    fn the_real_program_is_the_first_on_path_that_isnt_headroom() {
        let root = tmp("real");
        let exe = fake_headroom(&root);
        let shims = root.join("shims");
        install(&shims, &exe).unwrap();
        let (a, b) = (root.join("a"), root.join("b"));
        file(&b.join("pnpm"), 0o755);
        std::fs::create_dir_all(&a).unwrap();
        assert_eq!(
            find_real("pnpm", &path(&[&shims, &a, &b])),
            Some(b.join("pnpm"))
        );
        // Not executable, or a directory: skipped like the shell skips them.
        file(&a.join("pnpm"), 0o644);
        std::fs::create_dir_all(a.join("cargo")).unwrap();
        assert_eq!(
            find_real("pnpm", &path(&[&shims, &a, &b])),
            Some(b.join("pnpm"))
        );
        assert_eq!(find_real("cargo", &path(&[&shims, &a, &b])), None);
    }

    #[test]
    fn a_second_copy_of_the_shims_is_never_the_real_program() {
        let root = tmp("twice");
        let exe = fake_headroom(&root);
        let (one, two) = (root.join("one"), root.join("two"));
        install(&one, &exe).unwrap();
        install(&two, &exe).unwrap();
        // Only shims on PATH, in two places and twice over: nothing to run.
        assert_eq!(find_real("npm", &path(&[&one, &two, &one])), None);
        assert_eq!(find_real("npm", ""), None);
    }

    #[test]
    fn install_links_every_name_and_is_safe_to_repeat() {
        let root = tmp("install");
        let exe = fake_headroom(&root);
        let dir = root.join("shims");
        // Someone's own file is in the way of one name.
        file(&dir.join("docker"), 0o755);
        let steps = install(&dir, &exe).unwrap();
        assert_eq!(steps.len(), NAMES.len());
        assert!(steps.contains(&("docker", Step::Kept)));
        assert_eq!(
            steps.iter().filter(|(_, s)| *s == Step::Created).count(),
            NAMES.len() - 1
        );
        assert_eq!(std::fs::read_link(dir.join("pnpm")).unwrap(), exe);
        let again = install(&dir, &exe).unwrap();
        assert!(
            again
                .iter()
                .all(|(n, s)| *s == Step::Unchanged || *n == "docker")
        );
        // The binary moved: the links follow it.
        let moved = root.join("moved/headroom");
        file(&moved, 0o755);
        std::fs::remove_file(&exe).unwrap();
        let after = install(&dir, &moved).unwrap();
        assert!(after.contains(&("pnpm", Step::Updated)));
        assert_eq!(std::fs::read_link(dir.join("pnpm")).unwrap(), moved);
    }

    #[test]
    fn uninstall_removes_only_our_links() {
        let root = tmp("uninstall");
        let exe = fake_headroom(&root);
        let dir = root.join("shims");
        file(&dir.join("docker"), 0o755);
        let other = root.join("bin/yarn");
        file(&other, 0o755);
        install(&dir, &exe).unwrap();
        std::fs::remove_file(dir.join("yarn")).unwrap();
        std::os::unix::fs::symlink(&other, dir.join("yarn")).unwrap();
        assert_eq!(uninstall(&dir).unwrap(), NAMES.len() - 2);
        assert!(dir.join("docker").exists() && dir.join("yarn").exists());
        // Again: nothing left of ours, nothing removed.
        assert_eq!(uninstall(&dir).unwrap(), 0);
        // Once only ours were there, the directory goes too.
        let clean = root.join("clean");
        install(&clean, &exe).unwrap();
        uninstall(&clean).unwrap();
        assert!(!clean.exists());
    }

    #[test]
    fn status_says_whether_the_shims_come_first() {
        let root = tmp("status");
        let exe = fake_headroom(&root);
        let dir = root.join("shims");
        install(&dir, &exe).unwrap();
        let brew = root.join("brew");
        file(&brew.join("pnpm"), 0o755);
        file(&brew.join("cargo"), 0o755);
        assert_eq!(on_path(&dir, &path(&[&brew])), OnPath::Missing);
        assert_eq!(on_path(&dir, ""), OnPath::Missing);
        assert_eq!(on_path(&dir, &path(&[&dir, &brew])), OnPath::First);
        assert_eq!(
            on_path(&dir, &path(&[&brew, &dir])),
            OnPath::Behind(vec![("pnpm", brew.clone()), ("cargo", brew.clone())])
        );
        // A directory before ours without any of the names doesn't matter.
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(on_path(&dir, &path(&[&empty, &dir, &brew])), OnPath::First);
    }
}
