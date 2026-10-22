//! Shrinking a job to fit instead of making it wait.
//!
//! Test runners and build tools start one worker per core by default, and
//! peak memory grows with the worker count. When the head of the queue
//! doesn't fit, admission may start it with fewer workers instead (see
//! `admission`); this module knows which tools can be told so, how, and
//! when the user has already decided for them.
//!
//! Only tools whose knob was checked against an installed version are here:
//!
//! | Tool   | Knob                         | Default workers                 |
//! |--------|------------------------------|---------------------------------|
//! | vitest | `VITEST_MAX_WORKERS` env     | cores - 1 (vitest 5.0.1 source) |
//! | turbo  | `--concurrency=N` argument   | 10 (turbo docs; not in --help)  |
//! | cargo  | `CARGO_BUILD_JOBS` env       | cores (cargo docs)              |
//!
//! vitest applies `VITEST_MAX_WORKERS` after its config and CLI flags, so it
//! would override a `maxWorkers` the user set. That is why a setting in the
//! command, the npm script or a vitest/vite config file turns tuning off.

use serde::{Deserialize, Serialize};

use crate::classify::basename;
use crate::shell::simple_commands;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    Vitest,
    Turbo,
    Cargo,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Tool::Vitest => "vitest",
            Tool::Turbo => "turbo",
            Tool::Cargo => "cargo",
        }
    }

    pub fn default_workers(self, cores: u32) -> u32 {
        match self {
            Tool::Vitest => cores.saturating_sub(1).max(1),
            Tool::Turbo => 10,
            Tool::Cargo => cores.max(1),
        }
    }

    /// Settings that mean the user chose the worker count themselves.
    fn explicit_flags(self) -> &'static [&'static str] {
        match self {
            Tool::Vitest => &[
                "--maxWorkers",
                "--max-workers",
                "--pool",
                "--poolOptions",
                "--fileParallelism",
                "--no-file-parallelism",
            ],
            // Parallel mode ignores concurrency. These path overrides select
            // configuration we have not inspected, so also leave them alone.
            Tool::Turbo => &["--concurrency", "--parallel", "--cwd", "--root-turbo-json"],
            Tool::Cargo => &["-j", "--jobs"],
        }
    }

    fn explicit_env(self) -> Option<&'static str> {
        match self {
            Tool::Vitest => Some("VITEST_MAX_WORKERS"),
            Tool::Turbo => None,
            Tool::Cargo => Some("CARGO_BUILD_JOBS"),
        }
    }

    /// Keys in the tool's config files that set parallelism.
    fn config_keys(self) -> &'static [&'static str] {
        match self {
            Tool::Vitest => &[
                "maxWorkers",
                "maxThreads",
                "maxForks",
                "singleThread",
                "singleFork",
                "fileParallelism",
            ],
            Tool::Turbo => &["concurrency"],
            Tool::Cargo => &["jobs"],
        }
    }

    /// Config files, relative to the directory a command runs in, that may
    /// set parallelism for this tool.
    pub fn config_files(self) -> &'static [&'static str] {
        match self {
            Tool::Vitest => &[
                "vitest.config.ts",
                "vitest.config.mts",
                "vitest.config.cts",
                "vitest.config.js",
                "vitest.config.mjs",
                "vitest.config.cjs",
                "vite.config.ts",
                "vite.config.mts",
                "vite.config.cts",
                "vite.config.js",
                "vite.config.mjs",
                "vite.config.cjs",
                "vitest.workspace.ts",
            ],
            Tool::Turbo => &["turbo.json", "turbo.jsonc"],
            Tool::Cargo => &[".cargo/config.toml", ".cargo/config"],
        }
    }
}

/// A job that can run with fewer workers, and how many it would use untouched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tunable {
    pub tool: Tool,
    pub default_workers: u32,
    /// The turbo argument goes after an npm script name: npm needs `--` first.
    #[serde(default)]
    pub npm: bool,
}

/// How a granted worker count is passed to the command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Knob {
    Env(&'static str, String),
    /// Appended to the command line, or to argv.
    Args(Vec<String>),
}

impl Tunable {
    pub fn knob(&self, workers: u32) -> Knob {
        match self.tool {
            Tool::Vitest => Knob::Env("VITEST_MAX_WORKERS", workers.to_string()),
            Tool::Cargo => Knob::Env("CARGO_BUILD_JOBS", workers.to_string()),
            Tool::Turbo => {
                let arg = format!("--concurrency={workers}");
                Knob::Args(if self.npm {
                    vec!["--".into(), arg]
                } else {
                    vec![arg]
                })
            }
        }
    }
}

const PACKAGE_MANAGERS: [&str; 4] = ["pnpm", "npm", "yarn", "bun"];
/// Package manager subcommands that are not script names.
const PM_BUILTINS: [&str; 10] = [
    "install", "add", "remove", "exec", "dlx", "x", "create", "why", "outdated", "update",
];

/// Package manager options that run a script somewhere other than here.
const ELSEWHERE: [&str; 11] = [
    "--filter",
    "-F",
    "--dir",
    "--cwd",
    "-C",
    "--prefix",
    "--workspace",
    "--workspaces",
    "-w",
    "--recursive",
    "-r",
];

/// A package-manager selector whose effective package/config is unresolved.
pub fn package_target_override(words: &[String]) -> bool {
    words
        .iter()
        .any(|w| PACKAGE_MANAGERS.contains(&basename(w)) || basename(w) == "npx")
        && words.iter().any(|w| {
            ELSEWHERE.iter().any(|flag| {
                w == flag
                    || w.starts_with(&format!("{flag}="))
                    || (flag.len() == 2 && w.starts_with(flag) && w.len() > flag.len())
            })
        })
}

/// The npm script a command runs, if it is a single `pnpm|npm|yarn|bun [run] NAME`
/// in this directory's package.json.
pub fn script_name(line: &str) -> Option<String> {
    let cmds = simple_commands(line);
    let [words] = cmds.as_slice() else {
        return None;
    };
    let mut it = words
        .iter()
        .skip_while(|w| w.contains('=') && !w.starts_with('-'));
    let pm = basename(it.next()?);
    if !PACKAGE_MANAGERS.contains(&pm) {
        return None;
    }
    // Another package's script: this directory's package.json says nothing about it.
    if package_target_override(words) {
        return None;
    }
    let mut name = it.find(|w| !w.starts_with('-'))?.as_str();
    if name == "run" || name == "run-script" {
        name = words
            .iter()
            .skip_while(|w| *w != "run" && *w != "run-script")
            .nth(1)?;
        return Some(name.to_string());
    }
    (!PM_BUILTINS.contains(&name)).then(|| name.to_string())
}

/// What the wrapper knows about a command before it is queued.
pub struct Probe<'a> {
    pub line: &'a str,
    /// The text of the npm script the line runs, from package.json.
    pub script: Option<&'a str>,
    /// The tool's config files near the command, concatenated.
    pub configs: &'a dyn Fn(Tool) -> String,
    /// Is this variable set in the wrapper's environment?
    pub env_set: &'a dyn Fn(&str) -> bool,
    pub cores: u32,
}

fn mentions(text: &str, program: &str) -> bool {
    simple_commands(text)
        .iter()
        .filter_map(|words| crate::classify::normalize(words))
        .any(|normal| normal.program == program)
}

fn has_flag(text: &str, flags: &[&str]) -> bool {
    simple_commands(text).iter().flatten().any(|w| {
        flags.iter().any(|f| {
            w == f
                || w.starts_with(&format!("{f}="))
                // -j4, -j 4
                || (f.len() == 2 && w.starts_with(f) && w[2..].chars().all(|c| c.is_ascii_digit()))
        })
    })
}

/// An injected variable must survive every env wrapper in the command and
/// resolved script. Unknown env options are left untuned rather than guessing
/// how they affect the environment or where the wrapped command starts.
fn clears_env(text: &str, variable: &str) -> bool {
    simple_commands(text).iter().any(|words| {
        words.iter().enumerate().any(|(i, word)| {
            if basename(word) != "env" {
                return false;
            }
            let mut args = words[i + 1..].iter();
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "-" | "-i" | "--ignore-environment" => return true,
                    "-u" | "--unset" => {
                        if args.next().is_none_or(|name| name == variable) {
                            return true;
                        }
                    }
                    "--" => break,
                    _ if arg.starts_with("--unset=") => {
                        if arg.strip_prefix("--unset=") == Some(variable) {
                            return true;
                        }
                    }
                    _ if arg.starts_with("-u") => {
                        if arg.strip_prefix("-u") == Some(variable) {
                            return true;
                        }
                    }
                    _ if arg.starts_with('-') => return true,
                    _ if arg.contains('=') => {}
                    _ => break,
                }
            }
            false
        })
    })
}

/// npm exec consumes appended options as npm configuration unless explicitly
/// forwarded. Its forwarding rules differ from npm run, which we handle below.
/// Global options can consume values, including optional boolean values, so
/// leave those invocations untuned rather than guessing the npm subcommand.
fn npm_exec_or_unresolved(text: &str) -> bool {
    simple_commands(text).iter().any(|words| {
        words.iter().enumerate().any(|(i, word)| {
            basename(word) == "npm"
                && words
                    .get(i + 1)
                    .is_none_or(|arg| arg.starts_with('-') || matches!(arg.as_str(), "exec" | "x"))
        })
    })
}

/// An appended argument must reach the existing command. A trailing shell
/// separator starts another command, while a comment swallows the argument.
fn can_append_arg(text: &str) -> bool {
    !text.contains(['#', '\n'])
        && !text.trim_end().ends_with('\\')
        && simple_commands(text).len() == 1
        && simple_commands(&format!("{text} --headroom-argument-probe")).len() == 1
}

/// npm consumes its first `--` after a script name. Any further separator,
/// or one already inside the script, forwards arguments to Turbo's tasks.
fn turbo_task_passthrough(line: &str, script: Option<&str>) -> bool {
    if script.is_some_and(|s| simple_commands(s).iter().flatten().any(|w| w == "--")) {
        return true;
    }
    let cmds = simple_commands(line);
    let [words] = cmds.as_slice() else {
        return true;
    };
    let separators: Vec<_> = words
        .iter()
        .enumerate()
        .filter(|(_, w)| *w == "--")
        .map(|(i, _)| i)
        .collect();
    if separators.is_empty() {
        return false;
    }
    let npm = words
        .iter()
        .find(|w| !w.contains('='))
        .is_some_and(|w| basename(w) == "npm");
    let npm_separator = script.is_some()
        && npm
        && separators.len() == 1
        && script_name(line).is_some_and(|name| {
            words
                .iter()
                .position(|w| w == &name)
                .is_some_and(|i| i < separators[0])
        });
    !npm_separator
}

/// Whether to tune at all, and how. `None` when the tool isn't one we know,
/// the user already set a worker count, or the line can't safely be edited.
pub fn detect(p: &Probe) -> Option<Tunable> {
    let text = format!("{} ; {}", p.line, p.script.unwrap_or(""));
    if simple_commands(&text)
        .iter()
        .any(|w| package_target_override(w))
    {
        return None;
    }
    let tool = if mentions(&text, "turbo") {
        // The argument is appended to the line, so it must land on turbo: one
        // simple command, nothing after it, and the script (if any) a single
        // turbo call too.
        let script_ok = p.script.is_none_or(|s| {
            let cmds = simple_commands(s);
            can_append_arg(s) && cmds[0].iter().any(|w| basename(w) == "turbo")
        });
        if !(can_append_arg(p.line) && script_ok)
            || turbo_task_passthrough(p.line, p.script)
            || npm_exec_or_unresolved(&text)
        {
            return None;
        }
        Tool::Turbo
    } else if mentions(&text, "vitest") {
        Tool::Vitest
    } else if simple_commands(&text).iter().any(|w| {
        w.first().is_some_and(|p| basename(p) == "cargo")
            && w.get(1).is_some_and(|s| {
                matches!(
                    s.as_str(),
                    "build" | "test" | "clippy" | "check" | "bench" | "doc"
                )
            })
    }) {
        Tool::Cargo
    } else {
        return None;
    };
    let explicit_env = tool.explicit_env().is_some_and(|v| {
        (p.env_set)(v)
            || clears_env(&text, v)
            || simple_commands(&text)
                .iter()
                .flatten()
                .any(|w| w.split_once('=').is_some_and(|(name, _)| name == v))
    });
    if has_flag(&text, tool.explicit_flags()) || explicit_env {
        return None;
    }
    let configs = (p.configs)(tool);
    if tool.config_keys().iter().any(|k| configs.contains(k)) {
        return None;
    }
    let npm = p.script.is_some()
        && simple_commands(p.line)
            .first()
            .and_then(|w| w.iter().find(|w| !w.contains('=')))
            .is_some_and(|w| basename(w) == "npm")
        && !simple_commands(p.line).iter().flatten().any(|w| w == "--");
    Some(Tunable {
        tool,
        default_workers: tool.default_workers(p.cores),
        npm: npm && tool == Tool::Turbo,
    })
}

/// The command line with turbo's argument appended; other tools use the
/// environment and leave the line alone.
pub fn apply_line(line: &str, knob: &Knob) -> String {
    match knob {
        Knob::Env(..) => line.to_string(),
        Knob::Args(a) => format!("{} {}", line.trim_end(), a.join(" ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(line: &str, script: Option<&str>, configs: &str, env: &[&str]) -> Option<Tunable> {
        let configs = configs.to_string();
        let set: Vec<String> = env.iter().map(|s| s.to_string()).collect();
        detect(&Probe {
            line,
            script,
            configs: &|_| configs.clone(),
            env_set: &|v| set.iter().any(|s| s == v),
            cores: 12,
        })
    }

    fn tool(line: &str) -> Option<Tool> {
        probe(line, None, "", &[]).map(|t| t.tool)
    }

    #[test]
    fn finds_the_tool_in_the_command() {
        assert_eq!(tool("pnpm vitest run src/a.test.ts"), Some(Tool::Vitest));
        assert_eq!(tool("npx vitest run"), Some(Tool::Vitest));
        assert_eq!(
            tool("cd web && ./node_modules/.bin/vitest run"),
            Some(Tool::Vitest)
        );
        assert_eq!(tool("turbo run build"), Some(Tool::Turbo));
        assert_eq!(tool("cargo test --workspace"), Some(Tool::Cargo));
        assert_eq!(tool("cargo fmt"), None, "not parallel work");
        assert_eq!(tool("pnpm jest"), None, "not verified, so not tuned");
        assert_eq!(tool("ls"), None);
    }

    #[test]
    fn finds_the_tool_behind_an_npm_script() {
        let t = probe("pnpm test", Some("vitest run --coverage"), "", &[]).unwrap();
        assert_eq!((t.tool, t.default_workers), (Tool::Vitest, 11));
        assert_eq!(
            probe("yarn build", Some("turbo run build"), "", &[]).map(|t| t.tool),
            Some(Tool::Turbo)
        );
        assert_eq!(
            probe("pnpm test", Some("node scripts/test.js"), "", &[]),
            None
        );
    }

    #[test]
    fn a_worker_count_the_user_chose_is_never_overridden() {
        assert_eq!(tool("pnpm vitest run --maxWorkers=4"), None);
        assert_eq!(tool("pnpm vitest run --maxWorkers 4"), None);
        assert_eq!(tool("vitest run --no-file-parallelism"), None);
        assert_eq!(tool("turbo run build --concurrency=50%"), None);
        assert_eq!(tool("cargo build -j4"), None);
        assert_eq!(tool("cargo build -j 4"), None);
        assert_eq!(tool("cargo build --jobs=2"), None);
        assert_eq!(
            probe("pnpm test", Some("vitest run --maxWorkers=2"), "", &[]),
            None
        );
        assert_eq!(probe("vitest run", None, "", &["VITEST_MAX_WORKERS"]), None);
        assert_eq!(probe("cargo test", None, "", &["CARGO_BUILD_JOBS"]), None);
        for line in [
            "VITEST_MAX_WORKERS=8 vitest run",
            "env VITEST_MAX_WORKERS=8 pnpm vitest run",
            "export VITEST_MAX_WORKERS=8; vitest run",
            "VITEST_MAX_WORKERS='' vitest run",
        ] {
            assert_eq!(probe(line, None, "", &[]), None, "{line}");
        }
        assert_eq!(
            probe(
                "pnpm test",
                Some("VITEST_MAX_WORKERS=8 vitest run"),
                "",
                &[]
            ),
            None
        );
        assert_eq!(
            probe("cargo test", Some("CARGO_BUILD_JOBS=8 cargo test"), "", &[]),
            None
        );
        assert_eq!(
            probe(
                "vitest run",
                None,
                "export default { test: { maxWorkers: 2 } }",
                &[]
            ),
            None
        );
        assert_eq!(probe("cargo build", None, "[build]\njobs = 4", &[]), None);
        // An unrelated config is no reason to stop.
        assert!(
            probe(
                "vitest run",
                None,
                "export default { test: { globals: true } }",
                &[]
            )
            .is_some()
        );
    }

    #[test]
    fn turbo_is_only_tuned_where_the_argument_reaches_it() {
        assert_eq!(tool("turbo run build && echo done"), None);
        assert_eq!(tool("turbo run build | tee log"), None);
        assert_eq!(tool("turbo run build # all of it"), None);
        for suffix in [";", "&", "\n", " # comment"] {
            let script = format!("turbo run build{suffix}");
            assert_eq!(tool(&script), None, "{script}");
            assert_eq!(
                probe("pnpm build", Some(&script), "", &[]),
                None,
                "{script}"
            );
        }
        assert_eq!(
            probe(
                "pnpm build",
                Some("turbo run build && next export"),
                "",
                &[]
            ),
            None
        );
        assert!(probe("pnpm build", Some("turbo run build"), "", &[]).is_some());
    }

    #[test]
    fn npm_exec_does_not_advertise_an_unforwarded_turbo_limit() {
        for line in [
            "npm exec turbo run build",
            "npm x turbo run build",
            "npm --yes exec turbo run build",
            "npm --registry https://registry.npmjs.org exec turbo run build",
            "npm --registry=https://registry.npmjs.org exec turbo run build",
            "npm --unknown run exec turbo run build",
            "npm exec -- turbo run build",
        ] {
            assert_eq!(tool(line), None, "{line}");
            assert_eq!(probe("npm run build", Some(line), "", &[]), None, "{line}");
        }
        for line in ["npm run build", "npm run build -- exec", "npm run exec"] {
            assert!(
                probe(line, Some("turbo run build"), "", &[]).is_some(),
                "{line}"
            );
        }
        for option in ["--yes", "-y", "--silent", "-s", "--quiet", "-q"] {
            for value in ["true", "false"] {
                let line = format!("npm {option} {value} exec turbo run build");
                assert_eq!(tool(&line), None, "{line}");
                assert_eq!(probe("npm run build", Some(&line), "", &[]), None, "{line}");
            }
        }
    }

    #[test]
    fn capturing_more_wrapper_syntax_does_not_add_invalid_tuning() {
        for line in [
            "npm exec -- turbo run build",
            "pnpm exec -- turbo run build",
            "uv run --project turbo echo build",
            "env -u turbo echo build",
            "env -u vitest echo test",
            "cargo --config build.jobs=2 test",
        ] {
            assert_eq!(tool(line), None, "{line}");
        }
        for line in ["npm exec -- vitest run", "pnpm exec -- vitest run"] {
            let tunable = probe(line, None, "", &[]).unwrap();
            assert_eq!(apply_line(line, &tunable.knob(2)), line);
        }
    }

    #[test]
    fn turbo_parallel_mode_and_configured_concurrency_are_not_tuned() {
        for line in [
            "turbo run build --parallel",
            "turbo run build --parallel=true",
        ] {
            assert_eq!(tool(line), None, "{line}");
            assert_eq!(probe("npm run build", Some(line), "", &[]), None, "{line}");
        }
        assert_eq!(
            probe("turbo run build", None, r#"{"concurrency":"50%"}"#, &[]),
            None
        );
        assert!(probe("turbo run build", None, r#"{"tasks":{"build":{}}}"#, &[]).is_some());
        for line in [
            "turbo run build --cwd elsewhere",
            "turbo run build --cwd=elsewhere",
            "turbo run build --root-turbo-json custom.json",
            "turbo run build --root-turbo-json=custom.json",
        ] {
            assert_eq!(tool(line), None, "{line}");
        }
    }

    #[test]
    fn environment_limits_must_survive_wrappers_and_resolved_scripts() {
        for (command, variable) in [
            ("vitest run", "VITEST_MAX_WORKERS"),
            ("cargo build", "CARGO_BUILD_JOBS"),
        ] {
            for option in [
                format!("-u {variable}"),
                format!("-u{variable}"),
                format!("--unset {variable}"),
                format!("--unset={variable}"),
                "-i PATH=/usr/bin:/bin".into(),
                "--ignore-environment PATH=/usr/bin:/bin".into(),
                "- PATH=/usr/bin:/bin".into(),
            ] {
                let line = format!("env {option} {command}");
                assert_eq!(tool(&line), None, "{line}");
                assert_eq!(probe("npm run test", Some(&line), "", &[]), None, "{line}");
                // Cargo is found in the resolved script even when the env
                // wrapper belongs to the outer package-manager command.
                let outer = format!("env {option} npm run test");
                assert_eq!(probe(&outer, Some(command), "", &[]), None, "{outer}");
            }
        }
        for line in [
            "env FOO=bar vitest run",
            "env -u OTHER vitest run",
            "env --unset=OTHER vitest run",
            "env -uOTHER vitest run",
            "env -- vitest run",
        ] {
            assert_eq!(tool(line), Some(Tool::Vitest), "{line}");
        }
        assert!(probe("env FOO=bar npm run build", Some("cargo build"), "", &[]).is_some());
    }

    #[test]
    fn turbo_task_passthrough_is_not_tuned() {
        for line in [
            "turbo run build -- --foo",
            "pnpm turbo run build -- --foo",
            "turbo run build '--' --foo",
        ] {
            assert_eq!(tool(line), None, "{line}");
        }
        for line in ["npm run build", "npm run build --", "pnpm build"] {
            assert_eq!(
                probe(line, Some("turbo run build -- --foo"), "", &[]),
                None,
                "{line}"
            );
        }
        for line in [
            "npm run build -- -- --foo",
            "pnpm build -- --foo",
            "yarn build -- --foo",
        ] {
            assert_eq!(
                probe(line, Some("turbo run build"), "", &[]),
                None,
                "{line}"
            );
        }
    }

    #[test]
    fn npm_forwarding_separator_still_allows_turbo_tuning() {
        for line in [
            "npm run build --",
            "npm run build -- --dry-run=json",
            "npm run build '--'",
        ] {
            let t = probe(line, Some("turbo run build"), "", &[]).unwrap();
            assert!(!t.npm, "{line}");
            assert_eq!(
                apply_line(line, &t.knob(2)),
                format!("{line} --concurrency=2")
            );
        }
    }

    #[test]
    fn knobs_and_line_edits() {
        let v = Tunable {
            tool: Tool::Vitest,
            default_workers: 11,
            npm: false,
        };
        assert_eq!(v.knob(3), Knob::Env("VITEST_MAX_WORKERS", "3".into()));
        assert_eq!(apply_line("pnpm test", &v.knob(3)), "pnpm test");
        let t = probe("turbo run build  ", None, "", &[]).unwrap();
        assert_eq!(
            apply_line("turbo run build  ", &t.knob(4)),
            "turbo run build --concurrency=4"
        );
        let npm = probe("npm run build", Some("turbo run build"), "", &[]).unwrap();
        assert!(npm.npm);
        assert_eq!(
            apply_line("npm run build", &npm.knob(4)),
            "npm run build -- --concurrency=4"
        );
        let pnpm = probe("pnpm build", Some("turbo run build"), "", &[]).unwrap();
        assert_eq!(
            apply_line("pnpm build", &pnpm.knob(4)),
            "pnpm build --concurrency=4"
        );
    }

    #[test]
    fn script_names() {
        assert_eq!(script_name("pnpm test").as_deref(), Some("test"));
        assert_eq!(script_name("npm run build").as_deref(), Some("build"));
        assert_eq!(script_name("npm run exec").as_deref(), Some("exec"));
        assert_eq!(
            script_name("yarn test:unit --silent").as_deref(),
            Some("test:unit")
        );
        assert_eq!(
            script_name("CI=1 pnpm --silent test").as_deref(),
            Some("test")
        );
        assert_eq!(
            script_name("pnpm --filter web test"),
            None,
            "another package's script"
        );
        assert_eq!(script_name("npm --prefix=web run test"), None);
        assert_eq!(script_name("pnpm install"), None);
        assert_eq!(script_name("pnpm test && pnpm build"), None, "two commands");
        assert_eq!(script_name("cargo test"), None);
        assert_eq!(script_name("pnpm"), None);
    }

    #[test]
    fn package_target_selectors_prevent_script_lookup_and_tuning() {
        for line in [
            "npm run build --workspaces",
            "npm --workspaces=true run build",
            "npm -wweb run build",
            "pnpm -Fweb build",
            "pnpm -r build",
            "yarn --cwd=web build",
            "yarn --cwd web build",
            "pnpm -Cweb build",
        ] {
            assert_eq!(script_name(line), None, "{line}");
            assert_eq!(
                probe(line, Some("turbo run build"), "", &[]),
                None,
                "{line}"
            );
        }
        assert_eq!(
            probe("npm run build", Some("pnpm -Cweb turbo run build"), "", &[]),
            None
        );
    }

    #[test]
    fn default_workers_never_reach_zero() {
        assert_eq!(Tool::Vitest.default_workers(1), 1);
        assert_eq!(Tool::Vitest.default_workers(0), 1);
        assert_eq!(Tool::Cargo.default_workers(0), 1);
    }
}
