//! Where a command runs: its repository, worktree and our own executable.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use headroom_core::classify::basename;
use headroom_core::shell::simple_commands;
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

/// The nearest package.json's script and the directory npm runs it in.
fn npm_script(cwd: &Path, name: &str) -> Option<(PathBuf, String)> {
    let (dir, text) = up_to_top(cwd).into_iter().find_map(|d| {
        std::fs::read_to_string(d.join("package.json"))
            .ok()
            .map(|text| (d, text))
    })?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some((dir, json.get("scripts")?.get(name)?.as_str()?.to_string()))
}

/// Resolve only literal paths. Shell expansion and CDPATH can change a
/// relative `cd` target, so those cases must leave worker settings alone.
fn literal_path(base: &Path, path: &str) -> Option<PathBuf> {
    if path.is_empty()
        || path.contains(['$', '`', '~', '*', '?', '[', ']', '{', '}'])
        // Shell and Node path normalization can treat symlink/.. differently
        // from filesystem traversal. Do not guess which config that selects.
        || Path::new(path)
            .components()
            .any(|c| c == std::path::Component::ParentDir)
    {
        return None;
    }
    Some(base.join(path))
}

/// A plain command, optionally preceded by literal `cd DIR &&` commands.
/// Other shell control flow does not provide a reliable config directory.
fn config_command(line: &str, cwd: &Path) -> Option<(PathBuf, Vec<String>)> {
    let without_and = line.replace("&&", "");
    if without_and.contains([';', '&', '|', '\n', '(', ')', '$', '`', '<', '>', '\\', '#']) {
        return None;
    }
    let parts: Vec<_> = line.split("&&").collect();
    let mut dir = cwd.to_path_buf();
    for part in &parts[..parts.len() - 1] {
        let cmds = simple_commands(part);
        let [words] = cmds.as_slice() else {
            return None;
        };
        let path = match words.as_slice() {
            [cmd, path] if cmd == "cd" && !path.starts_with('-') => path,
            [cmd, sep, path] if cmd == "cd" && sep == "--" => path,
            _ => return None,
        };
        if std::env::var_os("CDPATH").is_some_and(|v| !v.is_empty())
            && !Path::new(path).is_absolute()
        {
            return None;
        }
        dir = literal_path(&dir, path)?.canonicalize().ok()?;
        if !dir.is_dir() {
            return None;
        }
    }
    let cmds = simple_commands(parts.last()?);
    let [words] = cmds.as_slice() else {
        return None;
    };
    // These wrappers select another package or directory. Until that target
    // is resolved, inspecting this directory's files cannot protect its settings.
    if tweaks::package_target_override(words) || words.iter().any(|word| word.starts_with("-C")) {
        return None;
    }
    Some((dir, words.clone()))
}

/// Config locations selected by the command and its forwarded arguments.
fn command_configs(
    tool: Tool,
    line: &str,
    cwd: &Path,
    script: Option<&(PathBuf, String)>,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Option<String> {
    let (dir, words) = if let Some((dir, script)) = script {
        let (dir, mut words) = config_command(script, dir)?;
        let (_, forwarded) = config_command(line, cwd)?;
        words.extend(forwarded);
        (dir, words)
    } else {
        config_command(line, cwd)?
    };
    let mut root = dir.clone();
    let mut explicit_configs = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let word = &words[i];
        if tool == Tool::Cargo {
            if word == "--config"
                || word.starts_with("--config=")
                // Inline assignments can select a different global config,
                // including inside npm scripts. Leave those commands alone.
                || word.starts_with("CARGO_HOME=")
                || word.starts_with("HOME=")
            {
                return None;
            }
            i += 1;
            continue;
        }
        // Attached short options are accepted by CLI parsers, but do not
        // expose an unambiguous path here.
        if ["-c", "-r"]
            .iter()
            .any(|flag| word.starts_with(flag) && word.len() > flag.len())
        {
            return None;
        }
        let option = ["--config", "-c", "--root", "-r"]
            .into_iter()
            .find_map(|flag| {
                if word == flag {
                    Some((flag, None))
                } else {
                    word.strip_prefix(&format!("{flag}="))
                        .map(|value| (flag, Some(value.to_string())))
                }
            });
        if let Some((flag, value)) = option {
            let value = match value {
                Some(value) => value,
                None => {
                    i += 1;
                    words.get(i)?.clone()
                }
            };
            let path = literal_path(&dir, &value)?;
            if matches!(flag, "--root" | "-r") {
                root = path.canonicalize().ok()?;
                if !root.is_dir() {
                    return None;
                }
            } else {
                // An unreadable explicit config is not evidence that the user
                // left workers at their default.
                explicit_configs.push(std::fs::read_to_string(path).ok()?);
            }
        }
        i += 1;
    }
    if tool == Tool::Cargo {
        // Cargo searches past repository boundaries, then its global home.
        // Relative CARGO_HOME paths are relative to the command's directory.
        let home = env("CARGO_HOME")
            .filter(|v| !v.is_empty())
            .map(|v| dir.join(v))
            .or_else(|| {
                env("HOME")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
                    .or_else(std::env::home_dir)
                    .map(|p| p.join(".cargo"))
            })?;
        let mut configs = Vec::new();
        for config_dir in dir
            .ancestors()
            .map(|d| d.join(".cargo"))
            .chain(std::iter::once(home))
        {
            for name in ["config", "config.toml"] {
                match std::fs::read_to_string(config_dir.join(name)) {
                    Ok(text) => {
                        configs.push(text);
                        // Cargo prefers the extensionless file if both exist.
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return None,
                }
            }
        }
        return Some(configs.join("\n"));
    }
    let mut configs = explicit_configs;
    configs.extend(up_to_top(&root).iter().flat_map(|d| {
        tool.config_files()
            .iter()
            .filter_map(move |f| std::fs::read_to_string(d.join(f)).ok())
    }));
    Some(configs.join("\n"))
}

/// Can this command be told to use fewer workers? Reads the npm script it
/// runs and the tool's config files. Cargo also reads above the checkout and
/// from its global home.
pub fn tunable(line: &str, cwd: &Path) -> Option<Tunable> {
    tunable_with_env(line, cwd, &|v| std::env::var_os(v))
}

fn tunable_with_env(
    line: &str,
    cwd: &Path,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Option<Tunable> {
    let script = tweaks::script_name(line).and_then(|n| npm_script(cwd, &n));
    let script_text = script.as_ref().map(|(_, text)| text.as_str());
    let env_set = |v: &str| env(v).is_some_and(|x| !x.is_empty());
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
    let candidate = tweaks::detect(&Probe {
        line,
        script: script_text,
        configs: &|_| String::new(),
        env_set: &env_set,
        cores,
    })?;
    let located_configs = if matches!(candidate.tool, Tool::Vitest | Tool::Cargo) {
        Some(command_configs(
            candidate.tool,
            line,
            cwd,
            script.as_ref(),
            env,
        )?)
    } else {
        None
    };
    let dirs = up_to_top(cwd);
    let configs = |tool: Tool| {
        if let Some(configs) = &located_configs {
            return configs.clone();
        }
        dirs.iter()
            .flat_map(|d| tool.config_files().iter().map(move |f| d.join(f)))
            .filter_map(|p| std::fs::read_to_string(p).ok())
            .collect::<Vec<_>>()
            .join("\n")
    };
    tweaks::detect(&Probe {
        line,
        script: script_text,
        configs: &configs,
        env_set: &env_set,
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
    fn turbo_config_concurrency_is_preserved_for_direct_commands_and_scripts() {
        let root = checkout("turbo-config");
        let web = root.join("apps/web");
        std::fs::write(
            root.join("package.json"),
            r#"{"scripts":{"build":"turbo run build"}}"#,
        )
        .unwrap();
        for filename in ["turbo.json", "turbo.jsonc"] {
            let config = root.join(filename);
            std::fs::write(&config, r#"{"tasks":{"build":{}}}"#).unwrap();
            for line in ["turbo run build", "npm run build"] {
                assert!(tunable(line, &web).is_some(), "{filename}: {line}");
            }
            std::fs::write(&config, "{\n// Explicit limit\n\"concurrency\": \"50%\"\n}").unwrap();
            for line in ["turbo run build", "npm run build"] {
                assert_eq!(tunable(line, &web), None, "{filename}: {line}");
            }
            std::fs::remove_file(config).unwrap();
        }
    }

    #[test]
    fn resolved_scripts_that_clear_the_worker_environment_are_not_tuned() {
        let root = checkout("script-clears-workers");
        for script in [
            "env -u VITEST_MAX_WORKERS vitest run",
            "env -i PATH=/usr/bin:/bin vitest run",
            "env -u CARGO_BUILD_JOBS cargo build",
            "env -i PATH=/usr/bin:/bin cargo build",
            "npm exec turbo run build",
        ] {
            std::fs::write(
                root.join("package.json"),
                serde_json::json!({"scripts": {"test": script}}).to_string(),
            )
            .unwrap();
            assert_eq!(tunable("npm run test", &root), None, "{script}");
        }
        std::fs::write(
            root.join("package.json"),
            r#"{"scripts":{"test":"env FOO=bar vitest run"}}"#,
        )
        .unwrap();
        assert!(tunable("npm run test", &root).is_some());
    }

    #[test]
    fn vitest_commonjs_configs_preserve_worker_settings() {
        let root = checkout("commonjs-config");
        for name in [
            "vitest.config.cjs",
            "vitest.config.cts",
            "vite.config.cjs",
            "vite.config.cts",
        ] {
            let config = root.join(name);
            assert!(tunable("vitest run", &root).is_some(), "{name}");
            std::fs::write(&config, "module.exports = { test: { maxWorkers: 1 } }").unwrap();
            assert_eq!(tunable("vitest run", &root), None, "{name}");
            std::fs::remove_file(config).unwrap();
        }
    }

    #[test]
    fn config_lookup_follows_literal_cd_in_commands_and_scripts() {
        let root = checkout("cd-config");
        assert!(tunable("cd apps/web && vitest run", &root).is_some());
        std::fs::write(
            root.join("apps/web/vitest.config.ts"),
            "export default { test: { maxWorkers: 2 } }",
        )
        .unwrap();
        assert_eq!(tunable("cd apps/web && vitest run", &root), None);
        std::fs::write(
            root.join("package.json"),
            r#"{"scripts":{"test":"cd apps/web && vitest run"}}"#,
        )
        .unwrap();
        assert_eq!(tunable("npm test", &root), None);
        assert_eq!(tunable("pnpm test", &root), None);
    }

    #[test]
    fn explicit_vitest_config_and_root_settings_win() {
        let root = checkout("config-targets");
        std::fs::write(root.join("apps/web/custom.ts"), "maxWorkers: 2").unwrap();
        for line in [
            "vitest run --config apps/web/custom.ts",
            "vitest run --config=apps/web/custom.ts",
            "vitest run -c apps/web/custom.ts",
            "cd apps/web && vitest run --config custom.ts",
        ] {
            assert_eq!(tunable(line, &root), None, "{line}");
        }
        std::fs::write(root.join("apps/web/vitest.config.ts"), "maxWorkers: 2").unwrap();
        for line in ["vitest run --root apps/web", "vitest run --root=apps/web"] {
            assert_eq!(tunable(line, &root), None, "{line}");
        }
        std::fs::write(
            root.join("package.json"),
            r#"{"scripts":{"test":"vitest run"}}"#,
        )
        .unwrap();
        assert_eq!(tunable("npm test -- --root apps/web", &root), None);
        assert_eq!(
            tunable("npm test -- --config apps/web/custom.ts", &root),
            None
        );
    }

    #[test]
    fn config_lookup_uses_the_package_scripts_directory() {
        let root = checkout("script-directory");
        std::fs::write(
            root.join("package.json"),
            r#"{"scripts":{"test":"cd apps/web && vitest run"}}"#,
        )
        .unwrap();
        std::fs::write(root.join("apps/web/vitest.config.ts"), "maxWorkers: 2").unwrap();
        assert_eq!(tunable("npm test", &root.join("apps")), None);
    }

    #[test]
    fn cargo_config_lookup_follows_cd_and_preserves_workspace_support() {
        let root = checkout("cargo-cd-config");
        assert!(tunable("cargo build --workspace", &root).is_some());
        assert!(tunable("cd apps/web && cargo build", &root).is_some());
        std::fs::create_dir_all(root.join("apps/web/.cargo")).unwrap();
        std::fs::write(
            root.join("apps/web/.cargo/config.toml"),
            "[build]\njobs = 2",
        )
        .unwrap();
        assert_eq!(tunable("cd apps/web && cargo build", &root), None);
    }

    #[test]
    fn cargo_checks_global_configs_and_ancestors_above_the_checkout() {
        let root = tmp("cargo-global-config");
        let repo = root.join("repo");
        let home = root.join("home");
        let cargo_home = root.join("cargo-home");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        for config_dir in [root.join(".cargo"), home.join(".cargo"), cargo_home.clone()] {
            std::fs::create_dir_all(&config_dir).unwrap();
            for filename in ["config.toml", "config"] {
                let env = |key: &str| match key {
                    "HOME" => Some(home.clone().into_os_string()),
                    "CARGO_HOME" if config_dir == cargo_home => {
                        Some(cargo_home.clone().into_os_string())
                    }
                    _ => None,
                };
                assert!(tunable_with_env("cargo build", &repo, &env).is_some());
                let path = config_dir.join(filename);
                std::fs::write(&path, "[build]\njobs = 1\n").unwrap();
                assert_eq!(
                    tunable_with_env("cargo build", &repo, &env),
                    None,
                    "{path:?}"
                );
                std::fs::remove_file(path).unwrap();
            }
        }
    }

    #[test]
    fn cargo_home_can_be_relative_to_the_command_directory() {
        let root = checkout("cargo-relative-home");
        let cargo_home = root.join("apps/web/cargo-home");
        std::fs::create_dir_all(&cargo_home).unwrap();
        std::fs::write(cargo_home.join("config.toml"), "[build]\njobs = 1\n").unwrap();
        let env = |key: &str| (key == "CARGO_HOME").then(|| OsString::from("cargo-home"));
        assert_eq!(
            tunable_with_env("cd apps/web && cargo build", &root, &env),
            None
        );
    }

    #[test]
    fn cargo_prefers_legacy_config_and_skips_unreadable_configs() {
        let root = checkout("cargo-config-precedence");
        let cargo_home = root.join("cargo-home");
        std::fs::create_dir_all(&cargo_home).unwrap();
        let env = |key: &str| (key == "CARGO_HOME").then(|| cargo_home.clone().into_os_string());
        std::fs::write(cargo_home.join("config.toml"), "[build]\njobs = 1\n").unwrap();
        std::fs::write(cargo_home.join("config"), "[build]\nincremental = true\n").unwrap();
        assert!(tunable_with_env("cargo build", &root, &env).is_some());
        std::fs::remove_file(cargo_home.join("config")).unwrap();
        std::fs::create_dir(cargo_home.join("config")).unwrap();
        assert_eq!(tunable_with_env("cargo build", &root, &env), None);
    }

    #[test]
    fn cargo_inline_home_overrides_are_not_tuned() {
        let root = checkout("cargo-inline-home");
        std::fs::write(
            root.join("package.json"),
            r#"{"scripts":{"build":"cargo build"}}"#,
        )
        .unwrap();
        for line in [
            "CARGO_HOME=elsewhere cargo build",
            "HOME=elsewhere cargo build",
            "CARGO_HOME=elsewhere npm run build",
            "HOME=elsewhere npm run build",
        ] {
            assert_eq!(tunable(line, &root), None, "{line}");
        }
        std::fs::write(
            root.join("package.json"),
            r#"{"scripts":{"build":"CARGO_HOME=elsewhere cargo build"}}"#,
        )
        .unwrap();
        assert_eq!(tunable("npm run build", &root), None);
    }

    #[test]
    fn unresolved_config_targets_are_not_tuned() {
        let root = checkout("unsafe-config-targets");
        for line in [
            "cd missing && vitest run",
            "cd $WEB && vitest run",
            "cd apps/../apps/web && vitest run",
            "cd apps/web || vitest run",
            "vitest run --config missing.ts",
            "vitest run --config $CONFIG",
            "vitest run --config apps/../custom.ts",
            "vitest run -capps/web/custom.ts",
            "vitest run -rapps/web",
            "vitest run --root missing",
            "pnpm -C apps/web vitest run",
            "pnpm --dir apps/web vitest run",
            "pnpm --dir=apps/web vitest run",
            "pnpm -Capps/web vitest run",
            "yarn --cwd apps/web vitest run",
            "pnpm -C apps/web cargo build",
            "cargo build --config build.jobs=2",
        ] {
            assert_eq!(tunable(line, &root), None, "{line}");
        }
    }

    #[test]
    fn package_target_selectors_do_not_use_the_root_config_or_script() {
        let root = checkout("workspace-targets");
        std::fs::write(
            root.join("package.json"),
            r#"{"workspaces":["apps/web"],"scripts":{"build":"turbo run build"}}"#,
        )
        .unwrap();
        std::fs::write(root.join("apps/web/vitest.config.ts"), "maxWorkers: 2").unwrap();
        assert!(tunable("vitest run", &root).is_some());
        assert!(tunable("npm run build", &root).is_some());
        for line in [
            "npm exec --workspaces -- vitest run",
            "npm exec --workspaces=true -- vitest run",
            "npm exec -wweb -- vitest run",
            "pnpm -Fweb vitest run",
            "pnpm -r vitest run",
            "npm run build --workspaces",
            "npm -wweb run build",
            "pnpm -Fweb build",
            "pnpm -r build",
            "yarn --cwd apps/web build",
            "yarn --cwd=apps/web build",
        ] {
            assert_eq!(tunable(line, &root), None, "{line}");
        }
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
