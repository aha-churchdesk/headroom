//! Bounded read-only Codex app-server metadata queries. No thread is created,
//! and no hook approval, configuration or model request is sent.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(3);
const OUTPUT_LIMIT: usize = 1024 * 1024;

pub(crate) fn absolute_normalized(path: &Path) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Some(normalized)
}

pub(crate) fn query(path: &Path) -> Option<Value> {
    let home = directories::BaseDirs::new()?.home_dir().to_path_buf();
    let codex = find_codex(&home)?;
    query_with(&codex, path, &home, TIMEOUT, OUTPUT_LIMIT)
}

fn find_codex(home: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let candidates = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .map(|p| p.join("codex"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    candidates
        .into_iter()
        .chain([
            home.join(".local/bin/codex"),
            PathBuf::from("/opt/homebrew/bin/codex"),
            PathBuf::from("/usr/local/bin/codex"),
        ])
        .find(|candidate| {
            candidate
                .metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .and_then(|candidate| absolute_normalized(&candidate))
}

fn query_with(
    codex: &Path,
    path: &Path,
    home: &Path,
    timeout: Duration,
    cap: usize,
) -> Option<Value> {
    let source = absolute_normalized(path)?;
    let codex_home = source.parent()?;
    let home = absolute_normalized(home)?;
    let mut rpc = Rpc::spawn(codex, codex_home, &home, timeout, cap)?;
    let init = rpc.request(
        1,
        "initialize",
        json!({
            "clientInfo": {"name": "headroom_hook_check", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {"experimentalApi": true, "explicitGatewayOauth": true}
        }),
    )?;
    // Reject another config root, even if its hook command happens to match.
    if init["codexHome"].as_str()? != codex_home.to_str()? {
        return None;
    }
    rpc.send(&json!({"method": "initialized"}))?;
    let listing = rpc.request(2, "hooks/list", json!({"cwds": [&home]}))?;
    let entries = listing["data"].as_array()?;
    if entries.len() != 1 || entries[0]["cwd"].as_str()? != home.to_str()? {
        return None;
    }
    if !entries[0]["errors"].as_array()?.is_empty() {
        return None;
    }
    let hooks = entries[0]["hooks"].as_array()?.clone();
    let mut cursor = Value::Null;
    // Pagination is bounded independently of the wall clock and output cap.
    for id in 3..11 {
        let features = rpc.request(
            id,
            "experimentalFeature/list",
            json!({"limit": 100, "cursor": cursor}),
        )?;
        for feature in features["data"].as_array()? {
            if feature["name"] == "hooks" {
                return Some(
                    json!({"hooksEnabled": feature["enabled"].as_bool()?, "hooks": hooks}),
                );
            }
        }
        cursor = features.get("nextCursor").cloned().unwrap_or(Value::Null);
        if cursor.is_null() {
            return None;
        }
        cursor.as_str()?;
    }
    None
}

struct Rpc {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    pending: Vec<u8>,
    received: usize,
    cap: usize,
    deadline: Instant,
}

impl Rpc {
    fn spawn(
        codex: &Path,
        codex_home: &Path,
        home: &Path,
        timeout: Duration,
        cap: usize,
    ) -> Option<Self> {
        let deadline = Instant::now() + timeout;
        let child = Command::new(codex)
            .args(["app-server", "--stdio"])
            .current_dir(home)
            .env("HOME", home)
            .env("CODEX_HOME", codex_home)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        // Install cleanup before any fallible pipe setup.
        let mut rpc = Self {
            child,
            stdin: None,
            stdout: None,
            pending: Vec::new(),
            received: 0,
            cap,
            deadline,
        };
        rpc.stdin = rpc.child.stdin.take();
        rpc.stdout = rpc.child.stdout.take();
        nonblocking(rpc.stdin.as_ref()?.as_raw_fd())?;
        nonblocking(rpc.stdout.as_ref()?.as_raw_fd())?;
        Some(rpc)
    }

    fn pause(&self) -> Option<()> {
        let remaining = self.deadline.checked_duration_since(Instant::now())?;
        std::thread::sleep(remaining.min(Duration::from_millis(5)));
        Some(())
    }

    fn send(&mut self, value: &Value) -> Option<()> {
        let mut message = serde_json::to_vec(value).ok()?;
        message.push(b'\n');
        let mut remaining = message.as_slice();
        while !remaining.is_empty() {
            if Instant::now() >= self.deadline {
                return None;
            }
            match self.stdin.as_mut()?.write(remaining) {
                Ok(0) => return None,
                Ok(n) => remaining = &remaining[n..],
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => self.pause()?,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
        }
        Some(())
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Option<Value> {
        self.send(&json!({"id": id, "method": method, "params": params}))?;
        loop {
            if Instant::now() >= self.deadline {
                return None;
            }
            if let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=end).collect();
                let response: Value = serde_json::from_slice(&line).ok()?;
                if response.get("id").is_some() {
                    if response["id"].as_u64() != Some(id) || response.get("error").is_some() {
                        return None;
                    }
                    return response.get("result").cloned();
                }
                // Notifications may accompany initialization, but a server
                // request must never be answered by this metadata client.
                response["method"].as_str()?;
                continue;
            }
            let mut buffer = [0; 8192];
            match self.stdout.as_mut()?.read(&mut buffer) {
                Ok(0) => return None,
                Ok(n) => {
                    self.received += n;
                    if self.received > self.cap {
                        return None;
                    }
                    self.pending.extend_from_slice(&buffer[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => self.pause()?,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
        }
    }
}

fn nonblocking(fd: std::os::fd::RawFd) -> Option<()> {
    // These are owned pipe descriptors that remain open for this operation.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        None
    } else {
        Some(())
    }
}

impl Drop for Rpc {
    fn drop(&mut self) {
        self.stdin.take();
        self.stdout.take();
        // npm's Codex launcher can have a native child. Keep the probe in its
        // own process group so timeout cleanup also stops that child.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Fixture {
        dir: PathBuf,
        script: PathBuf,
        hooks: PathBuf,
    }

    impl Fixture {
        fn new(init: Option<Value>, listing: Value, feature: Value, mode: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "headroom-codex-probe-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(dir.join(".codex")).unwrap();
            let hooks = dir.join(".codex/hooks.json");
            fs::write(&hooks, "original hooks\n").unwrap();
            fs::write(dir.join(".codex/config.toml"), "# original config\n").unwrap();
            let script = dir.join("codex");
            let init = init.unwrap_or_else(|| json!({"codexHome": dir.join(".codex")}));
            let list = if listing.is_null() {
                json!({"data": [{"cwd": dir, "errors": [], "hooks": [{"trustStatus": "trusted"}]}]})
            } else {
                listing
            };
            let initialization = match mode {
                "timeout" => "read -r waiting_forever".to_owned(),
                "malformed" => "printf '%s\\n' 'not-json'".to_owned(),
                "oversize" => format!("printf '%s\\n' '{}'", "x".repeat(2048)),
                "error" => "printf '%s\\n' '{\"id\":1,\"error\":{\"message\":\"secret-value\"}}'"
                    .to_owned(),
                "early-exit" => "exit 0".to_owned(),
                _ => format!(
                    "printf '%s\\n' {}",
                    crate::shell::quote(&json!({"id": 1, "result": init}).to_string())
                ),
            };
            let script_text = format!(
                r#"#!/bin/sh
printf '%s\n' "$$" > pid
printf '%s\n' "$PWD" "$CODEX_HOME" "$@" > environment
IFS= read -r request
printf '%s\n' "$request" >> requests
{initialization}
IFS= read -r request
printf '%s\n' "$request" >> requests
IFS= read -r request
printf '%s\n' "$request" >> requests
printf '%s\n' {listing}
IFS= read -r request
printf '%s\n' "$request" >> requests
printf '%s\n' {feature}
IFS= read -r wait_for_cleanup
"#,
                listing = crate::shell::quote(&json!({"id": 2, "result": list}).to_string()),
                feature = crate::shell::quote(&json!({"id": 3, "result": feature}).to_string())
            );
            fs::write(&script, script_text).unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
            Self { dir, script, hooks }
        }

        fn query(&self) -> Option<Value> {
            query_with(&self.script, &self.hooks, &self.dir, TIMEOUT, 1024)
        }

        fn assert_reaped_and_unchanged(&self) {
            let pid: i32 = fs::read_to_string(self.dir.join("pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
            assert_eq!(fs::read_to_string(&self.hooks).unwrap(), "original hooks\n");
            assert_eq!(
                fs::read_to_string(self.dir.join(".codex/config.toml")).unwrap(),
                "# original config\n"
            );
            assert_eq!(fs::read_dir(self.dir.join(".codex")).unwrap().count(), 2);
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn features(enabled: bool) -> Value {
        json!({"data": [{"name": "hooks", "enabled": enabled}], "nextCursor": null})
    }

    #[test]
    fn codex_probe_only_sends_readonly_metadata_requests_and_reaps_server() {
        let fixture = Fixture::new(None, Value::Null, features(true), "ok");
        assert_eq!(
            fixture.query(),
            Some(json!({"hooksEnabled": true, "hooks": [{"trustStatus": "trusted"}]}))
        );
        let requests: Vec<Value> = fs::read_to_string(fixture.dir.join("requests"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let methods: Vec<&str> = requests
            .iter()
            .map(|request| request["method"].as_str().unwrap())
            .collect();
        assert_eq!(
            methods,
            [
                "initialize",
                "initialized",
                "hooks/list",
                "experimentalFeature/list"
            ]
        );
        assert_eq!(requests[2]["params"]["cwds"], json!([fixture.dir]));
        assert_eq!(
            fs::read_to_string(fixture.dir.join("environment")).unwrap(),
            format!(
                "{}\n{}\napp-server\n--stdio\n",
                fixture.dir.canonicalize().unwrap().display(),
                fixture.dir.join(".codex").display()
            )
        );
        fixture.assert_reaped_and_unchanged();
    }

    #[test]
    fn codex_probe_honors_effective_disabled_feature() {
        let fixture = Fixture::new(None, Value::Null, features(false), "ok");
        assert_eq!(fixture.query().unwrap()["hooksEnabled"], false);
        fixture.assert_reaped_and_unchanged();
    }

    #[test]
    fn codex_probe_rejects_wrong_home_malformed_errors_and_excess_output() {
        for mode in ["malformed", "error", "early-exit", "oversize"] {
            let fixture = Fixture::new(None, Value::Null, features(true), mode);
            assert!(fixture.query().is_none(), "{mode}");
            fixture.assert_reaped_and_unchanged();
        }
        for init in [json!({"codexHome": "/wrong/home"}), json!({})] {
            let fixture = Fixture::new(Some(init), Value::Null, features(true), "ok");
            assert!(fixture.query().is_none());
            fixture.assert_reaped_and_unchanged();
        }
    }

    #[test]
    fn codex_probe_timeout_is_bounded_and_reaps_server() {
        let fixture = Fixture::new(None, Value::Null, features(true), "timeout");
        let started = Instant::now();
        assert!(fixture.query().is_none());
        assert!(started.elapsed() < Duration::from_secs(4));
        fixture.assert_reaped_and_unchanged();
    }

    #[test]
    fn codex_probe_rejects_unknown_feature_schema() {
        let fixture = Fixture::new(None, Value::Null, json!({"data": []}), "ok");
        assert!(fixture.query().is_none());
        fixture.assert_reaped_and_unchanged();
    }

    #[test]
    fn source_paths_are_normalized_without_resolving_symlinks() {
        assert_eq!(
            absolute_normalized(Path::new("/Users/test/link/../.codex/./hooks.json")),
            Some(PathBuf::from("/Users/test/.codex/hooks.json"))
        );
    }
}
