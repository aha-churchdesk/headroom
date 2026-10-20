//! `headroom mcp`: an MCP server on stdio, for agents that want to see the
//! queue or reorder it.
//!
//! Optional on purpose. Gating happens in each agent's hook whether or not
//! the agent ever calls a tool; these tools only explain and reprioritise.
//! The transport is newline-delimited JSON-RPC 2.0, small enough to answer
//! by hand: initialize, ping, tools/list and tools/call.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use headroom_core::classify::{Class, classify, describe, profile_keys};
use headroom_core::model::{Bytes, GIB, Job, JobState};
use headroom_core::protocol::{ClientMsg, Control, DaemonMsg, Listing, request};
use headroom_core::store::Store;
use serde_json::{Map, Value, json};

use crate::context::git_context;

const PROTOCOL: &str = "2025-06-18";

const INSTRUCTIONS: &str = "Headroom queues heavy commands (tests, builds, e2e runs) so parallel \
agents don't run the machine out of memory. Your shell commands are already routed through it by \
a hook: run them as usual and don't wrap them in `headroom run` yourself. A command that waits is \
queued, not stuck. Use these tools to see why something waits, estimate a command's memory before \
starting it, or move a job up the queue.";

fn gb(b: Bytes) -> f64 {
    (b as f64 / GIB as f64 * 10.0).round() / 10.0
}

fn tools() -> Value {
    let id = json!({ "type": "integer", "description": "Job id, from list_jobs" });
    json!([
        {
            "name": "system_status",
            "description": "Memory, pressure and queue at a glance, and why the queue is waiting if it is.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "list_jobs",
            "description": "Running and queued jobs in queue order, with their memory use and estimates.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "estimate_command",
            "description": "Whether a shell command would be queued, and how much memory it is expected to peak at, from past runs in this repository.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The shell command, as you would run it" },
                    "cwd": { "type": "string", "description": "Directory it would run in; defaults to the server's" }
                },
                "required": ["command"]
            }
        },
        {
            "name": "move_job",
            "description": "Move a queued job to the top of the queue, or to a position (0 = next).",
            "inputSchema": {
                "type": "object",
                "properties": { "id": id, "to": { "type": "integer", "minimum": 0 } },
                "required": ["id"]
            }
        },
        {
            "name": "pin_job",
            "description": "Pin a job: it runs next without waiting for memory, and is never paused or killed. on=false unpins.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": id, "on": { "type": "boolean" } },
                "required": ["id"]
            }
        }
    ])
}

fn job_json(j: &Job, pos: Option<usize>) -> Value {
    json!({
        "id": j.id,
        "state": match j.state {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Frozen => "frozen",
        },
        "position": pos,
        "label": j.spec.label,
        "command": j.spec.full_cmd,
        "cwd": j.spec.cwd,
        "agent": j.spec.agent,
        "pinned": j.pinned,
        "low": j.spec.low,
        "estimate_gb": gb(j.estimate),
        "footprint_gb": gb(j.footprint),
        "profiled_runs": j.samples,
    })
}

fn listing(store: &Store) -> Result<Listing, String> {
    match request(&store.socket(), &ClientMsg::List)? {
        DaemonMsg::Jobs(l) => Ok(l),
        other => Err(format!("unexpected answer from headroomd: {other:?}")),
    }
}

fn status(l: &Listing) -> Value {
    let s = &l.snapshot;
    json!({
        "pressure": s.pressure,
        "memory_used_gb": gb(s.mem_used),
        "memory_total_gb": gb(s.mem_total),
        "memory_available_gb": gb(s.mem_available),
        "swap_used_gb": gb(s.swap_used),
        "running": l.running.len(),
        "queued": l.queued.len(),
        "held": l.held,
        "waiting_because": l.blocked.as_ref().map(|b| b.describe()),
    })
}

fn control(store: &Store, c: Control) -> Result<Value, String> {
    match request(&store.socket(), &ClientMsg::Control(c))? {
        DaemonMsg::Error { msg } => Err(msg),
        _ => listing(store).map(|l| jobs(&l)),
    }
}

fn jobs(l: &Listing) -> Value {
    let running = l.running.iter().map(|j| job_json(j, None));
    let queued = l
        .queued
        .iter()
        .enumerate()
        .map(|(i, j)| job_json(j, Some(i)));
    json!({
        "jobs": running.chain(queued).collect::<Vec<_>>(),
        "waiting_because": l.blocked.as_ref().map(|b| b.describe()),
    })
}

fn estimate(store: &Store, command: &str, cwd: &Path) -> Value {
    let cfg = store.load_config();
    match classify(command, &cfg) {
        Class::Light | Class::Wrapped => json!({
            "queued": false,
            "why": "not a heavy command; it runs immediately",
        }),
        Class::Excluded { why } => json!({
            "queued": false,
            "why": format!("long-lived ({why}), so it is never queued"),
        }),
        Class::Heavy { .. } => {
            let git = git_context(cwd);
            let (cmd, program) = describe(command, &cfg);
            let (key, fallback) = profile_keys(&git.repo, &git.subdir, &cmd, &program);
            let e = store
                .load_profiles()
                .estimate(&key, &fallback, cfg.default_estimate);
            json!({
                "queued": true,
                "profile": key,
                "estimate_gb": gb(e.bytes),
                "profiled_runs": e.samples,
                "basis": if e.samples == 0 { "no runs of its own yet: the repository's or the default estimate" } else { "p90 of past peaks" },
            })
        }
    }
}

fn id_arg(args: &Map<String, Value>) -> Result<u64, String> {
    args.get("id")
        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
        .ok_or_else(|| "id must be a job id from list_jobs".to_string())
}

fn call(store: &Store, cwd: &Path, name: &str, args: &Map<String, Value>) -> Result<Value, String> {
    match name {
        "system_status" => listing(store).map(|l| status(&l)),
        "list_jobs" => listing(store).map(|l| jobs(&l)),
        "estimate_command" => {
            let command = args
                .get("command")
                .and_then(Value::as_str)
                .filter(|c| !c.trim().is_empty())
                .ok_or("command is required")?;
            let dir = args
                .get("cwd")
                .and_then(Value::as_str)
                .map_or_else(|| cwd.to_path_buf(), PathBuf::from);
            Ok(estimate(store, command, &dir))
        }
        "move_job" => {
            let id = id_arg(args)?;
            let c = match args
                .get("to")
                .map(|v| v.as_u64().ok_or("to must be a position, 0 or more"))
            {
                None => Control::Top { id },
                Some(to) => Control::Move {
                    id,
                    to: to? as usize,
                },
            };
            control(store, c)
        }
        "pin_job" => {
            let id = id_arg(args)?;
            let on = args.get("on").and_then(Value::as_bool).unwrap_or(true);
            control(store, Control::Pin { id, on })
        }
        other => Err(format!("unknown tool {other}")),
    }
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// The answer to one message, or `None` for a notification.
pub fn handle(store: &Store, cwd: &Path, line: &str) -> Option<Value> {
    let Ok(msg) = serde_json::from_str::<Value>(line) else {
        return Some(error(Value::Null, -32700, "parse error"));
    };
    let method = msg.get("method").and_then(Value::as_str);
    // No id: a notification (initialized, cancelled), which gets no answer.
    let id = msg.get("id").cloned()?;
    let Some(method) = method else {
        return Some(error(id, -32600, "invalid request"));
    };
    let params = msg.get("params").and_then(Value::as_object);
    let result = match method {
        "initialize" => {
            let version = params
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL);
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "headroom", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => {
            let Some(name) = params.and_then(|p| p.get("name")).and_then(Value::as_str) else {
                return Some(error(id, -32602, "tools/call needs a tool name"));
            };
            let empty = Map::new();
            let args = params
                .and_then(|p| p.get("arguments"))
                .and_then(Value::as_object)
                .unwrap_or(&empty);
            // Tool failures are results the model can read, not protocol errors.
            match call(store, cwd, name, args) {
                Ok(v) => json!({
                    "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }],
                    "structuredContent": v,
                    "isError": false,
                }),
                Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
            }
        }
        _ => return Some(error(id, -32601, &format!("method not found: {method}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

pub fn serve(input: impl BufRead, mut output: impl Write, store: &Store) -> io::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_default();
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(answer) = handle(store, &cwd, &line) {
            serde_json::to_writer(&mut output, &answer)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}

pub fn main() -> i32 {
    let store = match Store::open() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("headroom mcp: can't open the data directory: {e}");
            return 1;
        }
    };
    match serve(io::stdin().lock(), io::stdout().lock(), &store) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("headroom mcp: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use headroom_core::profile::{Profiles, Run};

    fn store(name: &str) -> (Store, PathBuf) {
        let root = std::env::temp_dir().join(format!("hr-mcp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("work")).unwrap();
        (Store::at(root.join("data")).unwrap(), root.join("work"))
    }

    fn ask(s: &Store, cwd: &Path, line: &str) -> Value {
        handle(s, cwd, line).expect("an answer")
    }

    fn tool(s: &Store, cwd: &Path, name: &str, args: Value) -> Value {
        let req = json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                          "params": { "name": name, "arguments": args } });
        ask(s, cwd, &req.to_string())["result"].clone()
    }

    #[test]
    fn initialize_echoes_the_clients_version_and_explains_the_hook() {
        let (s, cwd) = store("init");
        let r = ask(
            &s,
            &cwd,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
        );
        assert_eq!(r["id"], 1);
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(r["result"]["serverInfo"]["name"], "headroom");
        assert!(
            r["result"]["instructions"]
                .as_str()
                .unwrap()
                .contains("don't wrap them")
        );
        let r = ask(
            &s,
            &cwd,
            r#"{"jsonrpc":"2.0","id":"a","method":"initialize"}"#,
        );
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL);
        assert_eq!(r["id"], "a", "string ids come back as they were");
    }

    #[test]
    fn lists_five_tools_each_with_a_schema() {
        let (s, cwd) = store("list");
        let r = ask(
            &s,
            &cwd,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        );
        let names: Vec<&str> = r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                assert_eq!(t["inputSchema"]["type"], "object");
                t["name"].as_str().unwrap()
            })
            .collect();
        assert_eq!(
            names,
            [
                "system_status",
                "list_jobs",
                "estimate_command",
                "move_job",
                "pin_job"
            ]
        );
    }

    #[test]
    fn notifications_get_no_answer_and_ping_gets_an_empty_one() {
        let (s, cwd) = store("notify");
        assert!(
            handle(
                &s,
                &cwd,
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
            )
            .is_none()
        );
        assert_eq!(
            ask(&s, &cwd, r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#)["result"],
            json!({})
        );
    }

    #[test]
    fn protocol_errors_carry_json_rpc_codes() {
        let (s, cwd) = store("errors");
        assert_eq!(ask(&s, &cwd, "{not json")["error"]["code"], -32700);
        assert_eq!(
            ask(
                &s,
                &cwd,
                r#"{"jsonrpc":"2.0","id":4,"method":"resources/list"}"#
            )["error"]["code"],
            -32601
        );
        assert_eq!(
            ask(&s, &cwd, r#"{"jsonrpc":"2.0","id":5}"#)["error"]["code"],
            -32600
        );
        assert_eq!(
            ask(
                &s,
                &cwd,
                r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{}}"#
            )["error"]["code"],
            -32602
        );
    }

    #[test]
    fn without_a_daemon_queue_tools_fail_as_readable_results() {
        let (s, cwd) = store("nodaemon");
        for (name, args) in [
            ("system_status", json!({})),
            ("list_jobs", json!({})),
            ("pin_job", json!({ "id": 3 })),
        ] {
            let r = tool(&s, &cwd, name, args);
            assert_eq!(r["isError"], true, "{name}");
            assert_eq!(r["content"][0]["text"], "headroomd isn't running", "{name}");
        }
    }

    #[test]
    fn bad_arguments_are_reported_before_anything_is_sent() {
        let (s, cwd) = store("badargs");
        for (name, args, says) in [
            ("move_job", json!({}), "id must be"),
            ("move_job", json!({ "id": "j4" }), "id must be"),
            ("move_job", json!({ "id": 4, "to": -1 }), "to must be"),
            ("pin_job", json!({ "id": 4.5 }), "id must be"),
            (
                "estimate_command",
                json!({ "command": "  " }),
                "command is required",
            ),
            ("frobnicate", json!({}), "unknown tool"),
        ] {
            let r = tool(&s, &cwd, name, args.clone());
            assert_eq!(r["isError"], true, "{name} {args}");
            assert!(
                r["content"][0]["text"].as_str().unwrap().contains(says),
                "{name} {args}: {r}"
            );
        }
    }

    #[test]
    fn estimates_say_whether_a_command_is_queued_and_from_what() {
        let (s, cwd) = store("estimate");
        let light = tool(&s, &cwd, "estimate_command", json!({ "command": "ls -la" }));
        assert_eq!(light["structuredContent"]["queued"], false);
        let dev = tool(
            &s,
            &cwd,
            "estimate_command",
            json!({ "command": "pnpm dev" }),
        );
        assert!(
            dev["structuredContent"]["why"]
                .as_str()
                .unwrap()
                .starts_with("long-lived")
        );

        let fresh = tool(
            &s,
            &cwd,
            "estimate_command",
            json!({ "command": "pnpm vitest run", "cwd": cwd }),
        );
        let c = &fresh["structuredContent"];
        assert_eq!(
            (c["queued"].clone(), c["profiled_runs"].clone()),
            (json!(true), json!(0))
        );
        assert_eq!(c["estimate_gb"], 1.5, "the default");

        // Three runs at 4 GB in this directory: trusted.
        let key = c["profile"].as_str().unwrap().to_string();
        let mut p = Profiles::default();
        for _ in 0..3 {
            let run = Run {
                peak: 4 * GIB,
                duration_ms: 1000,
                exit: Some(0),
                flagged: false,
                at: 0,
            };
            p.record(&key, &key, run);
        }
        s.save_profiles(&p).unwrap();
        let known = tool(
            &s,
            &cwd,
            "estimate_command",
            json!({ "command": "pnpm vitest run", "cwd": cwd }),
        );
        assert_eq!(known["structuredContent"]["estimate_gb"], 4.0);
        assert_eq!(known["structuredContent"]["profiled_runs"], 3);
    }

    #[test]
    fn serve_answers_line_by_line_and_skips_blank_lines() {
        let (s, _) = store("serve");
        let input = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n";
        let mut out = Vec::new();
        serve(input.as_bytes(), &mut out, &s).unwrap();
        let lines: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            lines.iter().map(|l| l["id"].clone()).collect::<Vec<_>>(),
            [json!(1), json!(2)]
        );
    }
}
