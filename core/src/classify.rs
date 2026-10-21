//! Is this command heavy, and what is it, for profiling purposes?
//!
//! Two jobs. The hook asks "should this go through the queue?", and the
//! profile asks "which earlier runs predict this one?". Both need the same
//! reduction of a command line to the part that decides its workload:
//! `cd web && FOO=1 pnpm --filter app vitest run src/a.test.ts` is
//! `vitest run`, the same workload as yesterday's `npx vitest run src/b.test.ts`.

use crate::config::Config;
use crate::shell::simple_commands;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Class {
    /// Already runs through `headroom run`; wrapping again would queue twice.
    Wrapped,
    Light,
    /// Long-lived (a dev server, a watcher): it would hold a slot forever,
    /// so it is never queued.
    Excluded {
        why: String,
    },
    Heavy {
        cmd: String,
        program: String,
    },
}

/// A simple command reduced to what identifies its workload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Normal {
    pub program: String,
    /// Program plus subcommand or script: `["pnpm", "test"]`, `["vitest", "run"]`.
    pub key: Vec<String>,
    /// Every argument after the program, for flag-based exclusions.
    pub args: Vec<String>,
}

impl Normal {
    pub fn cmd(&self) -> String {
        self.key.join(" ")
    }
}

/// Builtins and plumbing that never carry the workload themselves.
const NOT_WORK: &[&str] = &[
    "cd", "pushd", "popd", "export", "source", ".", "echo", "printf", "true", "false", "set",
    "unset", "test", "[", "mkdir", "rm", "cp", "mv", "cat", "tee", "grep", "rg", "head", "tail",
    "sleep", "wait", "ls", "pwd", "which", "git", "exit", "return", ":",
];

/// Tools a package manager can run directly (`pnpm vitest run`). Without
/// this list `pnpm vitest` would read as a package script named "vitest".
const PM_TOOLS: &[&str] = &[
    "vitest",
    "jest",
    "tsc",
    "playwright",
    "eslint",
    "next",
    "nx",
    "turbo",
    "prettier",
    "vite",
    "webpack",
    "tsx",
    "lefthook",
];

/// Tools whose first positional argument is a test filter or a path, never
/// a subcommand: `jest auth` is jest, not a workload called "auth".
const FILTER_TOOLS: &[&str] = &["jest", "pytest", "tsc", "eslint", "prettier", "mocha"];

pub fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn is_assignment(w: &str) -> bool {
    match w.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !name.starts_with(|c: char| c.is_ascii_digit())
        }
        None => false,
    }
}

fn is_flag(w: &str) -> bool {
    w.starts_with('-') && w.len() > 1
}

/// Could this argument be a subcommand or script name, rather than a path,
/// a file or a value?
fn subcommand_like(w: &str) -> bool {
    // Starts with a letter: `sleep 30` and `hog 1 6` take values, not subcommands.
    w.starts_with(|c: char| c.is_ascii_alphabetic())
        && !is_flag(w)
        && !w.contains(['/', '.', '='])
        && w.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_:".contains(c))
}

/// Index of the first argument that isn't a flag (or a flag's value).
fn first_positional(args: &[String], value_flags: &[&str]) -> Option<usize> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if is_flag(a) {
            i += if value_flags.contains(&a.as_str()) {
                2
            } else {
                1
            };
        } else {
            return Some(i);
        }
    }
    None
}

/// Reduce one simple command. `None` means it carries no workload of its own
/// (`cd`, `echo`, an empty line after the wrappers).
pub fn normalize(words: &[String]) -> Option<Normal> {
    let mut i = 0;
    while i < words.len() && is_assignment(&words[i]) {
        i += 1;
    }
    // Wrappers that run the real command: look through them.
    loop {
        let w = words.get(i)?;
        match basename(w) {
            "time" | "nohup" | "command" | "exec" | "caffeinate" | "sudo" | "stdbuf" => {
                i += 1;
                while words.get(i).is_some_and(|w| is_flag(w)) {
                    i += 1;
                }
            }
            "nice" => {
                i += 1;
                match words.get(i).map(String::as_str) {
                    Some("-n") => i += 2,
                    Some(w) if is_flag(w) => i += 1,
                    _ => {}
                }
            }
            "timeout" => {
                i += 1;
                while words.get(i).is_some_and(|w| is_flag(w)) {
                    i += 1;
                }
                i += 1; // the duration
            }
            "env" => {
                i += 1;
                while words.get(i).is_some_and(|w| is_flag(w) || is_assignment(w)) {
                    i += 1;
                }
            }
            // Version managers and project runners: `mise exec -- cargo test`.
            "mise" | "rtx"
                if matches!(words.get(i + 1).map(String::as_str), Some("exec" | "x")) =>
            {
                i += 2;
                match words[i..].iter().position(|w| w == "--") {
                    Some(dd) => i += dd + 1,
                    // Without `--`: tool specs (node@20) and flags come first.
                    None => {
                        while words.get(i).is_some_and(|w| is_flag(w) || w.contains('@')) {
                            i += 1;
                        }
                    }
                }
            }
            "asdf" | "rbenv" | "pyenv" | "nodenv" | "bundle"
                if words.get(i + 1).is_some_and(|w| w == "exec") =>
            {
                i += 2;
            }
            "uv" | "poetry" | "pipenv" if words.get(i + 1).is_some_and(|w| w == "run") => {
                i += 2;
                while words.get(i).is_some_and(|w| is_flag(w)) {
                    i += 1;
                }
            }
            "direnv" if words.get(i + 1).is_some_and(|w| w == "exec") => i += 3,
            // `corepack yarn test`: the package manager it picks runs the work.
            "corepack"
                if words.get(i + 1).is_some_and(|w| {
                    ["yarn", "pnpm", "npm", "npx", "pnpx", "yarnpkg"].contains(&basename(w))
                }) =>
            {
                i += 1
            }
            _ => break,
        }
    }
    let program = basename(&words[i]).to_string();
    if NOT_WORK.contains(&program.as_str()) {
        return None;
    }
    let rest = &words[i + 1..];
    Some(tool(&program, rest))
}

fn normal(program: &str, key: &[&str], args: &[String]) -> Normal {
    Normal {
        program: program.to_string(),
        key: key.iter().map(|s| s.to_string()).collect(),
        args: args.to_vec(),
    }
}

fn tool(program: &str, rest: &[String]) -> Normal {
    match program {
        "pnpm" | "npm" | "yarn" | "bun" => package_manager(program, rest),
        "npx" | "pnpx" | "bunx" => {
            let i = first_positional(rest, &["-p", "--package"]);
            match i.and_then(|i| normalize(&rest[i..])) {
                Some(n) => n,
                None => normal(program, &[program], rest),
            }
        }
        "python" | "python3" if rest.first().is_some_and(|a| a == "-m") && rest.len() > 1 => {
            tool(&rest[1], &rest[2..])
        }
        "docker" => docker(rest),
        "xcodebuild" => {
            const ACTIONS: &[&str] = &[
                "build",
                "test",
                "archive",
                "analyze",
                "build-for-testing",
                "test-without-building",
                "clean",
            ];
            let action = rest.iter().find(|a| ACTIONS.contains(&a.as_str()));
            normal(program, &[program, action.map_or("build", |a| a)], rest)
        }
        "gradle" | "gradlew" => match first_positional(rest, &["-p", "--project-dir"]) {
            Some(i) => normal("gradle", &["gradle", &rest[i]], rest),
            None => normal("gradle", &["gradle"], rest),
        },
        "turbo" => {
            let i = first_positional(rest, &["--filter", "-F", "--concurrency"]);
            match i.map(|i| (i, rest[i].as_str())) {
                Some((i, "run")) => match rest[i + 1..].iter().position(|a| !is_flag(a)) {
                    Some(j) => normal(program, &[program, &rest[i + 1 + j]], rest),
                    None => normal(program, &[program], rest),
                },
                Some((_, task)) => normal(program, &[program, task], rest),
                None => normal(program, &[program], rest),
            }
        }
        "vitest" => {
            const SUBS: &[&str] = &[
                "run",
                "watch",
                "dev",
                "bench",
                "related",
                "list",
                "typecheck",
            ];
            let sub = rest.iter().find(|a| !is_flag(a)).map(String::as_str);
            // Outside a terminal plain `vitest` runs once, the same workload as `vitest run`.
            let sub = sub.filter(|s| SUBS.contains(s)).unwrap_or("run");
            normal(program, &[program, sub], rest)
        }
        p if FILTER_TOOLS.contains(&p) => normal(program, &[program], rest),
        // `lefthook run pre-push`: each hook is its own workload.
        "lefthook" => match rest.iter().filter(|a| !is_flag(a)).collect::<Vec<_>>()[..] {
            [run, hook, ..] if run == "run" => normal(program, &[program, run, hook], rest),
            _ => generic(program, rest),
        },
        _ => generic(program, rest),
    }
}

fn generic(program: &str, rest: &[String]) -> Normal {
    // cargo +nightly test: the toolchain selector is not the subcommand.
    let sub = rest
        .iter()
        .filter(|a| !a.starts_with('+'))
        .find(|a| !is_flag(a))
        .filter(|a| subcommand_like(a));
    match sub {
        Some(s) => normal(program, &[program, s], rest),
        None => normal(program, &[program], rest),
    }
}

fn package_manager(pm: &str, rest: &[String]) -> Normal {
    let value_flags: &[&str] = match pm {
        "pnpm" => &["--filter", "-F", "-C", "--dir"],
        "npm" => &["--prefix", "-w", "--workspace"],
        "yarn" => &["--cwd"],
        _ => &["--cwd", "--filter"],
    };
    let Some(i) = first_positional(rest, value_flags) else {
        return normal(pm, &[pm], rest);
    };
    let first = rest[i].as_str();
    match first {
        "run" | "run-script" => match first_positional(&rest[i + 1..], value_flags) {
            Some(j) => normal(pm, &[pm, &rest[i + 1 + j]], rest),
            None => normal(pm, &[pm], rest),
        },
        "exec" | "x" | "dlx" => {
            normalize(&rest[i + 1..]).unwrap_or_else(|| normal(pm, &[pm], rest))
        }
        "t" | "tst" => normal(pm, &[pm, "test"], rest),
        t if PM_TOOLS.contains(&t) => tool(t, &rest[i + 1..]),
        script => normal(pm, &[pm, script], rest),
    }
}

fn docker(rest: &[String]) -> Normal {
    let flags = &[
        "-H",
        "--host",
        "--context",
        "-c",
        "--config",
        "-f",
        "--file",
        "-p",
        "--project-name",
    ];
    let Some(i) = first_positional(rest, flags) else {
        return normal("docker", &["docker"], rest);
    };
    match rest[i].as_str() {
        "build" => normal("docker", &["docker", "build"], rest),
        "buildx" | "image" if rest.get(i + 1).is_some_and(|a| a == "build") => {
            normal("docker", &["docker", "build"], rest)
        }
        "compose" => match first_positional(&rest[i + 1..], flags) {
            Some(j) => normal("docker", &["docker", "compose", &rest[i + 1 + j]], rest),
            None => normal("docker", &["docker", "compose"], rest),
        },
        sub => normal("docker", &["docker", sub], rest),
    }
}

/// Pattern token against key token. `*` matches anything; `test` also
/// matches script variants `test:unit` and `test-e2e`.
fn token_matches(pat: &str, tok: &str) -> bool {
    pat == "*"
        || tok == pat
        || tok
            .strip_prefix(pat)
            .is_some_and(|rest| rest.starts_with(':') || rest.starts_with('-'))
}

/// A heavy pattern's word also matches it at the end of a script name:
/// `test` covers `unit-test` and `storybook:test` as well as `test:unit`.
fn heavy_token_matches(pat: &str, tok: &str) -> bool {
    token_matches(pat, tok)
        || tok
            .strip_suffix(pat)
            .is_some_and(|rest| rest.ends_with(':') || rest.ends_with('-'))
}

/// `test`, `test:unit`, `unit-test`: a run that ends, whatever else the
/// name says. `storybook-test` runs Storybook's tests; it isn't Storybook.
fn is_test_script(tok: &str) -> bool {
    heavy_token_matches("test", tok)
}

fn pattern_matches(pattern: &str, key: &[String]) -> bool {
    let p: Vec<&str> = pattern.split_whitespace().collect();
    !p.is_empty() && p.len() <= key.len() && p.iter().zip(key).all(|(p, k)| token_matches(p, k))
}

fn heavy_pattern_matches(pattern: &str, key: &[String]) -> bool {
    let p: Vec<&str> = pattern.split_whitespace().collect();
    !p.is_empty()
        && p.len() <= key.len()
        && p.iter().zip(key).all(|(p, k)| heavy_token_matches(p, k))
}

fn excluded_by(n: &Normal, cfg: &Config) -> Option<String> {
    for e in &cfg.excluded {
        let hit = if e.starts_with("--") {
            n.args.iter().any(|a| {
                a == e
                    || a.strip_prefix(e.as_str())
                        .is_some_and(|r| r.starts_with('='))
            })
        } else if e.contains(' ') {
            pattern_matches(e, &n.key)
        } else {
            n.key
                .iter()
                .any(|k| token_matches(e, k) && !is_test_script(k))
        };
        if hit {
            return Some(e.clone());
        }
    }
    None
}

pub fn is_heavy(n: &Normal, cfg: &Config) -> bool {
    cfg.patterns.iter().any(|p| heavy_pattern_matches(p, &n.key))
        // Running a repository's hooks by hand runs whatever they hold.
        || (n.program == "lefthook" && n.key.len() == 3)
}

/// A git command that runs one of the repository's hooks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitHook {
    /// `git -C <dir>`, relative to where the line runs.
    pub dir: Option<String>,
    /// `commit` or `push`.
    pub sub: &'static str,
    /// The hook it runs: `pre-commit` or `pre-push`.
    pub hook: &'static str,
}

/// The hook this simple command runs, if any. A commit runs pre-commit and
/// a push runs pre-push, unless told not to (`--no-verify`, `LEFTHOOK=0`,
/// `HUSKY=0`). Whether the repository has that hook is for the caller to
/// look up.
pub fn git_hook(words: &[String]) -> Option<GitHook> {
    let mut i = 0;
    while i < words.len() && is_assignment(&words[i]) {
        if ["LEFTHOOK=0", "LEFTHOOK=false", "HUSKY=0"].contains(&words[i].as_str()) {
            return None;
        }
        i += 1;
    }
    if basename(words.get(i)?) != "git" {
        return None;
    }
    i += 1;
    let mut dir = None;
    while let Some(w) = words.get(i).filter(|w| is_flag(w)) {
        match w.as_str() {
            "-C" => dir = words.get(i + 1).cloned(),
            "-c" => {}
            _ => {
                i += 1;
                continue;
            }
        }
        i += 2;
    }
    let rest = &words.get(i + 1..)?;
    let skip = |short: bool| {
        rest.iter()
            .any(|a| a == "--no-verify" || (short && a == "-n"))
    };
    let (sub, hook) = match words.get(i)?.as_str() {
        "commit" if !skip(true) => ("commit", "pre-commit"),
        "push" if !skip(false) => ("push", "pre-push"),
        _ => return None,
    };
    Some(GitHook { dir, sub, hook })
}

fn wrapped(words: &[String]) -> bool {
    words.first().is_some_and(|w| basename(w) == "headroom")
        && words.get(1).is_some_and(|w| w == "run")
}

/// Is a git hook installed? Given `git -C`'s directory, if any, and the
/// hook's name.
pub type HookProbe<'a> = &'a dyn Fn(Option<&str>, &str) -> bool;

/// [`classify_in`] without a repository to look in: git commands are light.
pub fn classify(line: &str, cfg: &Config) -> Class {
    classify_in(line, cfg, &|_, _| false)
}

/// Classify a line run where `hooked` can tell which git hooks exist: a
/// commit or push in a repository with that hook is as heavy as the hook.
pub fn classify_in(line: &str, cfg: &Config, hooked: HookProbe) -> Class {
    let cmds = simple_commands(line);
    if cmds.iter().any(|w| wrapped(w)) {
        return Class::Wrapped;
    }
    let mut heavy: Option<Normal> = None;
    for words in &cmds {
        if let Some(g) = git_hook(words) {
            if heavy.is_none() && hooked(g.dir.as_deref(), g.hook) {
                heavy = Some(normal("git", &["git", g.sub], &words[1..]));
            }
            continue;
        }
        let Some(n) = normalize(words) else { continue };
        // One long-lived part makes the whole line long-lived: wrapping
        // `pnpm build && pnpm start` would hold a slot until the server stops.
        if let Some(why) = excluded_by(&n, cfg) {
            return Class::Excluded { why };
        }
        if heavy.is_none() && is_heavy(&n, cfg) {
            heavy = Some(n);
        }
    }
    match heavy {
        Some(n) => Class::Heavy {
            cmd: n.cmd(),
            program: n.program,
        },
        None => Class::Light,
    }
}

/// What to call a command that is queued regardless of its class, as with an
/// explicit `headroom run`: the heavy part if there is one, else the first
/// part with a workload, else the line itself.
pub fn describe(line: &str, cfg: &Config) -> (String, String) {
    describe_in(line, cfg, &|_, _| false)
}

pub fn describe_in(line: &str, cfg: &Config, hooked: HookProbe) -> (String, String) {
    if let Class::Heavy { cmd, program } = classify_in(line, cfg, hooked) {
        return (cmd, program);
    }
    for words in simple_commands(line) {
        if let Some(n) = normalize(&words) {
            return (n.cmd(), n.program);
        }
    }
    let short: String = line.chars().take(40).collect();
    (short, "sh".to_string())
}

/// Profile keys. Worktrees of one repository share `repo` (the main
/// checkout), so a fresh worktree starts with its siblings' history.
pub fn profile_keys(repo: &str, subdir: &str, cmd: &str, program: &str) -> (String, String) {
    let key = if subdir.is_empty() {
        format!("{repo}|{cmd}")
    } else {
        format!("{repo}|{subdir}|{cmd}")
    };
    (key, format!("{repo}|{program}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cls(line: &str) -> Class {
        classify(line, &Config::default())
    }

    fn heavy(cmd: &str, program: &str) -> Class {
        Class::Heavy {
            cmd: cmd.into(),
            program: program.into(),
        }
    }

    fn excluded(why: &str) -> Class {
        Class::Excluded { why: why.into() }
    }

    #[test]
    fn package_manager_scripts_are_named_by_script() {
        assert_eq!(cls("pnpm test"), heavy("pnpm test", "pnpm"));
        assert_eq!(cls("npm run build"), heavy("npm build", "npm"));
        assert_eq!(cls("npm t"), heavy("npm test", "npm"));
        assert_eq!(
            cls("yarn test:unit --coverage"),
            heavy("yarn test:unit", "yarn")
        );
        assert_eq!(cls("pnpm --filter web test"), heavy("pnpm test", "pnpm"));
        assert_eq!(cls("pnpm -F web run e2e-ci"), heavy("pnpm e2e-ci", "pnpm"));
    }

    #[test]
    fn tools_reached_through_runners_are_the_tool() {
        assert_eq!(
            cls("pnpm vitest run src/a.test.ts"),
            heavy("vitest run", "vitest")
        );
        assert_eq!(cls("npx vitest run"), heavy("vitest run", "vitest"));
        assert_eq!(cls("npx -y vitest"), heavy("vitest run", "vitest"));
        assert_eq!(
            cls("pnpm exec playwright test --project=chromium"),
            heavy("playwright test", "playwright")
        );
        assert_eq!(cls("./node_modules/.bin/jest auth"), heavy("jest", "jest"));
        assert_eq!(cls("python -m pytest tests/"), heavy("pytest", "pytest"));
        assert_eq!(
            cls("bunx tsc --noEmit -p tsconfig.json"),
            heavy("tsc", "tsc")
        );
    }

    #[test]
    fn compound_lines_find_the_heavy_part() {
        assert_eq!(
            cls("cd apps/web && FOO=1 pnpm vitest run src/x"),
            heavy("vitest run", "vitest")
        );
        assert_eq!(cls("pnpm test 2>&1 | tail -40"), heavy("pnpm test", "pnpm"));
        assert_eq!(
            cls("time nice -n 10 cargo test -p core"),
            heavy("cargo test", "cargo")
        );
        assert_eq!(
            cls("env -i PATH=/bin timeout 60 pytest"),
            heavy("pytest", "pytest")
        );
    }

    #[test]
    fn other_tools() {
        assert_eq!(
            cls("cargo +nightly build --release"),
            heavy("cargo build", "cargo")
        );
        assert_eq!(cls("docker build -t x ."), heavy("docker build", "docker"));
        assert_eq!(
            cls("docker buildx build ."),
            heavy("docker build", "docker")
        );
        assert_eq!(
            cls("xcodebuild -scheme App test"),
            heavy("xcodebuild test", "xcodebuild")
        );
        assert_eq!(
            cls("xcodebuild -scheme App"),
            heavy("xcodebuild build", "xcodebuild")
        );
        assert_eq!(
            cls("turbo run build --filter=web"),
            heavy("turbo build", "turbo")
        );
        assert_eq!(cls("next build"), heavy("next build", "next"));
        assert_eq!(cls("go test ./..."), heavy("go test", "go"));
        assert_eq!(
            cls("./gradlew assemble"),
            heavy("gradle assemble", "gradle")
        );
    }

    #[test]
    fn numeric_arguments_are_values_not_subcommands() {
        let cfg = Config::default();
        assert_eq!(
            describe("headroom-hog 1 6", &cfg),
            ("headroom-hog".into(), "headroom-hog".into())
        );
        assert_eq!(describe("make -j8 build", &cfg).0, "make build");
    }

    #[test]
    fn light_commands_stay_light() {
        for line in [
            "ls -la",
            "git status",
            "cat package.json",
            "pnpm install",
            "cargo fmt",
            "rg foo src",
            "echo pnpm test",
            "cd web",
            "",
            "   ",
            "node script.js",
            "test -f x && echo y",
        ] {
            assert_eq!(cls(line), Class::Light, "{line:?}");
        }
    }

    #[test]
    fn long_lived_commands_are_excluded() {
        assert_eq!(cls("pnpm dev"), excluded("dev"));
        assert_eq!(cls("pnpm run dev:web"), excluded("dev"));
        assert_eq!(cls("npm start"), excluded("start"));
        assert_eq!(cls("vitest --watch"), excluded("--watch"));
        assert_eq!(cls("jest --watchAll"), excluded("--watchAll"));
        assert_eq!(cls("vitest watch"), excluded("watch"));
        assert_eq!(cls("playwright test --ui"), excluded("--ui"));
        assert_eq!(cls("docker compose up -d"), excluded("docker compose up"));
        assert_eq!(cls("tsc --watch=true"), excluded("--watch"));
    }

    #[test]
    fn a_long_lived_part_excludes_the_whole_line() {
        assert_eq!(cls("pnpm build && pnpm start"), excluded("start"));
    }

    #[test]
    fn already_wrapped_is_not_wrapped_again() {
        assert_eq!(cls("headroom run --sh 'pnpm test'"), Class::Wrapped);
        assert_eq!(
            cls("/opt/homebrew/bin/headroom run -- pnpm test"),
            Class::Wrapped
        );
        assert_eq!(cls("cd x && headroom run -- cargo test"), Class::Wrapped);
        // A different headroom subcommand is just a command.
        assert_eq!(cls("headroom ls"), Class::Light);
    }

    #[test]
    fn pattern_tokens_match_script_variants_only_at_separators() {
        assert!(token_matches("test", "test"));
        assert!(token_matches("test", "test:unit"));
        assert!(token_matches("test", "test-e2e"));
        assert!(!token_matches("test", "testing"));
        assert!(!token_matches("test", "unit-test"));
        assert!(token_matches("*", "anything"));
    }

    #[test]
    fn user_patterns_extend_and_trim_the_defaults() {
        let mut cfg = Config::default();
        assert_eq!(classify("bun test", &cfg), heavy("bun test", "bun"));
        cfg.patterns = vec!["eslint".into()];
        assert_eq!(classify("pnpm test", &cfg), Class::Light);
        assert_eq!(classify("npx eslint .", &cfg), heavy("eslint", "eslint"));
        cfg.patterns.clear();
        assert_eq!(classify("vitest run", &cfg), Class::Light);
    }

    #[test]
    fn an_empty_pattern_matches_nothing() {
        let cfg = Config {
            patterns: vec!["".into(), "  ".into()],
            ..Config::default()
        };
        assert_eq!(classify("pnpm test", &cfg), Class::Light);
    }

    #[test]
    fn version_managers_and_runners_are_looked_through() {
        let cargo_test = heavy("cargo test", "cargo");
        assert_eq!(cls("mise exec -- cargo test -p core"), cargo_test);
        assert_eq!(cls("mise x -- cargo test"), cargo_test);
        assert_eq!(cls("mise exec rust@1.97 -- cargo test"), cargo_test);
        assert_eq!(cls("mise exec rust@1.97 cargo test"), cargo_test);
        assert_eq!(
            cls("cd core && mise exec -- cargo test 2>&1 | tail -5"),
            cargo_test
        );
        assert_eq!(cls("asdf exec cargo test"), cargo_test);
        assert_eq!(cls("bundle exec rake test"), heavy("rake test", "rake"));
        assert!(matches!(
            cls("uv run --frozen pytest -x"),
            Class::Heavy { .. }
        ));
        assert!(matches!(cls("poetry run pytest"), Class::Heavy { .. }));
        assert!(matches!(
            cls("direnv exec . pnpm test"),
            Class::Heavy { .. }
        ));
        // Their other subcommands are not wrappers.
        assert_eq!(cls("mise install"), Class::Light);
        assert_eq!(cls("uv sync"), Class::Light);
        assert_eq!(cls("mise exec --"), Class::Light, "nothing to run");
        // A long-lived command stays long-lived behind a wrapper.
        assert!(matches!(
            cls("mise exec -- pnpm dev"),
            Class::Excluded { .. }
        ));
    }

    /// A repository whose hooks are `hooks`, under `git -C` directory `dir`.
    fn in_repo(line: &str, dir: Option<&str>, hooks: &[&str]) -> Class {
        classify_in(line, &Config::default(), &|d, h| {
            d == dir && hooks.contains(&h)
        })
    }

    #[test]
    fn commits_and_pushes_are_as_heavy_as_the_repositorys_hooks() {
        let both = &["pre-commit", "pre-push"];
        assert_eq!(
            in_repo("git commit -m 'Fix the thing'", None, both),
            heavy("git commit", "git")
        );
        assert_eq!(
            in_repo("git push -u origin HEAD", None, both),
            heavy("git push", "git")
        );
        assert_eq!(
            in_repo(
                "git add -A && git commit --amend --no-edit && git push",
                None,
                both
            ),
            heavy("git commit", "git"),
            "the first hooked part names it"
        );
        assert_eq!(
            in_repo("git -C web commit -m x", Some("web"), both),
            heavy("git commit", "git")
        );
        assert_eq!(
            in_repo("git -c core.editor=true --no-pager push", None, both),
            heavy("git push", "git")
        );
        // Only the hook the command runs counts.
        assert_eq!(in_repo("git push", None, &["pre-commit"]), Class::Light);
        assert_eq!(
            in_repo("git commit -m x", None, &["pre-push"]),
            Class::Light
        );
    }

    #[test]
    fn commits_and_pushes_without_hooks_stay_light() {
        let both = &["pre-commit", "pre-push"];
        assert_eq!(in_repo("git commit -m x", None, &[]), Class::Light);
        assert_eq!(cls("git push"), Class::Light, "no repository to look in");
        for line in [
            "git commit --no-verify -m x",
            "git commit -n -m x",
            "git push --no-verify",
            "LEFTHOOK=0 git push",
            "HUSKY=0 git commit -m x",
            "git status",
            "git log --oneline -5",
            "git",
        ] {
            assert_eq!(in_repo(line, None, both), Class::Light, "{line}");
        }
        // `-n` on a push is a dry run, which still runs pre-push.
        assert_eq!(in_repo("git push -n", None, both), heavy("git push", "git"));
        // The hook of another directory is not this one's.
        assert_eq!(in_repo("git -C web commit -m x", None, both), Class::Light);
    }

    #[test]
    fn corepack_runs_the_package_manager_it_picks() {
        assert_eq!(
            cls("corepack yarn typecheck"),
            heavy("yarn typecheck", "yarn")
        );
        assert_eq!(
            cls("corepack yarn test 2>&1 | tail -15"),
            heavy("yarn test", "yarn")
        );
        assert_eq!(
            cls("corepack pnpm exec vitest run src/a.test.ts"),
            heavy("vitest run", "vitest")
        );
        assert_eq!(
            cls("cd apps/web && corepack yarn turbo run typecheck --affected"),
            heavy("turbo typecheck", "turbo")
        );
        // corepack's own commands are not a package manager running work.
        assert_eq!(cls("corepack enable"), Class::Light);
        assert_eq!(cls("corepack prepare yarn@4 --activate"), Class::Light);
        assert_eq!(cls("corepack"), Class::Light);
        // Long-lived behind corepack stays long-lived.
        assert!(matches!(cls("corepack yarn dev"), Class::Excluded { .. }));
    }

    #[test]
    fn linters_and_mocha_are_heavy_by_default() {
        assert_eq!(cls("npx eslint src --ext .ts"), heavy("eslint", "eslint"));
        assert_eq!(cls("yarn eslint --fix apps/x"), heavy("eslint", "eslint"));
        assert_eq!(
            cls("NODE_ENV=test npx mocha -r ./setup.js tests/unit/a.test.js"),
            heavy("mocha", "mocha")
        );
    }

    #[test]
    fn scripts_ending_in_test_are_tests_not_servers() {
        assert_eq!(cls("yarn unit-test"), heavy("yarn unit-test", "yarn"));
        assert_eq!(
            cls("yarn storybook-test"),
            heavy("yarn storybook-test", "yarn")
        );
        assert_eq!(cls("pnpm run e2e:test"), heavy("pnpm e2e:test", "pnpm"));
        assert_eq!(
            cls("npm run start-server-and-test"),
            heavy("npm start-server-and-test", "npm"),
            "it starts a server, runs the tests, and stops"
        );
        // Still long-lived: the server itself, and watch mode.
        assert!(matches!(cls("yarn storybook"), Class::Excluded { .. }));
        assert!(matches!(cls("pnpm dev:web"), Class::Excluded { .. }));
        assert!(matches!(
            cls("yarn unit-test --watch"),
            Class::Excluded { .. }
        ));
        // Only at a separator: `latest` and `contest` are not tests.
        assert_eq!(cls("pnpm latest"), Class::Light);
        assert_eq!(cls("yarn contest"), Class::Light);
    }

    #[test]
    fn running_hooks_by_hand_is_heavy() {
        assert_eq!(
            cls("lefthook run pre-push"),
            heavy("lefthook run pre-push", "lefthook")
        );
        assert_eq!(
            cls("pnpm exec lefthook run pre-commit"),
            heavy("lefthook run pre-commit", "lefthook")
        );
        assert_eq!(
            cls("yarn lefthook run pre-push --force"),
            heavy("lefthook run pre-push", "lefthook")
        );
        assert_eq!(cls("lefthook install"), Class::Light);
        assert_eq!(cls("lefthook run"), Class::Light, "no hook named");
    }

    #[test]
    fn describe_names_anything_queued_explicitly() {
        let cfg = Config::default();
        assert_eq!(
            describe("pnpm test", &cfg),
            ("pnpm test".into(), "pnpm".into())
        );
        assert_eq!(
            describe("node build.js", &cfg),
            ("node".into(), "node".into())
        );
        assert_eq!(describe("cd x", &cfg), ("cd x".into(), "sh".into()));
        assert_eq!(describe("exit 3", &cfg), ("exit 3".into(), "sh".into()));
        assert_eq!(describe("node a.js; exit 3", &cfg).1, "node");
    }

    #[test]
    fn worktrees_share_keys_through_the_repo() {
        let (k, f) = profile_keys("/r/browser", "", "vitest run", "vitest");
        assert_eq!(k, "/r/browser|vitest run");
        assert_eq!(f, "/r/browser|vitest");
        let (k, _) = profile_keys("/r/browser", "apps/web", "vitest run", "vitest");
        assert_eq!(k, "/r/browser|apps/web|vitest run");
    }
}
