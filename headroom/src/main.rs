//! headroom — run heavy commands through the Headroom queue.

mod context;
mod ctl;
mod hookcmd;
mod mcp;
mod run;

use headroom_core::hookio::Agent;
use headroom_core::hooksettings::Scope;
use headroom_core::protocol::Control;

const USAGE: &str = "\
headroom — queue heavy commands so parallel agents don't exhaust memory

usage:
  headroom run [--background] [--wait-cap SECS] [--low] [--label L] (--sh 'LINE' | -- CMD ARGS…)
  headroom ls
  headroom hold on|off
  headroom pin|unpin|low|normal|top|cancel|kill|freeze|resume ID
  headroom reset-profile KEY
  headroom classify 'LINE'
  headroom mcp                               (MCP server on stdio: queue status and controls)
  headroom hook install|uninstall|status [--agent claude|codex|pi|all] [--scope user|project|local] [--dry-run]
  headroom hook --agent claude|codex|pi      (called by the agent: payload on stdin)
";

fn usage() -> i32 {
    eprint!("{USAGE}");
    2
}

fn parse_run(args: &[String]) -> Option<run::RunOpts> {
    let mut o = run::RunOpts::default();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let val = || args.get(i + 1).cloned();
        match a {
            "--background" => o.background = true,
            "--low" => o.low = true,
            "--sh" => {
                o.sh = Some(val()?);
                i += 1;
            }
            "--wait-cap" => {
                o.wait_cap = Some(val()?.parse().ok()?);
                i += 1;
            }
            "--session" => {
                o.session = Some(val()?);
                i += 1;
            }
            "--agent" => {
                o.agent = Some(val()?);
                i += 1;
            }
            "--label" => {
                o.label = Some(val()?);
                i += 1;
            }
            "--" => {
                o.argv = args[i + 1..].to_vec();
                break;
            }
            _ => return None,
        }
        i += 1;
    }
    Some(o)
}

fn parse_hook(args: &[String]) -> i32 {
    let mut agent: Option<String> = None;
    let mut scope = Scope::User;
    let mut dry_run = false;
    let mut action: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--agent" => {
                agent = args.get(i + 1).cloned();
                i += 1;
            }
            "--scope" => {
                match args.get(i + 1).and_then(|s| Scope::parse(s)) {
                    Some(s) => scope = s,
                    None => return usage(),
                }
                i += 1;
            }
            "--dry-run" => dry_run = true,
            a @ ("install" | "uninstall" | "status") if action.is_none() => action = Some(a),
            _ => return usage(),
        }
        i += 1;
    }
    match action {
        // Called by an agent.
        None => match agent.as_deref().and_then(Agent::parse) {
            Some(a) => hookcmd::answer(a),
            None => usage(),
        },
        Some(action) => {
            let agents = match agent.as_deref() {
                None | Some("all") => Agent::ALL.to_vec(),
                Some(a) => match Agent::parse(a) {
                    Some(a) => vec![a],
                    None => return usage(),
                },
            };
            hookcmd::manage(
                action,
                &hookcmd::Target {
                    agents,
                    scope,
                    dry_run,
                },
            )
        }
    }
}

fn id_control(verb: &str, id: &str) -> Option<Control> {
    let id = id.parse().ok()?;
    Some(match verb {
        "pin" => Control::Pin { id, on: true },
        "unpin" => Control::Pin { id, on: false },
        "low" => Control::Low { id, on: true },
        "normal" => Control::Low { id, on: false },
        "top" => Control::Top { id },
        "cancel" => Control::Cancel { id },
        "kill" => Control::Kill { id },
        "freeze" => Control::Freeze { id },
        "resume" => Control::Resume { id },
        _ => return None,
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or_default();
    let code = match args.first().map(String::as_str) {
        Some("run") => parse_run(rest).map_or_else(usage, run::run),
        Some("hook") => parse_hook(rest),
        Some("ls") => ctl::ls(),
        Some("mcp") => mcp::main(),
        Some("hold") => match rest.first().map(String::as_str) {
            Some("on") => ctl::control(Control::Hold { on: true }),
            Some("off") => ctl::control(Control::Hold { on: false }),
            _ => usage(),
        },
        Some("reset-profile") => match rest.first() {
            Some(k) => ctl::control(Control::ResetProfile { key: k.clone() }),
            None => usage(),
        },
        Some("classify") => match rest.first() {
            Some(line) => ctl::explain(line),
            None => usage(),
        },
        Some(verb) if rest.len() == 1 => {
            id_control(verb, &rest[0]).map_or_else(usage, ctl::control)
        }
        Some("--help" | "-h" | "help") => {
            print!("{USAGE}");
            0
        }
        _ => usage(),
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn run_flags_and_argv() {
        let o = parse_run(&v(&[
            "--background",
            "--low",
            "--label",
            "x",
            "--",
            "pnpm",
            "--filter",
            "web",
            "test",
        ]))
        .unwrap();
        assert!(o.background && o.low);
        assert_eq!(o.label.as_deref(), Some("x"));
        assert_eq!(o.argv, v(&["pnpm", "--filter", "web", "test"]));
        let o = parse_run(&v(&[
            "--agent",
            "claude",
            "--session",
            "s",
            "--wait-cap",
            "60",
            "--sh",
            "pnpm test",
        ]))
        .unwrap();
        assert_eq!((o.wait_cap, o.sh.as_deref()), (Some(60), Some("pnpm test")));
    }

    #[test]
    fn bad_run_flags_are_refused() {
        assert!(parse_run(&v(&["--wait-cap", "soon"])).is_none());
        assert!(parse_run(&v(&["--sh"])).is_none());
        assert!(parse_run(&v(&["--frobnicate"])).is_none());
    }

    #[test]
    fn id_controls() {
        assert_eq!(
            id_control("pin", "4"),
            Some(Control::Pin { id: 4, on: true })
        );
        assert_eq!(
            id_control("normal", "4"),
            Some(Control::Low { id: 4, on: false })
        );
        assert_eq!(id_control("pin", "four"), None);
        assert_eq!(id_control("explode", "4"), None);
    }
}
