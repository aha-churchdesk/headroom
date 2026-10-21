//! Agent hooks: read what an agent is about to run, answer with the command
//! wrapped in `headroom run`.
//!
//! Each agent speaks its own dialect:
//!
//! | Agent  | In                                   | Out                                         |
//! |--------|--------------------------------------|---------------------------------------------|
//! | Claude | PreToolUse payload, `tool_name=Bash` | `hookSpecificOutput.updatedInput`           |
//! | Codex  | PreToolUse payload (Claude-shaped)   | `hookSpecificOutput.updatedInput.command`   |
//! | pi     | `{command, cwd, timeout}` from our extension | `{command, timeout}`                |
//!
//! Silence (no output) means "run it as it is". Every failure falls back to
//! silence: a broken hook must never stop an agent from working.
//!
//! The rewrite returns no permission decision. The agent's own permission
//! rules then judge the wrapped command, so Headroom can't widen what an
//! agent is allowed to run.

use serde_json::{Map, Value, json};

use crate::classify::{Class, HookProbe, classify_in};
use crate::config::Config;
use crate::shell::quote;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
    Pi,
}

impl Agent {
    pub const ALL: [Agent; 3] = [Agent::Claude, Agent::Codex, Agent::Pi];

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Agent::Claude),
            "codex" => Some(Agent::Codex),
            "pi" => Some(Agent::Pi),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Pi => "pi",
        }
    }
}

/// Claude's Bash tool: default and maximum timeout, in milliseconds.
pub const CLAUDE_DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const CLAUDE_MAX_TIMEOUT_MS: u64 = 600_000;
/// The least queue wait a foreground command gets, even when its tool
/// timeout can't be raised to pay for it.
pub const MIN_WAIT_S: u64 = 60;

#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub command: String,
    pub session: Option<String>,
    pub background: bool,
    /// The tool's own timeout, in the agent's unit (ms for Claude and Codex,
    /// seconds for pi). `None` when the agent didn't set one.
    pub timeout: Option<u64>,
    /// Where the command runs, when the agent says.
    pub cwd: Option<String>,
    /// The tool input as sent. Claude replaces the whole input with ours,
    /// so every field we don't change must be echoed back.
    input: Map<String, Value>,
}

fn str_field(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

pub fn parse(agent: Agent, payload: &str) -> Option<Call> {
    let v: Value = serde_json::from_str(payload).ok()?;
    match agent {
        Agent::Claude | Agent::Codex => {
            if agent == Agent::Claude && v.get("tool_name").and_then(Value::as_str) != Some("Bash")
            {
                return None;
            }
            let input = v.get("tool_input")?.as_object()?.clone();
            let command = match input.get("command")? {
                Value::String(s) => s.clone(),
                // Some shells tools send argv; it is wrapped as one line.
                Value::Array(a) => crate::shell::join(
                    &a.iter()
                        .map(|x| x.as_str().map(str::to_string))
                        .collect::<Option<Vec<_>>>()?,
                ),
                _ => return None,
            };
            let timeout_key = if agent == Agent::Claude {
                "timeout"
            } else {
                "timeout_ms"
            };
            Some(Call {
                command,
                session: str_field(&v, "session_id"),
                background: input
                    .get("run_in_background")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                timeout: input.get(timeout_key).and_then(Value::as_u64),
                cwd: str_field(&v, "cwd"),
                input,
            })
        }
        Agent::Pi => {
            let command = str_field(&v, "command")?;
            Some(Call {
                command,
                session: str_field(&v, "session"),
                background: false,
                timeout: v.get("timeout").and_then(Value::as_u64),
                cwd: str_field(&v, "cwd"),
                input: Map::new(),
            })
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Wrap {
    pub command: String,
    /// New tool timeout, in the agent's unit, when it was raised.
    pub timeout: Option<u64>,
    /// Seconds the wrapper may wait in the queue. `None` for background.
    pub wait_s: Option<u64>,
}

/// The rewrite for a heavy command, or `None` to leave it alone. `hooked`
/// says which git hooks the command's repository has.
pub fn wrap(agent: Agent, call: &Call, exe: &str, cfg: &Config, hooked: HookProbe) -> Option<Wrap> {
    if !matches!(classify_in(&call.command, cfg, hooked), Class::Heavy { .. }) {
        return None;
    }
    let (timeout, wait_s) = if call.background {
        (None, None)
    } else {
        match agent {
            // Raise the tool timeout by the queue budget, so waiting doesn't
            // eat into the command's own time.
            Agent::Claude => {
                let orig = call.timeout.unwrap_or(CLAUDE_DEFAULT_TIMEOUT_MS);
                let raised = (orig + cfg.wait_cap_s * 1000)
                    .min(CLAUDE_MAX_TIMEOUT_MS)
                    .max(orig);
                let wait = ((raised - orig) / 1000).max(MIN_WAIT_S);
                (Some(raised).filter(|&t| t != orig), Some(wait))
            }
            // pi has no timeout unless one was set; raise only an existing one.
            Agent::Pi => (
                call.timeout.map(|t| t + cfg.wait_cap_s),
                Some(cfg.wait_cap_s),
            ),
            Agent::Codex => (None, Some(cfg.wait_cap_s)),
        }
    };
    let mut cmd = format!("{} run --agent {}", quote(exe), agent.name());
    if let Some(s) = &call.session {
        cmd += &format!(" --session {}", quote(s));
    }
    match wait_s {
        Some(w) => cmd += &format!(" --wait-cap {w}"),
        None => cmd += " --background",
    }
    // Bash tool syntax must survive a user's fish or zsh login shell.
    if matches!(agent, Agent::Claude | Agent::Pi) {
        cmd += " --shell /bin/bash";
    }
    cmd += &format!(" --sh {}", quote(&call.command));
    Some(Wrap {
        command: cmd,
        timeout,
        wait_s,
    })
}

/// The agent's answer, as the hook prints it.
pub fn respond(agent: Agent, call: &Call, w: &Wrap) -> String {
    match agent {
        Agent::Claude | Agent::Codex => {
            let mut input = call.input.clone();
            input.insert("command".into(), Value::String(w.command.clone()));
            if let Some(t) = w.timeout {
                let key = if agent == Agent::Claude {
                    "timeout"
                } else {
                    "timeout_ms"
                };
                input.insert(key.into(), json!(t));
            }
            json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "updatedInput": input } }).to_string()
        }
        Agent::Pi => {
            let mut out = json!({ "command": w.command });
            if let Some(t) = w.timeout {
                out["timeout"] = json!(t);
            }
            out.to_string()
        }
    }
}

/// [`HookProbe`] for a given working directory, `None` when unknown.
pub type CwdHookProbe<'a> = &'a dyn Fn(Option<&str>, Option<&str>, &str) -> bool;

/// The whole hook: payload in, answer out (empty for "leave it alone").
/// `hooked` answers for the payload's working directory (`None` when the
/// agent didn't send one): is this git hook installed there?
pub fn handle(
    agent: Agent,
    payload: &str,
    exe: &str,
    cfg: &Config,
    hooked: CwdHookProbe,
) -> String {
    parse(agent, payload)
        .and_then(|call| {
            let probe = |dir: Option<&str>, hook: &str| hooked(call.cwd.as_deref(), dir, hook);
            wrap(agent, &call, exe, cfg, &probe).map(|w| respond(agent, &call, &w))
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::simple_commands;

    const EXE: &str = "/opt/homebrew/bin/headroom";

    fn no_hooks(_: Option<&str>, _: Option<&str>, _: &str) -> bool {
        false
    }

    fn claude(cmd: &str, extra: Value) -> String {
        let mut input = json!({ "command": cmd, "description": "Run tests" });
        for (k, v) in extra.as_object().unwrap() {
            input[k] = v.clone();
        }
        json!({
            "session_id": "abc123", "cwd": "/repo", "hook_event_name": "PreToolUse",
            "tool_name": "Bash", "tool_input": input, "tool_use_id": "toolu_1"
        })
        .to_string()
    }

    fn out(agent: Agent, payload: &str) -> Value {
        let s = handle(agent, payload, EXE, &Config::default(), &no_hooks);
        serde_json::from_str(&s).unwrap_or(Value::Null)
    }

    #[test]
    fn claude_heavy_command_is_wrapped_with_every_field_echoed() {
        let o = out(Agent::Claude, &claude("pnpm test", json!({})));
        let input = &o["hookSpecificOutput"]["updatedInput"];
        assert_eq!(o["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert!(o["hookSpecificOutput"].get("permissionDecision").is_none());
        assert_eq!(input["description"], "Run tests");
        assert_eq!(
            input["command"],
            "/opt/homebrew/bin/headroom run --agent claude --session abc123 --wait-cap 480 --shell /bin/bash --sh 'pnpm test'"
        );
        // 120 s default + 480 s budget = the 600 s maximum.
        assert_eq!(input["timeout"], 600_000);
    }

    #[test]
    fn the_wrapped_line_runs_the_original_command_verbatim() {
        let original = r#"cd "my app" && pnpm vitest run -t 'it'\''s fine' | tee out.log"#;
        let o = out(Agent::Claude, &claude(original, json!({})));
        let cmd = o["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .unwrap()
            .to_string();
        let words = &simple_commands(&cmd)[0];
        assert_eq!(words.last().unwrap(), original);
        assert_eq!(words[words.len() - 2], "--sh");
    }

    #[test]
    fn claude_timeout_is_raised_only_as_far_as_the_maximum() {
        let o = out(
            Agent::Claude,
            &claude("pnpm test", json!({ "timeout": 300_000 })),
        );
        assert_eq!(o["hookSpecificOutput"]["updatedInput"]["timeout"], 600_000);
        let cmd = o["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(cmd.contains("--wait-cap 300 "), "{cmd}");
    }

    #[test]
    fn at_the_maximum_timeout_the_wait_still_gets_a_minute() {
        let o = out(
            Agent::Claude,
            &claude("pnpm test", json!({ "timeout": 600_000 })),
        );
        let input = &o["hookSpecificOutput"]["updatedInput"];
        assert_eq!(input["timeout"], 600_000);
        assert!(
            input["command"]
                .as_str()
                .unwrap()
                .contains("--wait-cap 60 ")
        );
    }

    #[test]
    fn background_commands_wait_without_a_cap_and_keep_their_timeout() {
        let o = out(
            Agent::Claude,
            &claude("pnpm build", json!({ "run_in_background": true })),
        );
        let input = &o["hookSpecificOutput"]["updatedInput"];
        assert!(
            input["command"]
                .as_str()
                .unwrap()
                .contains(" --background --shell /bin/bash --sh ")
        );
        assert!(input.get("timeout").is_none());
        assert_eq!(input["run_in_background"], true);
    }

    #[test]
    fn light_excluded_and_wrapped_commands_get_silence() {
        let cfg = Config::default();
        for cmd in ["git status", "pnpm dev", "headroom run --sh 'pnpm test'"] {
            assert_eq!(
                handle(Agent::Claude, &claude(cmd, json!({})), EXE, &cfg, &no_hooks),
                "",
                "{cmd}"
            );
        }
    }

    #[test]
    fn other_claude_tools_get_silence() {
        let p =
            json!({ "tool_name": "Read", "tool_input": { "command": "pnpm test" } }).to_string();
        assert_eq!(
            handle(Agent::Claude, &p, EXE, &Config::default(), &no_hooks),
            ""
        );
    }

    #[test]
    fn malformed_payloads_get_silence() {
        let cfg = Config::default();
        for p in [
            "",
            "not json",
            "{}",
            r#"{"tool_name":"Bash"}"#,
            r#"{"tool_name":"Bash","tool_input":"pnpm test"}"#,
            r#"{"tool_name":"Bash","tool_input":{"command":42}}"#,
            r#"{"tool_name":"Bash","tool_input":{"command":["pnpm", 1]}}"#,
        ] {
            assert_eq!(handle(Agent::Claude, p, EXE, &cfg, &no_hooks), "", "{p:?}");
            assert_eq!(handle(Agent::Codex, p, EXE, &cfg, &no_hooks), "", "{p:?}");
            assert_eq!(handle(Agent::Pi, p, EXE, &cfg, &no_hooks), "", "{p:?}");
        }
    }

    #[test]
    fn codex_wraps_a_string_or_argv_command_and_leaves_its_timeout() {
        let p = json!({ "session_id": "s1", "tool_name": "shell", "tool_input": { "command": "cargo test", "timeout_ms": 1000 } });
        let o = out(Agent::Codex, &p.to_string());
        let input = &o["hookSpecificOutput"]["updatedInput"];
        assert!(
            input["command"]
                .as_str()
                .unwrap()
                .contains("--agent codex --session s1 --wait-cap 480 --sh 'cargo test'")
        );
        assert_eq!(input["timeout_ms"], 1000);

        let p = json!({ "tool_input": { "command": ["pnpm", "vitest", "run"] } });
        let o = out(Agent::Codex, &p.to_string());
        assert!(
            o["hookSpecificOutput"]["updatedInput"]["command"]
                .as_str()
                .unwrap()
                .ends_with("--sh 'pnpm vitest run'")
        );
    }

    #[test]
    fn pi_gets_its_own_small_answer() {
        let o = out(
            Agent::Pi,
            &json!({ "command": "pnpm test", "cwd": "/r" }).to_string(),
        );
        assert_eq!(
            o["command"],
            "/opt/homebrew/bin/headroom run --agent pi --wait-cap 480 --shell /bin/bash --sh 'pnpm test'"
        );
        assert!(o.get("timeout").is_none());
        let o = out(
            Agent::Pi,
            &json!({ "command": "pnpm test", "timeout": 60 }).to_string(),
        );
        assert_eq!(o["timeout"], 540);
    }

    #[test]
    fn a_commit_is_wrapped_where_the_payloads_repository_has_the_hook() {
        let asked = std::cell::RefCell::new(vec![]);
        let hooked = |cwd: Option<&str>, dir: Option<&str>, hook: &str| {
            asked.borrow_mut().push((
                cwd.map(str::to_string),
                dir.map(str::to_string),
                hook.to_string(),
            ));
            cwd == Some("/repo") && hook == "pre-commit"
        };
        let cfg = Config::default();
        let s = handle(
            Agent::Claude,
            &claude("git commit -m done", json!({})),
            EXE,
            &cfg,
            &hooked,
        );
        let v: Value = serde_json::from_str(&s).unwrap();
        let cmd = v["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .unwrap();
        assert!(
            cmd.starts_with(EXE) && cmd.ends_with(" --sh 'git commit -m done'"),
            "{cmd}"
        );
        assert_eq!(
            asked.borrow()[0],
            (Some("/repo".to_string()), None, "pre-commit".to_string())
        );
        // No pre-push hook there: a push is left alone.
        assert_eq!(
            handle(
                Agent::Claude,
                &claude("git push", json!({})),
                EXE,
                &cfg,
                &hooked
            ),
            ""
        );
        // pi sends its cwd too; without one the probe is asked with none.
        let pi = handle(
            Agent::Pi,
            r#"{"command":"git commit -m x","cwd":"/repo"}"#,
            EXE,
            &cfg,
            &hooked,
        );
        assert!(pi.contains("git commit"), "{pi}");
        assert_eq!(
            handle(
                Agent::Pi,
                r#"{"command":"git commit -m x"}"#,
                EXE,
                &cfg,
                &hooked
            ),
            ""
        );
        assert_eq!(asked.borrow().last().unwrap().0, None);
    }

    #[test]
    fn executable_paths_with_spaces_are_quoted() {
        let s = handle(
            Agent::Pi,
            r#"{"command":"pnpm test"}"#,
            "/Users/me/My Tools/headroom",
            &Config::default(),
            &no_hooks,
        );
        let v: Value = serde_json::from_str(&s).unwrap();
        assert!(
            v["command"]
                .as_str()
                .unwrap()
                .starts_with("'/Users/me/My Tools/headroom' run ")
        );
    }

    #[test]
    fn agent_names_round_trip() {
        for a in Agent::ALL {
            assert_eq!(Agent::parse(a.name()), Some(a));
        }
        assert_eq!(Agent::parse("cursor"), None);
    }
}
