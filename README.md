# Headroom

Headroom is a macOS menu bar app and CLI that schedules heavy development commands so parallel coding agents can share your Mac without freezing it.

- **Agent integration.** Routes tests, builds, e2e runs, and type checks from Codex, Claude Code, and pi through one queue. Other tools can use the CLI.
- **Live job overview.** Shows the agent behind each job, memory and CPU use, subprocesses, elapsed time, and usual runtime when known.
- **Workload-specific estimates.** Learns memory and runtime separately for complete invocations, including package filters, test files, and build flags. Shows when history is missing or memory use varies widely.
- **Scheduling explanations.** Shows why jobs wait and how memory availability, system pressure, CPU load, and runtime estimates affect the queue.
- **Worker adjustment.** Can reduce parallel workers in supported tools to fit a job into available memory, while respecting explicit worker settings.
- **Queue controls.** Hold, reorder, pin, pause, resume, or stop jobs from the menu bar. Inspect past runs and the memory used by other apps.

Status: early. Apple Silicon only, ad-hoc signed, used daily by one person.

## How it decides

- **Memory, not job count.** Headroom records peak memory per repository, starting directory, and complete invocation. With at least three successful measured runs, the estimate uses their p90 peak among the last 20 attempts. Until then, it blends with broader repository history or a 1.5 GB default. Failed or interrupted peaks remain minimum memory requirements, and known worker counts can raise the estimate. A queued job normally starts when its estimate fits in available memory, minus a reserve (2 GB by default), minus what running jobs are still expected to grow into. Memory includes processes started by the job, including tasks that move to their own process groups.
- **Pressure.** Under normal memory pressure jobs start as above. Under warn, one job may start only when nothing else of ours is running. Under critical, nothing starts.
- **Queue order.** The queue considers its first job before later jobs. With "Short jobs go first" disabled, later jobs wait if the first does not fit. When enabled, the exception below applies. An oversized estimate can still start when Headroom has no running jobs, provided the queue is not held and memory pressure is not critical.
- **Short jobs may go first.** Enabled by default. While the head waits on memory, a later job may start if it fits now and its typical run time says it will be done before the head could start anyway (when the running jobs are expected to finish). A job with no history never goes first. The head is passed over at most 3 times or for 5 minutes; after that the queue waits for it. The popover shows `passed over 2× · strict in 3m`.
- **The guard.** Under sustained critical pressure the newest job gets SIGSTOP. If pressure stays critical it is killed and requeued once; a second kill gives up and the agent sees exit 137 with the reason. Low-priority jobs go first, and a job using more than twice its estimate (and over 8 GB) goes before anything else. Freezing and killing reach every process the job started. Pinned jobs and processes Headroom didn't start are never touched.
- **Fail open.** If the daemon isn't running or stops answering, `headroom run` runs the command directly. Headroom can slow a command down; it can't stop one from running.

## Shrinking a job to fit

Vitest, turbo and cargo run work concurrently, and memory use can grow with the worker count. When the next job doesn't fit, Headroom can start it with fewer workers instead of making it wait. The agent sees one line saying so: `starting with 6 vitest workers instead of 14 so it fits in memory`.

| Tool | How | Default it assumes |
|---|---|---|
| vitest | `VITEST_MAX_WORKERS` | cores - 1 |
| turbo | `--concurrency=N` appended to the command | 10 |
| cargo | `CARGO_BUILD_JOBS` | cores |

It is found in the command itself or behind an npm script (`pnpm test` running `vitest run`). Headroom leaves the count alone when you chose one yourself: a flag in the command or script (`--maxWorkers`, `--concurrency`, `-j`), the environment variable, or a parallelism setting in `vitest.config.*`, `vite.config.*` or `.cargo/config.toml`. It never goes below 2 workers, because one worker can change how a test suite behaves and not only how fast it runs.

Runs record worker counts when Headroom can determine them. With successful measured runs at two or more counts, peak memory is fitted as a line in the worker count, kept above the observations; failed or interrupted peaks can raise that allowance. Before that, a quarter of the peak is assumed to be fixed cost. A command with no history is never tuned. Turn it off in Settings ("Shrink jobs to fit"), or for one tool or repository with `"tune_off": ["turbo", "/path/to/repo"]` in `config.json`.

A foreground command waits at most 8 minutes in the queue (under the 10 minute limit of Claude Code's Bash tool), then exits 75 with "still queued, try again later". The hook raises the Bash tool's timeout by the time it may spend queued.

## Install

From source (Rust 1.99, pinned in `mise.toml`):

```sh
git clone https://github.com/aha-churchdesk/headroom
cd headroom
./scripts/install.sh --login --hooks
```

That builds `Headroom.app` into `/Applications`, links the CLI to `~/.local/bin/headroom` (`--bin DIR` for elsewhere), starts the app now and at login, and installs the agent hooks. Leave out `--login` or `--hooks` to do those yourself later. It never edits your shell config; if the bin directory isn't on your PATH it prints the line to add. Run it again after `git pull` to upgrade. `./scripts/install.sh --uninstall` removes the hooks, the login item, the link and the app, and keeps your learned profiles.

To install a bundle you have already built and tested, run `./scripts/install.sh --bundle ./Headroom.app`. Updates stage and verify the replacement before stopping the installed app, refuse to restart a busy queue, and preserve the existing login registration. The installer checks that the app is running and its queue responds before reporting success. If replacement fails, it restores the previous bundle when safe. If the new app has accepted jobs or its state cannot be checked, it keeps both bundles and reports the backup path. The idle check happens immediately before shutdown; a new job arriving between those steps can still lose its queue connection and run directly.

Homebrew:

```sh
brew install aha-churchdesk/tap/headroom
brew services start headroom       # menu bar app, now and at login
```

## Connect your agents

```sh
headroom hook install              # every agent found on this Mac
headroom hook status
```

This adds one `PreToolUse` entry to each agent's config and touches nothing else in the file. It refuses to write a file that isn't valid JSON, and keeps the previous version as `<file>.headroom-bak`. `--dry-run` prints the diff instead of writing, `--agent claude|codex|pi` picks one agent, and `--scope project|local` writes Claude Code's project settings instead of `~/.claude/settings.json`.

| Agent | Where | After installing |
|---|---|---|
| Claude Code | `~/.claude/settings.json` | New sessions pick it up. |
| Codex | `~/.codex/hooks.json` | Open `/hooks` in Codex and review Headroom before it runs. |
| pi | `~/.pi/agent/extensions/headroom.ts` | Run `/reload` in pi. |

`headroom hook uninstall` removes exactly that entry.

For Codex, `headroom hook status` asks the local Codex app server whether the current user hook is approved. It reports **approved**, **needs review** for new or changed hooks, or **disabled**. If verification is unavailable, it falls back to the local configuration. A stored approval alone is shown as **check in Codex** until verified. The menu bar checks in the background and discards cached approval when `hooks.json` or `config.toml` changes. Pending and unavailable checks stay neutral; approval and disabled warnings appear only after Codex confirms them. Open `/hooks` in Codex to review and approve changes. Headroom only reads hook metadata; it never approves hooks or starts a model session. The check uses your home directory's configuration; individual sessions can apply different project, profile, or command-line settings. See the [Codex hook documentation](https://learn.chatgpt.com/docs/hooks).

Codex capture handles only canonical `Bash` calls, including unified exec. Heavy commands receive the `permissionDecision: "allow"` required by Codex's input-rewrite protocol, together with the wrapped command.

### Terminals and everything else: PATH shims

Agent hooks only see the command an agent types. To also queue what you run in a terminal, what a git hook or task runner starts, and agents without a hook:

```sh
headroom shims install             # links pnpm, npm, npx, yarn, bun, corepack, vitest, jest, playwright,
                                   # cargo, tsc, turbo, nx, xcodebuild, docker to headroom
export PATH="$HOME/.local/share/headroom/shims:$PATH"   # at the end of ~/.zshrc, yourself
headroom shims status              # are they first on PATH?
```

`install` and `status` print the line for your shell and the file it goes in: `~/.zshrc` for zsh, `~/.bash_profile` for bash, and `fish_add_path --path --prepend …` in `~/.config/fish/config.fish` for fish. It goes at the end, after fnm, mise, nvm or brew have set PATH; `status` names any program that is still found before the shims.

Called as `pnpm`, Headroom classifies the whole command. A heavy one runs through the queue; anything else, and anything inside a queued job, runs the real `pnpm` (the next one on PATH) straight away. If the daemon isn't running the command runs directly. `headroom shims uninstall` removes the links.

Claude Code note: the hook rewrites the command to `headroom run … --sh '<your command>'` and leaves the permission decision to you. Allow rules written for the raw command (`Bash(pnpm test:*)`) won't match the rewritten one, so you may see a prompt you didn't see before. Deny and ask rules still apply.

## Which commands are heavy

Commands that match a pattern are queued: `* test`, `* build`, `* e2e`, `* typecheck`, `vitest`, `jest`, `mocha`, `playwright`, `tsc`, `eslint`, `nx`, `turbo`, `next build`, `xcodebuild`, `cargo clippy`, `gradle`, `pytest`. `*` stands for any program, so `* test` covers `pnpm test` and `cargo test`, and `test` also covers scripts like `test:unit` and `unit-test`. `corepack yarn …` is classified as the `yarn …` it runs. Long-lived commands are never queued, because they would hold a slot forever: `dev`, `serve`, `start`, `watch`, `preview`, `storybook`, anything with `--watch` or `--ui`, and `docker compose up`.

A `git commit` or `git push` is queued when the repository has a pre-commit or pre-push hook (lefthook, husky or a plain script), because the hook runs the type checks and linters. Its command label is `git commit` or `git push`; its complete invocation determines the workload profile. `--no-verify`, `LEFTHOOK=0` and `HUSKY=0` skip the hook and the queue. `lefthook run <hook>` is always queued.

Check one:

```sh
$ headroom classify 'pnpm vitest run src/calendar'
heavy: queued, command type `vitest run`
$ headroom classify 'pnpm dev'
long-lived (dev): never queued
```

Patterns are editable in the menu bar's Settings page.

Profiles distinguish the complete invocation and its starting repository directory. Package filters, test files, build flags, inline environment settings, directory changes, and compound commands get separate history. Identical invocations share history across worktrees at the same repository subdirectory. Direct arguments and shell commands have separate profiles.

Earlier broad profiles remain fallback estimates; each new workload collects its own measured peaks and durations. Old runs cannot be split reliably because they did not store their complete invocation. The identity does not track ambient environment, script/configuration contents, or cache state, so the same command can still vary. Different shell spelling or quoting also creates a separate profile.

## The menu bar

The icon is a four-bar memory gauge. It turns amber or red under memory pressure, and the title shows running·queued jobs, with ❄︎ while something is frozen and ⏸︎ while the queue is held.

The popover lists running and queued jobs, their reported agent, and why the queue is waiting (`next needs 4.2 GB, 2.9 GB free`). Running jobs show memory, CPU, elapsed time including pauses, and usual runtime when known. The largest process inside a job appears when it differs from the command itself: `git commit`, then `↳ tsc --noEmit` while the hook runs. Expand a job to see its full command, queue wait, memory growth allowance, and how runtime estimates, pinning, or worker counts affect scheduling.

After at least three successful, uninterrupted measured runs of the same workload at the same recorded worker count, the popover also shows the historical p10–p90 band of peak memory. The conservative scheduling estimate stays separate. This band describes past runs; it is not a guarantee for the current run. When the spread exceeds fourfold and 1 GB, the UI says “Memory varies widely” instead of drawing a band; the observed bounds remain in the explanation.

From it you can:

- drag queued jobs to reorder them, move one to the top, or cancel it;
- **pin** a job, so it runs next, skips the memory check and is never frozen or killed;
- mark a job **low**, so it runs at background CPU priority and is first to be frozen;
- freeze, resume or kill a running job;
- **hold** the queue during a call or a demo (running jobs carry on).

Hover a number for what it means. It also has a history of the last 50 runs with each command's memory profile, a Commands page, the top memory users Headroom doesn't manage, and settings.

The Commands page ranks the commands that ran in the last 7, 30 or 90 days by time running, number of runs, memory or time waiting. Memory is GB-hours: each run's peak times its run time, the memory it kept from other work. Totals are kept per command and day for 90 days.

## CLI

```
headroom run [--background] [--wait-cap SECS] [--low] [--label L] ([--shell PATH] --sh 'LINE' | -- CMD ARGS…)
headroom ls
headroom hold on|off
headroom pin|unpin|low|normal|top|cancel|kill|freeze|resume ID
headroom reset-profile KEY
headroom classify 'LINE'
headroom hook install|uninstall|status [--agent claude|codex|pi|all] [--scope user|project|local] [--dry-run]
headroom shims install|uninstall|status [--dir DIR]
headroom mcp
```

`--sh` uses `$SHELL`, defaulting to `/bin/zsh`. `--shell PATH` selects another shell. The Claude Code and pi hooks select Bash so a different login shell does not change their commands' syntax.

Exit codes from `headroom run`: the command's own, or 75 (still queued when the wait cap ran out), 130 (cancelled from the queue), 137 (killed twice to free memory).

A `headroom run` inside a job runs directly (the job sets `HEADROOM_JOB`), so a script that calls another heavy script can't deadlock behind itself.

## MCP server

Optional. The hooks do the gating whether an agent calls these tools or not. `headroom mcp` speaks MCP over stdio with five tools: `system_status`, `list_jobs`, `estimate_command`, `move_job` and `pin_job`.

```sh
claude mcp add headroom -- headroom mcp
```

## Files

Everything lives in `~/Library/Application Support/dev.headroom/` (or `$HEADROOM_HOME`): `config.json`, `profiles.json`, `history.json`, `stats.json` (the Commands page's totals) and the daemon's socket. Delete `profiles.json` to forget what Headroom has learned.

`headroomd --headless` runs the queue without the menu bar. `headroomd --probe` prints what it measures and exits.

## Development

```sh
./scripts/test.sh             # fmt, clippy, Rust tests, UI tests
./scripts/test.sh --coverage  # the same, with the core coverage gate (90% lines)
```

With Headroom installed and running, queue the entire suite explicitly:

```sh
headroom run --label headroom-full-suite -- ./scripts/test.sh --coverage
```

This lets the live daemon schedule and measure the suite. The integration tests still use isolated daemons and temporary data directories.

The scheduling policy lives in `core/` with no platform calls, so the queue and the guard are tested with scripted memory readings. `headroom/tests/e2e.rs` runs a real daemon against real processes (`headroom-hog GB SECS` allocates memory on demand). The popover is a single HTML file, `ui/index.html`; opened in a browser it shows design scenarios, and `tests/ui/` renders it under node from states captured in real runs.
