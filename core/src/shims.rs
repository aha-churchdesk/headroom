//! Read-only shim installation and fresh login-shell checks.

use std::ffi::CStr;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const NAMES: &[&str] = &[
    "pnpm",
    "npm",
    "npx",
    "yarn",
    "bun",
    "bunx",
    "corepack",
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

pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const OUTPUT_CAP: usize = 128 * 1024;
static PROBE_ID: AtomicU64 = AtomicU64::new(0);

pub fn default_dir(home: &Path) -> PathBuf {
    home.join(".local/share/headroom/shims")
}

pub fn executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Match the identity used by the CLI to skip its own shims when finding
/// the real command. A missing target never counts as Headroom.
pub fn is_headroom(path: &Path) -> bool {
    path.canonicalize().is_ok_and(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == "headroom" || n.starts_with("headroom-"))
    })
}

pub fn valid_shim(path: &Path) -> bool {
    // The CLI's broad recursion guard also recognizes helper binaries such
    // as headroom-hog. Installation status requires the actual CLI name.
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
        && executable(path)
        && path
            .canonicalize()
            .is_ok_and(|p| p.file_name().is_some_and(|n| n == "headroom"))
}

pub fn installed(dir: &Path) -> usize {
    NAMES
        .iter()
        .filter(|name| valid_shim(&dir.join(name)))
        .count()
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || a.canonicalize()
            .ok()
            .zip(b.canonicalize().ok())
            .is_some_and(|(a, b)| a == b)
}

#[derive(Debug, PartialEq, Eq)]
pub enum OnPath {
    Missing,
    Behind(Vec<(&'static str, PathBuf)>),
    First,
}

/// Where `dir` sits on `path`, for executable commands that precede it.
pub fn on_path(dir: &Path, path: &str) -> OnPath {
    let dirs: Vec<_> = path.split(':').filter(|d| !d.is_empty()).collect();
    let Some(at) = dirs.iter().position(|d| same_dir(Path::new(d), dir)) else {
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

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Checking,
    Active,
    Partial,
    Missing,
    NotOnPath,
    Shadowed,
    Unverified,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ShimsStatus {
    pub installed: usize,
    pub total: usize,
    pub path: PathBuf,
    pub status: Status,
    pub shell: Option<PathBuf>,
    pub source: String,
    pub shadowed: Vec<String>,
    pub message: Option<String>,
}

impl ShimsStatus {
    /// Installation is available immediately, before shell verification.
    pub fn checking(dir: &Path) -> Self {
        Self {
            installed: installed(dir),
            total: NAMES.len(),
            path: dir.into(),
            status: Status::Checking,
            shell: None,
            source: "fresh_login_shell".into(),
            shadowed: Vec::new(),
            message: None,
        }
    }

    pub fn unverified(mut self, message: &str) -> Self {
        self.status = Status::Unverified;
        self.message = Some(message.into());
        self
    }
}

struct Account {
    shell: PathBuf,
    name: String,
}

/// Read the account's configured shell, not SHELL inherited by a daemon.
fn account() -> Option<Account> {
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result = std::ptr::null_mut();
        let code = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                &mut entry,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if code == libc::ERANGE && buffer.len() < 1024 * 1024 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if code != 0 || result.is_null() || entry.pw_shell.is_null() || entry.pw_name.is_null() {
            return None;
        }
        // getpwuid_r owns these strings in buffer until it is dropped.
        let shell = unsafe { CStr::from_ptr(entry.pw_shell) }.to_str().ok()?;
        let name = unsafe { CStr::from_ptr(entry.pw_name) }.to_str().ok()?;
        if shell.is_empty() || !Path::new(shell).is_absolute() {
            return None;
        }
        return Some(Account {
            shell: shell.into(),
            name: name.into(),
        });
    }
}

/// Intended for a background worker: shell startup may run user commands.
pub fn check_default_shell(home: &Path, dir: &Path) -> ShimsStatus {
    let Some(account) = account() else {
        return ShimsStatus::checking(dir)
            .unverified("Could not read the account's default shell.");
    };
    check_shell(home, dir, &account.shell, &account.name, PROBE_TIMEOUT)
}

/// Probe only the supplied shell and home, which also keeps tests isolated.
pub fn check_shell(
    home: &Path,
    dir: &Path,
    shell: &Path,
    user: &str,
    timeout: Duration,
) -> ShimsStatus {
    let mut state = ShimsStatus::checking(dir);
    state.shell = Some(shell.into());
    let Some(kind) = shell
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| matches!(*s, "zsh" | "bash" | "sh" | "fish"))
    else {
        return state.unverified("This default shell cannot be verified automatically.");
    };
    let marker = format!(
        "HEADROOM_{}_{}",
        std::process::id(),
        PROBE_ID.fetch_add(1, Ordering::Relaxed)
    );
    let script = probe_script(kind, &marker);
    let output = match run_shell(home, shell, user, &script, timeout) {
        Ok(output) => output,
        Err(message) => return state.unverified(message),
    };
    let Some((cwd, path, resolved)) = parse_probe(&output, &marker) else {
        return state.unverified("The default shell did not return a complete status check.");
    };
    let cwd = Path::new(cwd.trim_end_matches(['\r', '\n']));
    if !cwd.is_absolute() {
        return state.unverified("The default shell did not return its working directory.");
    }
    let path = path
        .split(':')
        .map(|p| cwd.join(p).display().to_string())
        .collect::<Vec<_>>()
        .join(":");
    // Refresh the count after the shell exits: installation may have changed.
    state.installed = installed(dir);
    let path_state = on_path(dir, &path);
    for (&name, resolved) in NAMES.iter().zip(resolved) {
        if matches!(path_state, OnPath::Missing) {
            break;
        }
        if !valid_shim(&dir.join(name)) {
            continue;
        }
        let resolved = resolved.trim_end_matches(['\r', '\n']);
        let found = cwd.join(resolved);
        let correct = resolved.contains('/')
            && found.file_name().is_some_and(|n| n == name)
            && found.parent().is_some_and(|p| same_dir(p, dir));
        if !correct {
            state.shadowed.push(name.into());
        }
    }
    state.status = if state.installed == 0 {
        Status::Missing
    } else if matches!(path_state, OnPath::Missing) {
        state.message = Some("The shims directory is not on PATH in a fresh login shell.".into());
        Status::NotOnPath
    } else if !state.shadowed.is_empty() {
        Status::Shadowed
    } else if state.installed < state.total {
        Status::Partial
    } else {
        Status::Active
    };
    state
}

fn probe_script(shell: &str, marker: &str) -> String {
    let printf = if shell == "sh" {
        "command printf"
    } else {
        "builtin printf"
    };
    let mut script: String = if shell == "fish" {
        "set -g fish_history ''; ".into()
    } else {
        "unset HISTFILE; ".into()
    };
    script.push_str(&format!("{printf} '\\036{marker}_CWD\\037'; "));
    script.push_str(if shell == "sh" {
        "command pwd -P; "
    } else {
        "builtin pwd -P; "
    });
    script.push_str(&format!(
        "{printf} '\\036{marker}_END\\037'; {printf} '\\036{marker}_PATH\\037'; "
    ));
    if shell == "fish" {
        script.push_str("builtin string join : -- $PATH; ");
    } else {
        script.push_str(&format!("{printf} '%s' \"$PATH\"; "));
    }
    script.push_str(&format!("{printf} '\\036{marker}_END\\037'; "));
    for name in NAMES {
        script.push_str(&format!("{printf} '\\036{marker}_{name}\\037'; "));
        if shell == "fish" {
            script.push_str(&format!(
                "if test (type --type {name} 2>/dev/null) = file; command --search {name}; end; "
            ));
        } else {
            script.push_str(&format!("command -v {name} 2>/dev/null; "));
        }
        script.push_str(&format!("{printf} '\\036{marker}_END\\037'; "));
    }
    script
}

fn parse_probe<'a>(output: &'a [u8], marker: &str) -> Option<(&'a str, String, Vec<&'a str>)> {
    let end = format!("\x1e{marker}_END\x1f");
    let mut remaining = output;
    let mut field = |name: &str| {
        let start = format!("\x1e{marker}_{name}\x1f");
        let begin = remaining
            .windows(start.len())
            .position(|w| w == start.as_bytes())?
            + start.len();
        let rest = &remaining[begin..];
        let finish = rest.windows(end.len()).position(|w| w == end.as_bytes())?;
        remaining = &rest[finish + end.len()..];
        std::str::from_utf8(&rest[..finish]).ok()
    };
    let cwd = field("CWD")?;
    let path = field("PATH")?.trim_end_matches(['\r', '\n']).to_owned();
    let resolved = NAMES
        .iter()
        .map(|name| field(name))
        .collect::<Option<Vec<_>>>()?;
    Some((cwd, path, resolved))
}

struct ShellProcess(Child);

impl Drop for ShellProcess {
    fn drop(&mut self) {
        // Startup files can launch children. Bound the entire probe group.
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run_shell(
    home: &Path,
    shell: &Path,
    user: &str,
    script: &str,
    timeout: Duration,
) -> Result<Vec<u8>, &'static str> {
    let deadline = Instant::now() + timeout;
    let child = Command::new(shell)
        .args(["-ilc", script])
        .current_dir(home)
        .env_clear()
        .env("HOME", home)
        .env("USER", user)
        .env("LOGNAME", user)
        .env("SHELL", shell)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("TERM", "dumb")
        .env("LANG", "C")
        .env("HISTFILE", "/dev/null")
        .env("fish_history", "")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "Could not start the default shell status check.")?;
    let mut child = ShellProcess(child);
    let mut stdout = child
        .0
        .stdout
        .take()
        .ok_or("Could not read the default shell status check.")?;
    let fd = stdout.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err("Could not read the default shell status check.");
    }
    let mut output = Vec::new();
    let mut exited = None;
    let mut eof = false;
    loop {
        if Instant::now() >= deadline {
            return Err("The default shell status check timed out.");
        }
        let mut buffer = [0; 8192];
        match stdout.read(&mut buffer) {
            Ok(0) => eof = true,
            Ok(n) => {
                if output.len() + n > OUTPUT_CAP {
                    return Err("The default shell returned too much startup output.");
                }
                output.extend_from_slice(&buffer[..n]);
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err("Could not read the default shell status check."),
        }
        if exited.is_none() {
            exited = child
                .0
                .try_wait()
                .map_err(|_| "Could not finish the default shell status check.")?;
        }
        if let Some(status) = exited {
            if !status.success() {
                return Err("The default shell status check exited unsuccessfully.");
            }
            if eof {
                return Ok(output);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    struct Fixture {
        root: PathBuf,
        home: PathBuf,
        dir: PathBuf,
        shell: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "headroom-shim-status-{}-{}",
                std::process::id(),
                PROBE_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            let root = root.canonicalize().unwrap();
            let home = root.join("home");
            fs::create_dir_all(&home).unwrap();
            let dir = default_dir(&home);
            let shell = root.join("bin/zsh");
            Self {
                root,
                home,
                dir,
                shell,
            }
        }

        fn executable(path: &Path, body: &str) {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn install(&self) {
            let binary = self.root.join("bin/headroom");
            Self::executable(&binary, "exit 0");
            fs::create_dir_all(&self.dir).unwrap();
            for name in NAMES {
                symlink(&binary, self.dir.join(name)).unwrap();
            }
        }

        fn fake_shell(&self, before: &str) {
            Self::executable(&self.shell, &format!("{before}\nexec /bin/bash -c \"$2\""));
        }

        fn path_first(&self) {
            self.fake_shell(
                "PATH=\"$HOME/.local/share/headroom/shims:/usr/bin:/bin\"; export PATH",
            );
        }

        fn check(&self) -> ShimsStatus {
            check_shell(
                &self.home,
                &self.dir,
                &self.shell,
                "fixture-user",
                Duration::from_secs(2),
            )
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn installation_counts_only_executable_headroom_symlinks() {
        let f = Fixture::new();
        assert_eq!(installed(&f.dir), 0);
        f.install();
        assert_eq!(installed(&f.dir), NAMES.len());
        fs::remove_file(f.dir.join("cargo")).unwrap();
        symlink(f.root.join("missing/headroom"), f.dir.join("cargo")).unwrap();
        fs::remove_file(f.dir.join("pnpm")).unwrap();
        Fixture::executable(&f.dir.join("pnpm"), "exit 0");
        fs::remove_file(f.dir.join("npm")).unwrap();
        let foreign = f.root.join("bin/foreign");
        Fixture::executable(&foreign, "exit 0");
        symlink(&foreign, f.dir.join("npm")).unwrap();
        fs::remove_file(f.dir.join("npx")).unwrap();
        let not_executable = f.root.join("not-executable/headroom");
        fs::create_dir_all(not_executable.parent().unwrap()).unwrap();
        fs::write(&not_executable, "data").unwrap();
        symlink(not_executable, f.dir.join("npx")).unwrap();
        assert_eq!(installed(&f.dir), NAMES.len() - 4);
    }

    #[test]
    fn helpers_and_foreign_headroom_prefixes_never_count_as_installed_shims() {
        let f = Fixture::new();
        f.install();
        f.path_first();
        for (name, target) in [
            ("pnpm", "headroom-hog"),
            ("cargo", "headroomd"),
            ("npm", "headroom-foreign"),
        ] {
            let helper = f.root.join("bin").join(target);
            Fixture::executable(&helper, "exit 0");
            fs::remove_file(f.dir.join(name)).unwrap();
            symlink(helper, f.dir.join(name)).unwrap();
        }
        let state = f.check();
        assert_eq!(state.installed, NAMES.len() - 3);
        assert_eq!(state.status, Status::Partial);
    }

    #[test]
    fn real_zsh_uses_isolated_startup_files_and_detects_aliases_and_functions() {
        let f = Fixture::new();
        f.install();
        let startup = f.home.join(".zshrc");
        let config = "export PATH=\"$HOME/.local/share/headroom/shims:/usr/bin:/bin\"\n";
        fs::write(&startup, config).unwrap();
        let check = || {
            check_shell(
                &f.home,
                &f.dir,
                Path::new("/bin/zsh"),
                "fixture-user",
                Duration::from_secs(3),
            )
        };
        assert_eq!(check().status, Status::Active);
        fs::write(
            &startup,
            format!("{config}alias pnpm='printf shadowed'\ncargo() {{ :; }}\n"),
        )
        .unwrap();
        let before = fs::read(&startup).unwrap();
        let state = check();
        assert_eq!(state.status, Status::Shadowed);
        assert_eq!(state.shadowed, vec!["pnpm", "cargo"]);
        assert_eq!(state.installed, 16);
        assert_eq!(fs::read(startup).unwrap(), before);
        assert!(!f.home.join(".zsh_history").exists());
    }

    #[test]
    fn relative_shell_path_entries_are_resolved_from_the_shell_working_directory() {
        let f = Fixture::new();
        f.install();
        f.fake_shell(
            "cd \"$HOME/.local/share/headroom\" || exit 8\nPATH='shims:/usr/bin:/bin'; export PATH",
        );
        assert_eq!(f.check().status, Status::Active);
        f.fake_shell("cd \"$HOME/.local/share/headroom/shims\" || exit 8\nPATH='./:/usr/bin:/bin'; export PATH");
        assert_eq!(f.check().status, Status::Active);
    }

    #[test]
    fn fresh_shell_status_ignores_startup_noise_and_never_exports_it() {
        let f = Fixture::new();
        f.install();
        let config = f.home.join(".zshrc");
        fs::write(&config, "private config bytes").unwrap();
        f.fake_shell("test \"$1\" = '-ilc' || exit 8\ntest \"$USER\" = fixture-user || exit 8\ntest \"$PATH\" = '/usr/bin:/bin:/usr/sbin:/sbin' || exit 8\nprintf 'startup private-value \\377\\n'\nPATH=\"$HOME/.local/share/headroom/shims:/usr/bin:/bin\"; export PATH");
        let state = f.check();
        assert_eq!(state.installed, 16);
        assert_eq!(state.status, Status::Active);
        assert_eq!(state.shell, Some(f.shell.clone()));
        assert_eq!(state.source, "fresh_login_shell");
        assert!(state.shadowed.is_empty());
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("private-value"));
        assert!(!json.contains("/usr/bin:/bin"));
        assert_eq!(fs::read_to_string(config).unwrap(), "private config bytes");
    }

    #[test]
    fn partial_missing_and_not_on_path_are_distinct() {
        let f = Fixture::new();
        f.path_first();
        assert_eq!(f.check().status, Status::Missing);
        f.install();
        fs::remove_file(f.dir.join("docker")).unwrap();
        let state = f.check();
        assert_eq!(state.status, Status::Partial);
        assert_eq!(state.installed, 15);
        assert!(state.shadowed.is_empty());
        f.fake_shell("PATH='/usr/bin:/bin'; export PATH");
        let state = f.check();
        assert_eq!(state.status, Status::NotOnPath);
        assert_eq!(state.installed, 15);
        assert!(state.shadowed.is_empty());
    }

    #[test]
    fn earlier_commands_and_shell_functions_shadow_installed_shims() {
        let f = Fixture::new();
        f.install();
        Fixture::executable(&f.home.join("earlier/cargo"), "exit 0");
        f.fake_shell("PATH=\"$HOME/earlier:$HOME/.local/share/headroom/shims:/usr/bin:/bin\"; export PATH\npnpm() { :; }; export -f pnpm");
        let state = f.check();
        assert_eq!(state.status, Status::Shadowed);
        assert_eq!(state.shadowed, vec!["pnpm", "cargo"]);
        assert_eq!(state.installed, 16);
    }

    #[test]
    fn missing_unsupported_nonzero_and_incomplete_shell_checks_are_unverified() {
        let f = Fixture::new();
        f.install();
        assert_eq!(f.check().status, Status::Unverified);
        let unsupported = check_shell(
            &f.home,
            &f.dir,
            Path::new("/bin/tcsh"),
            "fixture-user",
            Duration::from_secs(1),
        );
        assert_eq!(unsupported.status, Status::Unverified);
        Fixture::executable(&f.shell, "printf 'sensitive startup output'; exit 7");
        let state = f.check();
        assert_eq!(state.status, Status::Unverified);
        assert!(!state.message.unwrap().contains("sensitive"));
        Fixture::executable(&f.shell, "printf 'startup output only'; exit 0");
        assert_eq!(f.check().status, Status::Unverified);
    }

    #[test]
    fn shell_timeout_is_bounded() {
        let f = Fixture::new();
        f.install();
        Fixture::executable(&f.shell, "exec /bin/sleep 30");
        let started = Instant::now();
        let state = check_shell(
            &f.home,
            &f.dir,
            &f.shell,
            "fixture-user",
            Duration::from_millis(100),
        );
        assert_eq!(state.status, Status::Unverified);
        assert!(state.message.unwrap().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn probe_cleanup_reaps_its_process() {
        let child = Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        drop(ShellProcess(child));
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }

    #[test]
    fn excessive_startup_output_is_bounded() {
        let f = Fixture::new();
        Fixture::executable(
            &f.shell,
            "while :; do printf 'startup noise startup noise startup noise\\n'; done",
        );
        let state = f.check();
        assert_eq!(state.status, Status::Unverified);
        assert!(state.message.unwrap().contains("too much"));
    }

    #[test]
    fn missing_directories_are_not_equal_just_because_both_canonicalizations_fail() {
        assert_eq!(
            on_path(Path::new("/missing/shims"), "/missing/other"),
            OnPath::Missing
        );
    }
}
