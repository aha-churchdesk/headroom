//! Installing Headroom's hook into each agent's configuration.
//!
//! These files belong to the user and to other tools (Orca, graft, plannotator
//! all keep hooks next to ours), so the rules are strict:
//!
//! - Add exactly one entry of our own; never edit or reorder anyone else's.
//! - Refuse to touch a file that isn't valid JSON, rather than guess.
//! - Write atomically, keep a backup of what was there, keep the file mode.
//! - Re-running is a no-op; a moved binary is updated in place.
//!
//! Claude and Codex share the PreToolUse JSON shape. pi takes an extension
//! file instead, which is ours alone, so nothing is merged.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::classify::basename;
use crate::hookio::Agent;
use crate::shell::{quote, simple_commands};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    User,
    Project,
    Local,
}

impl Scope {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "user" => Some(Scope::User),
            "project" => Some(Scope::Project),
            "local" => Some(Scope::Local),
            _ => None,
        }
    }
}

/// Where an agent's hook lives for a scope; `None` if the agent has no such scope.
pub fn settings_path(agent: Agent, scope: Scope, home: &Path, project: &Path) -> Option<PathBuf> {
    match (agent, scope) {
        (Agent::Claude, Scope::User) => Some(home.join(".claude/settings.json")),
        (Agent::Claude, Scope::Project) => Some(project.join(".claude/settings.json")),
        (Agent::Claude, Scope::Local) => Some(project.join(".claude/settings.local.json")),
        (Agent::Codex, Scope::User) => Some(home.join(".codex/hooks.json")),
        (Agent::Pi, Scope::User) => Some(home.join(".pi/agent/extensions/headroom.ts")),
        (Agent::Pi, Scope::Project) => Some(project.join(".pi/extensions/headroom.ts")),
        _ => None,
    }
}

/// Is the agent installed on this machine at all?
pub fn agent_present(agent: Agent, home: &Path) -> bool {
    match agent {
        Agent::Claude => home.join(".claude").is_dir(),
        Agent::Codex => home.join(".codex").is_dir(),
        Agent::Pi => home.join(".pi/agent").is_dir(),
    }
}

pub fn hook_command(exe: &str, agent: Agent) -> String {
    format!("{} hook --agent {}", quote(exe), agent.name())
}

/// The executable of our hook command for this agent, if `cmd` is one.
fn ours(cmd: &str, agent: Agent) -> Option<String> {
    // Exactly one simple command: `headroom hook … && something` is not ours.
    let mut cmds = simple_commands(cmd);
    if cmds.len() != 1 {
        return None;
    }
    let words = cmds.remove(0);
    let ok = words.len() == 4
        && basename(&words[0]) == "headroom"
        && words[1] == "hook"
        && words[2] == "--agent"
        && words[3] == agent.name();
    ok.then(|| words[0].clone())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    NotInstalled,
    Installed,
    /// Configured in Codex, but no persisted approval exists for this hook.
    NeedsReview,
    /// Disabled in the user Codex configuration.
    Disabled,
    /// A Codex approval exists, but its fingerprint has not been verified.
    ReviewStatusUnknown,
    /// Installed, but pointing at a binary other than this one.
    Stale {
        exe: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Added,
    Updated,
    Removed,
    Unchanged,
}

fn object<'a>(v: &'a mut Value, what: &str) -> Result<&'a mut Map<String, Value>, String> {
    v.as_object_mut()
        .ok_or_else(|| format!("{what} is not a JSON object; not touching it"))
}

/// Every hook command in a PreToolUse list that is ours, as (entry, hook) indices.
fn our_hooks(pre: &[Value], agent: Agent) -> Vec<(usize, usize, String)> {
    let mut found = Vec::new();
    for (i, entry) in pre.iter().enumerate() {
        let Some(hooks) = entry.get("hooks").and_then(Value::as_array) else {
            continue;
        };
        for (j, h) in hooks.iter().enumerate() {
            if let Some(exe) = h
                .get("command")
                .and_then(Value::as_str)
                .and_then(|c| ours(c, agent))
            {
                found.push((i, j, exe));
            }
        }
    }
    found
}

fn pre_tool_use(v: &Value) -> Option<&Vec<Value>> {
    v.get("hooks")?.get("PreToolUse")?.as_array()
}

pub fn json_status(root: &Value, agent: Agent, exe: &str) -> Status {
    let Some(pre) = pre_tool_use(root) else {
        return Status::NotInstalled;
    };
    match our_hooks(pre, agent).into_iter().next() {
        None => Status::NotInstalled,
        Some((_, _, e)) if e == exe => Status::Installed,
        Some((_, _, e)) => Status::Stale { exe: e },
    }
}

pub fn json_install(root: &mut Value, agent: Agent, exe: &str) -> Result<Change, String> {
    if root.is_null() {
        *root = json!({});
    }
    let top = object(root, "the settings file")?;
    let hooks = top.entry("hooks").or_insert_with(|| json!({}));
    let hooks = object(hooks, "\"hooks\"")?;
    let pre = hooks.entry("PreToolUse").or_insert_with(|| json!([]));
    let pre = pre
        .as_array_mut()
        .ok_or("\"hooks.PreToolUse\" is not a list; not touching it")?;

    let command = hook_command(exe, agent);
    let found = our_hooks(pre, agent);
    if let Some((i, j, current)) = found.first() {
        if current == exe {
            return Ok(Change::Unchanged);
        }
        pre[*i]["hooks"][*j]["command"] = Value::String(command);
        return Ok(Change::Updated);
    }
    let mut entry = json!({ "hooks": [{ "type": "command", "command": command, "timeout": 10 }] });
    // Both agents report shell calls as Bash, including Codex unified exec.
    if matches!(agent, Agent::Claude | Agent::Codex) {
        entry = json!({ "matcher": "Bash", "hooks": entry["hooks"].clone() });
    }
    pre.push(entry);
    Ok(Change::Added)
}

pub fn json_uninstall(root: &mut Value, agent: Agent) -> Result<Change, String> {
    if root.is_null() {
        return Ok(Change::Unchanged);
    }
    let top = object(root, "the settings file")?;
    let Some(hooks) = top.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(Change::Unchanged);
    };
    let Some(pre) = hooks.get_mut("PreToolUse").and_then(Value::as_array_mut) else {
        return Ok(Change::Unchanged);
    };
    let found = our_hooks(pre, agent);
    if found.is_empty() {
        return Ok(Change::Unchanged);
    }
    // Back to front, so earlier indices stay valid.
    for (i, j, _) in found.iter().rev() {
        if let Some(list) = pre[*i].get_mut("hooks").and_then(Value::as_array_mut) {
            list.remove(*j);
        }
    }
    pre.retain(|e| {
        e.get("hooks")
            .and_then(Value::as_array)
            .is_none_or(|h| !h.is_empty())
    });
    if pre.is_empty() {
        hooks.remove("PreToolUse");
    }
    if hooks.is_empty() {
        top.remove("hooks");
    }
    Ok(Change::Removed)
}

/// The pi extension. It shells out to `headroom hook --agent pi` and fails
/// open: any error leaves the command as it was.
pub fn pi_extension(exe: &str) -> String {
    let exe = serde_json::to_string(exe).unwrap_or_default();
    format!(
        r#"// Installed by `headroom hook install`. Sends heavy bash commands through
// Headroom's queue. Remove with `headroom hook uninstall --agent pi`.
import type {{ ExtensionAPI }} from "@earendil-works/pi-coding-agent";
import {{ isToolCallEventType }} from "@earendil-works/pi-coding-agent";
import {{ execFileSync }} from "node:child_process";

const HEADROOM = {exe};

export default function (pi: ExtensionAPI) {{
  pi.on("tool_call", (event, ctx) => {{
    if (!isToolCallEventType("bash", event)) return;
    try {{
      const out = execFileSync(HEADROOM, ["hook", "--agent", "pi"], {{
        input: JSON.stringify({{ command: event.input.command, timeout: event.input.timeout, cwd: ctx.cwd }}),
        encoding: "utf8",
        timeout: 5000,
      }});
      if (!out.trim()) return;
      const answer: {{ command?: string; timeout?: number }} = JSON.parse(out);
      if (typeof answer.command === "string") event.input.command = answer.command;
      if (typeof answer.timeout === "number") event.input.timeout = answer.timeout;
    }} catch {{
      // Fail open: the command runs as it was.
    }}
  }});
}}
"#
    )
}

/// The executable an installed pi extension points at, if the file is ours.
pub fn pi_status(content: &str, exe: &str) -> Status {
    let found = content
        .lines()
        .find_map(|l| l.strip_prefix("const HEADROOM = "))
        .and_then(|rest| serde_json::from_str::<String>(rest.trim_end_matches(';')).ok());
    match found {
        None => Status::NotInstalled,
        Some(e) if e == exe => Status::Installed,
        Some(e) => Status::Stale { exe: e },
    }
}

// ── IO ─────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    pub path: PathBuf,
    pub change: Change,
    pub before: Option<String>,
    pub after: Option<String>,
}

fn read_opt(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn parse_json(path: &Path, text: Option<&str>) -> Result<Value, String> {
    match text {
        None => Ok(Value::Null),
        Some(t) if t.trim().is_empty() => Ok(Value::Null),
        Some(t) => serde_json::from_str(t).map_err(|e| {
            format!(
                "{} is not valid JSON ({e}); fix it first, nothing was changed",
                path.display()
            )
        }),
    }
}

fn pretty(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(v).unwrap_or_default();
    s.push('\n');
    s
}

/// Write via a temp file and a rename, keeping a backup of the previous
/// content and its permissions. A crash mid-write leaves the old file.
pub fn write_atomic(path: &Path, content: &str, previous: Option<&str>) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("no parent directory"))?;
    fs::create_dir_all(dir)?;
    let mode = fs::metadata(path).ok().map(|m| m.permissions());
    if let Some(prev) = previous {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".headroom-bak");
        fs::write(PathBuf::from(bak), prev)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".headroom-tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, content)?;
    if let Some(m) = mode {
        fs::set_permissions(&tmp, m)?;
    }
    fs::rename(&tmp, path)
}

pub fn status(agent: Agent, path: &Path, exe: &str) -> Result<Status, String> {
    let text = read_opt(path).map_err(|e| format!("{}: {e}", path.display()))?;
    match agent {
        Agent::Pi => Ok(text.map_or(Status::NotInstalled, |t| pi_status(&t, exe))),
        Agent::Claude => Ok(json_status(&parse_json(path, text.as_deref())?, agent, exe)),
        Agent::Codex => codex_status(&parse_json(path, text.as_deref())?, path, exe),
    }
}

/// Check Codex's effective hook metadata with a bounded, read-only app-server
/// request. Call this off the UI thread; `status` remains a file-only check.
pub fn verified_status(agent: Agent, path: &Path, exe: &str) -> Result<Status, String> {
    verified_status_with(agent, path, exe, crate::codex_hooks::query)
}

/// Verify the exact hook/config bytes captured by a background caller. A
/// different snapshot must not inherit the trust result from this query.
pub fn verified_status_for_snapshot(
    agent: Agent,
    path: &Path,
    exe: &str,
    expected_hooks: &[u8],
    expected_config: Option<&[u8]>,
) -> Result<Status, String> {
    verified_status_with_snapshot(
        agent,
        path,
        exe,
        Some((expected_hooks, expected_config)),
        crate::codex_hooks::query,
    )
}

/// Return only a status confirmed for these exact bytes. Unavailable or
/// unrecognized Codex metadata is inconclusive, even when local configuration
/// suggests the hook needs approval or is disabled.
pub fn confirmed_status_for_snapshot(
    agent: Agent,
    path: &Path,
    exe: &str,
    expected_hooks: &[u8],
    expected_config: Option<&[u8]>,
) -> Result<Option<Status>, String> {
    check_status_with_snapshot(
        agent,
        path,
        exe,
        Some((expected_hooks, expected_config)),
        crate::codex_hooks::query,
    )
    .map(|check| check.confirmed.then_some(check.status))
}

struct StatusCheck {
    status: Status,
    confirmed: bool,
}

fn verified_status_with(
    agent: Agent,
    path: &Path,
    exe: &str,
    query: impl FnOnce(&Path) -> Option<Value>,
) -> Result<Status, String> {
    verified_status_with_snapshot(agent, path, exe, None, query)
}

fn verified_status_with_snapshot(
    agent: Agent,
    path: &Path,
    exe: &str,
    expected: Option<(&[u8], Option<&[u8]>)>,
    query: impl FnOnce(&Path) -> Option<Value>,
) -> Result<Status, String> {
    check_status_with_snapshot(agent, path, exe, expected, query).map(|check| check.status)
}

fn check_status_with_snapshot(
    agent: Agent,
    path: &Path,
    exe: &str,
    expected: Option<(&[u8], Option<&[u8]>)>,
    query: impl FnOnce(&Path) -> Option<Value>,
) -> Result<StatusCheck, String> {
    let before = read_opt(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let base = status(agent, path, exe)?;
    let eligible = agent == Agent::Codex
        && matches!(
            base,
            Status::NeedsReview | Status::ReviewStatusUnknown | Status::Disabled
        );
    if expected.is_none() && !eligible {
        return Ok(StatusCheck {
            status: base,
            confirmed: true,
        });
    }
    let config_path = path.with_file_name("config.toml");
    let config_before =
        read_opt(&config_path).map_err(|e| format!("{}: {e}", config_path.display()))?;
    if let Some((expected_hooks, expected_config)) = expected
        && (before.as_deref().map(str::as_bytes) != Some(expected_hooks)
            || config_before.as_deref().map(str::as_bytes) != expected_config)
    {
        return Err("Codex hook configuration changed while checking status".to_owned());
    }
    if !eligible {
        return Ok(StatusCheck {
            status: base,
            confirmed: true,
        });
    }
    let root = parse_json(path, before.as_deref())?;
    let result = query(path);
    // A hook edited while Codex was answering needs a new check. Do not attach
    // a previous definition's approval to the replacement file.
    if read_opt(path).ok() != Some(before) || read_opt(&config_path).ok() != Some(config_before) {
        return status(agent, path, exe).map(|status| StatusCheck {
            status,
            confirmed: false,
        });
    }
    let confirmed = result
        .as_ref()
        .and_then(|metadata| verified_codex_metadata(&root, path, metadata));
    Ok(StatusCheck {
        confirmed: confirmed.is_some(),
        status: confirmed.unwrap_or(base),
    })
}

fn verified_codex_metadata(root: &Value, path: &Path, metadata: &Value) -> Option<Status> {
    if !metadata["hooksEnabled"].as_bool()? {
        return Some(Status::Disabled);
    }
    let source = crate::codex_hooks::absolute_normalized(path)?;
    let pre = pre_tool_use(root)?;
    let found = our_hooks(pre, Agent::Codex);
    let hooks = metadata["hooks"].as_array()?;
    let mut review = false;
    let mut disabled = false;
    for (group, handler, _) in found {
        let key = format!("{}:pre_tool_use:{group}:{handler}", source.display());
        let command = pre[group]["hooks"][handler]["command"].as_str()?;
        let mut matching = hooks.iter().filter(|hook| {
            hook["source"] == "user"
                && hook["sourcePath"].as_str() == source.to_str()
                && hook["key"] == key
                && hook["eventName"] == "preToolUse"
                && hook["handlerType"] == "command"
                && hook["command"] == command
                && hook["command"]
                    .as_str()
                    .and_then(|c| ours(c, Agent::Codex))
                    .is_some()
        });
        let hook = matching.next()?;
        if matching.next().is_some() || hook["currentHash"].as_str()?.is_empty() {
            return None;
        }
        match (hook["enabled"].as_bool()?, hook["trustStatus"].as_str()?) {
            (false, "trusted" | "modified" | "untrusted") => disabled = true,
            (true, "trusted") => return Some(Status::Installed),
            (true, "modified" | "untrusted") => review = true,
            _ => return None,
        }
    }
    if review {
        Some(Status::NeedsReview)
    } else if disabled {
        Some(Status::Disabled)
    } else {
        None
    }
}

/// Read only the user configuration next to hooks.json. Codex's /hooks view
/// remains authoritative: profiles, project/managed overrides and invocation
/// flags may change the effective state. The fingerprint format is not a
/// documented interface, so a stored hash alone never means Installed here.
fn codex_status(root: &Value, path: &Path, exe: &str) -> Result<Status, String> {
    let Some(pre) = pre_tool_use(root) else {
        return Ok(Status::NotInstalled);
    };
    let found = our_hooks(pre, Agent::Codex);
    if found.is_empty() {
        return Ok(Status::NotInstalled);
    }
    if let Some((_, _, installed_exe)) = found.first()
        && installed_exe != exe
        && !Path::new(installed_exe).exists()
    {
        return Ok(Status::Stale {
            exe: installed_exe.clone(),
        });
    }
    let config_path = path.with_file_name("config.toml");
    let config_text =
        read_opt(&config_path).map_err(|e| format!("{}: {e}", config_path.display()))?;
    let config: toml_edit::DocumentMut = config_text
        .as_deref()
        .unwrap_or("")
        .parse()
        // TOML diagnostics may quote secret config values. Do not echo them.
        .map_err(|_| {
            format!(
                "{} is not valid TOML; check hook status in Codex /hooks",
                config_path.display()
            )
        })?;
    let features = config.get("features");
    let hooks_enabled = features
        .and_then(|f| f.get("hooks").or_else(|| f.get("codex_hooks")))
        .and_then(toml_edit::Item::as_bool);
    if hooks_enabled == Some(false)
        || config
            .get("allow_managed_hooks_only")
            .and_then(toml_edit::Item::as_bool)
            == Some(true)
    {
        return Ok(Status::Disabled);
    }
    let states = config.get("hooks").and_then(|h| h.get("state"));
    // Codex uses the absolute source path and zero-based matcher/handler
    // positions in persisted state keys. Preserve symlink spelling; its schema
    // promises an absolute normalized path, not filesystem canonicalization.
    let source = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(path)
    };
    let mut enabled = false;
    for (group, handler, _) in found {
        let key = format!("{}:pre_tool_use:{group}:{handler}", source.display());
        let state = states.and_then(|s| s.get(&key));
        if state
            .and_then(|s| s.get("enabled"))
            .and_then(toml_edit::Item::as_bool)
            == Some(false)
        {
            continue;
        }
        enabled = true;
        if state
            .and_then(|s| s.get("trusted_hash"))
            .and_then(toml_edit::Item::as_str)
            .is_some_and(|hash| !hash.is_empty())
        {
            return Ok(Status::ReviewStatusUnknown);
        }
    }
    Ok(if enabled {
        Status::NeedsReview
    } else {
        Status::Disabled
    })
}

pub fn install(agent: Agent, path: &Path, exe: &str, dry_run: bool) -> Result<Report, String> {
    let before = read_opt(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let (change, after) = match agent {
        Agent::Pi => {
            let want = pi_extension(exe);
            match before.as_deref() {
                Some(b) if b == want => (Change::Unchanged, want),
                Some(b) if pi_status(b, exe) == Status::NotInstalled => {
                    return Err(format!(
                        "{} exists and isn't Headroom's; not overwriting it",
                        path.display()
                    ));
                }
                Some(_) => (Change::Updated, want),
                None => (Change::Added, want),
            }
        }
        _ => {
            let mut v = parse_json(path, before.as_deref())?;
            let change =
                json_install(&mut v, agent, exe).map_err(|e| format!("{}: {e}", path.display()))?;
            (change, pretty(&v))
        }
    };
    if change != Change::Unchanged && !dry_run {
        write_atomic(path, &after, before.as_deref())
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(Report {
        path: path.to_path_buf(),
        change,
        before,
        after: Some(after),
    })
}

pub fn uninstall(agent: Agent, path: &Path, dry_run: bool) -> Result<Report, String> {
    let before = read_opt(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let unchanged = |before| {
        Ok(Report {
            path: path.to_path_buf(),
            change: Change::Unchanged,
            before,
            after: None,
        })
    };
    match agent {
        Agent::Pi => {
            let Some(b) = before.as_deref() else {
                return unchanged(None);
            };
            if pi_status(b, "").eq(&Status::NotInstalled) {
                return Err(format!("{} isn't Headroom's; leaving it", path.display()));
            }
            if !dry_run {
                fs::remove_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
            }
            Ok(Report {
                path: path.to_path_buf(),
                change: Change::Removed,
                before,
                after: None,
            })
        }
        _ => {
            if before.is_none() {
                return unchanged(None);
            }
            let mut v = parse_json(path, before.as_deref())?;
            let change =
                json_uninstall(&mut v, agent).map_err(|e| format!("{}: {e}", path.display()))?;
            if change == Change::Unchanged {
                return unchanged(before);
            }
            let after = pretty(&v);
            if !dry_run {
                write_atomic(path, &after, before.as_deref())
                    .map_err(|e| format!("{}: {e}", path.display()))?;
            }
            Ok(Report {
                path: path.to_path_buf(),
                change,
                before,
                after: Some(after),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = "/opt/homebrew/bin/headroom";

    fn metadata(path: &Path, trust: &str, enabled: bool) -> Value {
        json!({"hooksEnabled": true, "hooks": [{
            "source": "user", "sourcePath": path, "key": format!("{}:pre_tool_use:0:0", path.display()),
            "eventName": "preToolUse", "handlerType": "command",
            "command": hook_command(EXE, Agent::Codex), "currentHash": "current-definition",
            "trustStatus": trust, "enabled": enabled
        }]})
    }

    #[test]
    fn verified_codex_uses_authoritative_trust_without_changing_configuration() {
        let d = tmpdir("verified-trust");
        let path = d.join("hooks.json");
        install(Agent::Codex, &path, EXE, false).unwrap();
        let original = fs::read(&path).unwrap();
        let config_path = d.join("config.toml");
        let config = format!(
            "[hooks.state.\"{}:pre_tool_use:0:0\"]\ntrusted_hash = \"old\"\n",
            path.display()
        );
        fs::write(&config_path, &config).unwrap();
        for (trust, enabled, expected) in [
            ("trusted", true, Status::Installed),
            ("modified", true, Status::NeedsReview),
            ("untrusted", true, Status::NeedsReview),
            ("trusted", false, Status::Disabled),
            ("managed", true, Status::ReviewStatusUnknown),
        ] {
            let actual = verified_status_with(Agent::Codex, &path, EXE, |_| {
                Some(metadata(&path, trust, enabled))
            })
            .unwrap();
            assert_eq!(actual, expected);
        }
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::read_to_string(&config_path).unwrap(), config);
        assert_eq!(fs::read_dir(&d).unwrap().count(), 2);
    }

    #[test]
    fn verified_codex_never_borrows_another_hooks_trust_or_accepts_bad_metadata() {
        let d = tmpdir("verified-matching");
        let path = d.join("hooks.json");
        install(Agent::Codex, &path, EXE, false).unwrap();
        let root: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        for (field, value) in [
            ("sourcePath", json!("/other/hooks.json")),
            ("key", json!(format!("{}:pre_tool_use:1:0", path.display()))),
            ("source", json!("project")),
            ("eventName", json!("postToolUse")),
            ("handlerType", json!("prompt")),
            ("command", json!("/other/headroom hook --agent codex")),
            (
                "command",
                json!(format!("{} && echo extra", hook_command(EXE, Agent::Codex))),
            ),
            ("currentHash", json!("")),
            ("enabled", Value::Null),
            ("trustStatus", json!("future-value")),
        ] {
            let mut response = metadata(&path, "trusted", true);
            response["hooks"][0][field] = value;
            assert_eq!(
                verified_codex_metadata(&root, &path, &response),
                None,
                "{field}"
            );
        }
        let mut duplicate = metadata(&path, "trusted", true);
        let hook = duplicate["hooks"][0].clone();
        duplicate["hooks"].as_array_mut().unwrap().push(hook);
        assert_eq!(verified_codex_metadata(&root, &path, &duplicate), None);
        let mut disabled = metadata(&path, "trusted", true);
        disabled["hooksEnabled"] = json!(false);
        assert_eq!(
            verified_codex_metadata(&root, &path, &disabled),
            Some(Status::Disabled)
        );
    }

    #[test]
    fn verified_codex_preserves_fallback_and_ignores_racing_approvals() {
        let d = tmpdir("verified-fallback");
        let path = d.join("hooks.json");
        install(Agent::Codex, &path, EXE, false).unwrap();
        assert_eq!(
            verified_status_with(Agent::Codex, &path, EXE, |_| None),
            Ok(Status::NeedsReview)
        );
        let config_path = d.join("config.toml");
        let config = format!(
            "[hooks.state.\"{}:pre_tool_use:0:0\"]\ntrusted_hash = \"old\"\n",
            path.display()
        );
        fs::write(&config_path, &config).unwrap();
        assert_eq!(
            verified_status_with(Agent::Codex, &path, EXE, |_| None),
            Ok(Status::ReviewStatusUnknown)
        );
        assert_eq!(
            verified_status_with(Agent::Codex, &path, EXE, |_| {
                fs::write(&config_path, "[features]\nhooks = false\n").unwrap();
                Some(metadata(&path, "trusted", true))
            }),
            Ok(Status::Disabled)
        );
        fs::write(&config_path, config).unwrap();
        assert_eq!(
            verified_status_with(Agent::Codex, &path, EXE, |_| {
                fs::write(&path, "{}").unwrap();
                Some(metadata(&path, "trusted", true))
            }),
            Ok(Status::NotInstalled)
        );
    }

    #[test]
    fn snapshot_confirmation_requires_authoritative_metadata_even_without_local_approval() {
        let d = tmpdir("confirmed-metadata");
        let path = d.join("hooks.json");
        install(Agent::Codex, &path, EXE, false).unwrap();
        let hooks = fs::read(&path).unwrap();
        let config_path = d.join("config.toml");
        for config in [
            "",
            "profile = \"active\"\n[features]\nhooks = false\n[profiles.active.features]\nhooks = true\n",
        ] {
            fs::write(&config_path, config).unwrap();
            let expected = Some((hooks.as_slice(), Some(config.as_bytes())));
            let mut wrong_hook = metadata(&path, "trusted", true);
            wrong_hook["hooks"][0]["sourcePath"] = json!("/other/hooks.json");
            for response in [
                None,
                Some(json!({})),
                Some(json!({"hooksEnabled": true, "hooks": []})),
                Some(wrong_hook),
                Some(metadata(&path, "future-status", true)),
            ] {
                let check =
                    check_status_with_snapshot(Agent::Codex, &path, EXE, expected, |_| response)
                        .unwrap();
                assert!(
                    !check.confirmed,
                    "file-only status must remain inconclusive"
                );
            }
            for (trust, enabled, status) in [
                ("trusted", true, Status::Installed),
                ("untrusted", true, Status::NeedsReview),
                ("modified", true, Status::NeedsReview),
                ("trusted", false, Status::Disabled),
            ] {
                let check = check_status_with_snapshot(Agent::Codex, &path, EXE, expected, |_| {
                    Some(metadata(&path, trust, enabled))
                })
                .unwrap();
                assert!(check.confirmed);
                assert_eq!(check.status, status);
            }
            assert_eq!(fs::read(&path).unwrap(), hooks);
            assert_eq!(fs::read_to_string(&config_path).unwrap(), config);
        }
    }

    #[test]
    fn files_changed_during_query_cannot_produce_confirmed_status() {
        let d = tmpdir("confirmed-race");
        let path = d.join("hooks.json");
        install(Agent::Codex, &path, EXE, false).unwrap();
        let hooks = fs::read(&path).unwrap();
        let config_path = d.join("config.toml");
        let config = b"# before\n";
        for changed in [&path, &config_path] {
            fs::write(&path, &hooks).unwrap();
            fs::write(&config_path, config).unwrap();
            let check = check_status_with_snapshot(
                Agent::Codex,
                &path,
                EXE,
                Some((&hooks, Some(config))),
                |_| {
                    fs::write(
                        changed,
                        if changed == &path {
                            "{}"
                        } else {
                            "[features]\nhooks = false\n"
                        },
                    )
                    .unwrap();
                    Some(metadata(&path, "trusted", true))
                },
            )
            .unwrap();
            assert!(!check.confirmed);
            assert_ne!(check.status, Status::Installed);
        }
    }

    #[test]
    fn verified_status_skips_subprocesses_for_other_agents_and_absent_or_stale_hooks() {
        let d = tmpdir("verified-skip");
        let path = d.join("hooks.json");
        assert_eq!(
            verified_status_with(Agent::Codex, &path, EXE, |_| panic!("must not probe")),
            Ok(Status::NotInstalled)
        );
        install(Agent::Codex, &path, "/missing/headroom", false).unwrap();
        assert!(matches!(
            verified_status_with(Agent::Codex, &path, EXE, |_| panic!("must not probe")),
            Ok(Status::Stale { .. })
        ));
        assert_eq!(
            verified_status_with(Agent::Claude, &path, EXE, |_| panic!("must not probe")),
            Ok(Status::NotInstalled)
        );
    }

    #[test]
    fn verified_codex_effective_metadata_overrides_file_disabled_but_preserves_fallback() {
        let d = tmpdir("verified-disabled-overrides");
        let path = d.join("hooks.json");
        install(Agent::Codex, &path, EXE, false).unwrap();
        let config_path = d.join("config.toml");
        for config in [
            "profile = \"active\"\n[features]\nhooks = false\n[profiles.active.features]\nhooks = true\n".to_owned(),
            "[features]\ncodex_hooks = false\n".to_owned(),
            "allow_managed_hooks_only = true\n".to_owned(),
            format!("[hooks.state.\"{}:pre_tool_use:0:0\"]\nenabled = false\n", path.display()),
        ] {
            fs::write(&config_path, &config).unwrap();
            assert_eq!(status(Agent::Codex, &path, EXE), Ok(Status::Disabled));
            assert_eq!(verified_status_with(Agent::Codex, &path, EXE, |_| None), Ok(Status::Disabled));
            assert_eq!(verified_status_with(Agent::Codex, &path, EXE, |_| Some(json!({"hooksEnabled": true, "hooks": []}))), Ok(Status::Disabled));
            assert_eq!(verified_status_with(Agent::Codex, &path, EXE, |_| Some(metadata(&path, "trusted", true))), Ok(Status::Installed));
            assert_eq!(verified_status_with(Agent::Codex, &path, EXE, |_| Some(metadata(&path, "trusted", false))), Ok(Status::Disabled));
            assert_eq!(fs::read_to_string(&config_path).unwrap(), config);
        }
    }

    #[test]
    fn verified_codex_snapshot_mismatch_never_queries_or_borrows_approval() {
        let d = tmpdir("verified-snapshot");
        let path = d.join("hooks.json");
        install(Agent::Codex, &path, EXE, false).unwrap();
        let expected_hooks = fs::read(&path).unwrap();
        let config_path = d.join("config.toml");
        let expected_config = b"# config A\n";
        fs::write(&config_path, expected_config).unwrap();
        let expected = Some((expected_hooks.as_slice(), Some(expected_config.as_slice())));
        fs::write(&config_path, "# config B\n").unwrap();
        assert!(
            verified_status_with_snapshot(Agent::Codex, &path, EXE, expected, |_| panic!(
                "mismatch must not probe"
            ))
            .is_err()
        );
        fs::write(&config_path, expected_config).unwrap();
        let mut changed_hooks: Value = serde_json::from_slice(&expected_hooks).unwrap();
        changed_hooks["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"] = json!(20);
        fs::write(&path, changed_hooks.to_string()).unwrap();
        assert!(
            verified_status_with_snapshot(Agent::Codex, &path, EXE, expected, |_| panic!(
                "mismatch must not probe"
            ))
            .is_err()
        );
        fs::write(&path, "{}").unwrap();
        assert!(
            verified_status_with_snapshot(Agent::Codex, &path, EXE, expected, |_| panic!(
                "mismatch must not probe"
            ))
            .is_err()
        );
        fs::write(&path, &expected_hooks).unwrap();
        assert_eq!(
            verified_status_with_snapshot(Agent::Codex, &path, EXE, expected, |_| Some(metadata(
                &path, "trusted", true
            ))),
            Ok(Status::Installed)
        );
        fs::remove_file(&config_path).unwrap();
        assert!(
            verified_status_with_snapshot(Agent::Codex, &path, EXE, expected, |_| panic!(
                "mismatch must not probe"
            ))
            .is_err()
        );
        assert_eq!(
            verified_status_with_snapshot(
                Agent::Codex,
                &path,
                EXE,
                Some((&expected_hooks, None)),
                |_| Some(metadata(&path, "trusted", true))
            ),
            Ok(Status::Installed)
        );
    }

    /// Shaped like a real ~/.codex/hooks.json: other tools' entries with no
    /// matcher, and keys in a deliberate order.
    fn codex_file() -> Value {
        json!({
            "hooks": {
                "SessionStart": [{ "hooks": [{ "type": "command", "command": "node graft-hooks.cjs session-start" }] }],
                "PreToolUse": [{ "hooks": [{ "type": "command", "command": "/bin/sh orca-hook.sh", "timeout": 10 }] }],
                "Stop": [{ "hooks": [{ "type": "command", "command": "plannotator" }] }]
            }
        })
    }

    fn tmpdir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "headroom-hooksettings-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn codex_status_requires_the_exact_hook_approval_and_never_changes_config() {
        let d = tmpdir("codex-review");
        let path = d.join("hooks.json");
        let config_path = d.join("config.toml");
        fs::write(&path, pretty(&codex_file())).unwrap();
        install(Agent::Codex, &path, EXE, false).unwrap();
        assert_eq!(status(Agent::Codex, &path, EXE), Ok(Status::NeedsReview));
        // Approval for the unrelated first matcher must not approve Headroom.
        let config = format!(
            "[features]\nhooks = true\n[hooks.state.\"{}:pre_tool_use:0:0\"]\ntrusted_hash = \"sha256:unrelated\"\n",
            path.display()
        );
        fs::write(&config_path, &config).unwrap();
        assert_eq!(status(Agent::Codex, &path, EXE), Ok(Status::NeedsReview));
        assert_eq!(fs::read_to_string(&config_path).unwrap(), config);
        let ours = format!(
            "[hooks.state.\"{}:pre_tool_use:1:0\"]\ntrusted_hash = \"sha256:previous-definition\"\n",
            path.display()
        );
        fs::write(&config_path, &ours).unwrap();
        // Existence alone cannot prove the hook's current fingerprint matches.
        assert_eq!(
            status(Agent::Codex, &path, EXE),
            Ok(Status::ReviewStatusUnknown)
        );
        assert_eq!(fs::read_to_string(&config_path).unwrap(), ours);
        let disabled = format!("{ours}enabled = false\n");
        fs::write(&config_path, &disabled).unwrap();
        assert_eq!(status(Agent::Codex, &path, EXE), Ok(Status::Disabled));
        assert_eq!(fs::read_to_string(&config_path).unwrap(), disabled);
        assert!(!d.join("config.toml.headroom-bak").exists());
    }

    #[test]
    fn codex_status_reports_global_disabling_and_redacts_parse_errors() {
        let d = tmpdir("codex-disabled");
        let path = d.join("hooks.json");
        let config_path = d.join("config.toml");
        install(Agent::Codex, &path, EXE, false).unwrap();
        for config in [
            "[features]\nhooks = false\n",
            "[features]\ncodex_hooks = false\n",
            "allow_managed_hooks_only = true\n",
        ] {
            fs::write(&config_path, config).unwrap();
            assert_eq!(status(Agent::Codex, &path, EXE), Ok(Status::Disabled));
        }
        fs::write(
            &config_path,
            "[features]\nhooks = true\ncodex_hooks = false\n",
        )
        .unwrap();
        assert_eq!(status(Agent::Codex, &path, EXE), Ok(Status::NeedsReview));
        fs::write(&config_path, "token = secret-value-with-invalid-toml").unwrap();
        let error = status(Agent::Codex, &path, EXE).unwrap_err();
        assert!(error.contains("config.toml") && error.contains("/hooks"));
        assert!(!error.contains("secret-value"));
        // A malformed config does not turn an absent hook into an installed one.
        fs::write(&path, "{}").unwrap();
        assert_eq!(status(Agent::Codex, &path, EXE), Ok(Status::NotInstalled));
    }

    #[test]
    fn install_into_nothing_creates_just_our_entry() {
        let mut v = Value::Null;
        assert_eq!(json_install(&mut v, Agent::Claude, EXE), Ok(Change::Added));
        assert_eq!(
            v,
            json!({ "hooks": { "PreToolUse": [{ "matcher": "Bash", "hooks": [
                { "type": "command", "command": "/opt/homebrew/bin/headroom hook --agent claude", "timeout": 10 }
            ]}]}})
        );
    }

    #[test]
    fn install_beside_other_tools_leaves_them_byte_for_byte() {
        let mut v = codex_file();
        assert_eq!(json_install(&mut v, Agent::Codex, EXE), Ok(Change::Added));
        let pre = v["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2);
        assert_eq!(pre[0], codex_file()["hooks"]["PreToolUse"][0]);
        assert_eq!(pre[1]["matcher"], "Bash");
        // Key order survives, so the diff is only our entry.
        let keys: Vec<&String> = v["hooks"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["SessionStart", "PreToolUse", "Stop"]);
        assert_eq!(v["hooks"]["Stop"], codex_file()["hooks"]["Stop"]);
    }

    #[test]
    fn install_twice_is_a_no_op() {
        let mut v = Value::Null;
        json_install(&mut v, Agent::Claude, EXE).unwrap();
        let once = v.clone();
        assert_eq!(
            json_install(&mut v, Agent::Claude, EXE),
            Ok(Change::Unchanged)
        );
        assert_eq!(v, once);
    }

    #[test]
    fn a_moved_binary_is_reported_stale_and_updated_in_place() {
        let mut v = Value::Null;
        json_install(&mut v, Agent::Claude, "/old/bin/headroom").unwrap();
        assert_eq!(
            json_status(&v, Agent::Claude, EXE),
            Status::Stale {
                exe: "/old/bin/headroom".into()
            }
        );
        assert_eq!(
            json_install(&mut v, Agent::Claude, EXE),
            Ok(Change::Updated)
        );
        assert_eq!(json_status(&v, Agent::Claude, EXE), Status::Installed);
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn agents_do_not_see_each_others_hooks() {
        let mut v = Value::Null;
        json_install(&mut v, Agent::Claude, EXE).unwrap();
        assert_eq!(json_status(&v, Agent::Codex, EXE), Status::NotInstalled);
    }

    #[test]
    fn uninstall_restores_the_original() {
        let mut v = codex_file();
        json_install(&mut v, Agent::Codex, EXE).unwrap();
        assert_eq!(json_uninstall(&mut v, Agent::Codex), Ok(Change::Removed));
        assert_eq!(v, codex_file());
        let mut empty = Value::Null;
        json_install(&mut empty, Agent::Claude, EXE).unwrap();
        json_uninstall(&mut empty, Agent::Claude).unwrap();
        assert_eq!(empty, json!({}));
    }

    #[test]
    fn uninstall_when_absent_changes_nothing() {
        let mut v = codex_file();
        assert_eq!(json_uninstall(&mut v, Agent::Codex), Ok(Change::Unchanged));
        assert_eq!(v, codex_file());
        assert_eq!(
            json_uninstall(&mut Value::Null, Agent::Claude),
            Ok(Change::Unchanged)
        );
    }

    #[test]
    fn uninstall_keeps_a_neighbour_sharing_our_entry() {
        let mut v = json!({ "hooks": { "PreToolUse": [{ "matcher": "Bash", "hooks": [
            { "type": "command", "command": "safety-net" },
            { "type": "command", "command": "/x/headroom hook --agent claude" }
        ]}]}});
        json_uninstall(&mut v, Agent::Claude).unwrap();
        assert_eq!(
            v,
            json!({ "hooks": { "PreToolUse": [{ "matcher": "Bash", "hooks": [
                { "type": "command", "command": "safety-net" }
            ]}]}})
        );
    }

    #[test]
    fn unexpected_shapes_are_refused_not_overwritten() {
        for mut v in [
            json!([1, 2]),
            json!({ "hooks": "nope" }),
            json!({ "hooks": { "PreToolUse": {} } }),
        ] {
            let before = v.clone();
            assert!(json_install(&mut v, Agent::Claude, EXE).is_err());
            assert_eq!(v, before);
        }
    }

    #[test]
    fn lookalike_commands_are_not_ours() {
        assert_eq!(
            ours("/x/headroom hook --agent claude", Agent::Claude),
            Some("/x/headroom".into())
        );
        assert_eq!(
            ours("'/My Apps/headroom' hook --agent pi", Agent::Pi),
            Some("/My Apps/headroom".into())
        );
        assert_eq!(
            ours("headroom hook --agent claude && rm -rf x", Agent::Claude),
            None
        );
        assert_eq!(
            ours("not-headroom hook --agent claude", Agent::Claude),
            None
        );
        assert_eq!(
            ours("headroom hook --agent claude --extra", Agent::Claude),
            None
        );
    }

    #[test]
    fn pi_extension_round_trips_its_executable() {
        let ext = pi_extension("/Users/me/My \"Tools\"/headroom");
        assert_eq!(
            pi_status(&ext, "/Users/me/My \"Tools\"/headroom"),
            Status::Installed
        );
        assert_eq!(
            pi_status(&ext, EXE),
            Status::Stale {
                exe: "/Users/me/My \"Tools\"/headroom".into()
            }
        );
        assert_eq!(
            pi_status("export default () => {}", EXE),
            Status::NotInstalled
        );
    }

    #[test]
    fn file_install_backs_up_keeps_mode_and_is_idempotent() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmpdir("file");
        let path = d.join("settings.json");
        let original = "{\n  \"model\": \"opus\"\n}\n";
        fs::write(&path, original).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let r = install(Agent::Claude, &path, EXE, false).unwrap();
        assert_eq!(r.change, Change::Added);
        assert_eq!(
            fs::read_to_string(d.join("settings.json.headroom-bak")).unwrap(),
            original
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let written: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["model"], "opus");
        assert_eq!(status(Agent::Claude, &path, EXE), Ok(Status::Installed));

        let again = install(Agent::Claude, &path, EXE, false).unwrap();
        assert_eq!(again.change, Change::Unchanged);
        assert!(!d.join("settings.json.headroom-tmp").exists());
    }

    #[test]
    fn invalid_json_is_refused_and_left_untouched() {
        let d = tmpdir("invalid");
        let path = d.join("hooks.json");
        fs::write(&path, "{ \"hooks\": [ }").unwrap();
        let e = install(Agent::Codex, &path, EXE, false).unwrap_err();
        assert!(
            e.contains("is not valid JSON") && e.contains("nothing was changed"),
            "{e}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ \"hooks\": [ }");
        assert!(!d.join("hooks.json.headroom-bak").exists());
    }

    #[test]
    fn dry_run_reports_without_writing() {
        let d = tmpdir("dry");
        let path = d.join(".claude/settings.json");
        let r = install(Agent::Claude, &path, EXE, true).unwrap();
        assert_eq!(r.change, Change::Added);
        assert!(r.after.unwrap().contains("hook --agent claude"));
        assert!(!path.exists());
    }

    #[test]
    fn missing_file_and_directory_are_created() {
        let d = tmpdir("missing");
        let path = d.join(".claude/settings.json");
        install(Agent::Claude, &path, EXE, false).unwrap();
        assert_eq!(status(Agent::Claude, &path, EXE), Ok(Status::Installed));
        assert!(!d.join(".claude/settings.json.headroom-bak").exists());
    }

    #[test]
    fn read_only_file_fails_with_its_path_and_keeps_content() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmpdir("ro");
        let path = d.join("settings.json");
        fs::write(&path, "{}").unwrap();
        // A read-only directory stops the temp file and the rename.
        fs::set_permissions(&d, fs::Permissions::from_mode(0o500)).unwrap();
        let r = install(Agent::Claude, &path, EXE, false);
        fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
        let e = r.unwrap_err();
        assert!(e.contains("settings.json"), "{e}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "{}");
    }

    #[test]
    fn pi_install_uninstall_and_foreign_file() {
        let d = tmpdir("pi");
        let path = d.join("headroom.ts");
        assert_eq!(
            install(Agent::Pi, &path, EXE, false).unwrap().change,
            Change::Added
        );
        assert_eq!(
            install(Agent::Pi, &path, EXE, false).unwrap().change,
            Change::Unchanged
        );
        assert_eq!(
            install(Agent::Pi, &path, "/new/headroom", false)
                .unwrap()
                .change,
            Change::Updated
        );
        assert_eq!(
            uninstall(Agent::Pi, &path, false).unwrap().change,
            Change::Removed
        );
        assert!(!path.exists());
        assert_eq!(
            uninstall(Agent::Pi, &path, false).unwrap().change,
            Change::Unchanged
        );

        fs::write(&path, "// someone else's extension\n").unwrap();
        assert!(install(Agent::Pi, &path, EXE, false).is_err());
        assert!(uninstall(Agent::Pi, &path, false).is_err());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "// someone else's extension\n"
        );
    }

    #[test]
    fn file_uninstall_round_trip() {
        let d = tmpdir("uninst");
        let path = d.join("hooks.json");
        let original = pretty(&codex_file());
        fs::write(&path, &original).unwrap();
        install(Agent::Codex, &path, EXE, false).unwrap();
        assert_eq!(
            uninstall(Agent::Codex, &path, false).unwrap().change,
            Change::Removed
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert_eq!(
            uninstall(Agent::Codex, &path, false).unwrap().change,
            Change::Unchanged
        );
    }

    #[test]
    fn scopes_map_to_the_documented_files() {
        let (h, p) = (Path::new("/h"), Path::new("/p"));
        assert_eq!(
            settings_path(Agent::Claude, Scope::Local, h, p),
            Some(PathBuf::from("/p/.claude/settings.local.json"))
        );
        assert_eq!(
            settings_path(Agent::Codex, Scope::User, h, p),
            Some(PathBuf::from("/h/.codex/hooks.json"))
        );
        assert_eq!(settings_path(Agent::Codex, Scope::Project, h, p), None);
        assert_eq!(
            settings_path(Agent::Pi, Scope::User, h, p),
            Some(PathBuf::from("/h/.pi/agent/extensions/headroom.ts"))
        );
    }
}
