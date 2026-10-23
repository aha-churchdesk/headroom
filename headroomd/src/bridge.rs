//! What the popover asks for, turned into what the app does.
//!
//! The page sends small JSON intents (`{"cmd":"pin","id":"4","on":true}`).
//! Queue changes become daemon controls; settings edits become a whole new
//! config, so the daemon and `config.json` never disagree; the rest stays in
//! the app (dismissing a banner, copying, the login item).

use headroom_core::classify::{Class, classify};
use headroom_core::config::{Config, KillMode};
use headroom_core::model::JobId;
use headroom_core::protocol::Control;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum UiCmd {
    Hold { on: bool },
    Freeze { id: String },
    Resume { id: String },
    Kill { id: String },
    Cancel { id: String },
    Top { id: String },
    Pin { id: String, on: bool },
    Low { id: String, on: bool },
    Move { id: String, to: usize },
    Dismiss { id: String },
    Copy { text: String },
    FinishSetup,
    ResetProfile { key: String },
    ClearData,
    Set { key: String, value: Value },
    AddPattern { pattern: String },
    RemovePattern { pattern: String },
    Classify { text: String },
    Quit,
}

#[derive(Debug, PartialEq)]
pub enum Do {
    Control(Control),
    Dismiss(String),
    Copy(String),
    FinishSetup,
    Login(bool),
    Classify(String),
    Quit,
}

fn job(id: &str) -> Result<JobId, String> {
    id.parse().map_err(|_| format!("bad job id {id:?}"))
}

fn configure(cfg: Config) -> Do {
    Do::Control(Control::Configure {
        config: Box::new(cfg),
    })
}

pub fn translate(cmd: UiCmd, cfg: &Config) -> Result<Do, String> {
    Ok(match cmd {
        UiCmd::Hold { on } => Do::Control(Control::Hold { on }),
        UiCmd::Freeze { id } => Do::Control(Control::Freeze { id: job(&id)? }),
        UiCmd::Resume { id } => Do::Control(Control::Resume { id: job(&id)? }),
        UiCmd::Kill { id } => Do::Control(Control::Kill { id: job(&id)? }),
        UiCmd::Cancel { id } => Do::Control(Control::Cancel { id: job(&id)? }),
        UiCmd::Top { id } => Do::Control(Control::Top { id: job(&id)? }),
        UiCmd::Pin { id, on } => Do::Control(Control::Pin { id: job(&id)?, on }),
        UiCmd::Low { id, on } => Do::Control(Control::Low { id: job(&id)?, on }),
        UiCmd::Move { id, to } => Do::Control(Control::Move { id: job(&id)?, to }),
        UiCmd::ResetProfile { key } => Do::Control(Control::ResetProfile { key }),
        UiCmd::ClearData => Do::Control(Control::ClearData),
        UiCmd::Dismiss { id } => Do::Dismiss(id),
        UiCmd::Copy { text } => Do::Copy(text),
        UiCmd::FinishSetup => Do::FinishSetup,
        UiCmd::Classify { text } => Do::Classify(text),
        UiCmd::Quit => Do::Quit,
        UiCmd::AddPattern { pattern } => {
            let p = pattern.trim().to_string();
            if p.is_empty() {
                return Err("empty pattern".into());
            }
            let mut c = cfg.clone();
            if !c.patterns.contains(&p) {
                c.patterns.push(p);
            }
            configure(c)
        }
        UiCmd::RemovePattern { pattern } => {
            let mut c = cfg.clone();
            c.patterns.retain(|p| *p != pattern);
            configure(c)
        }
        UiCmd::Set { key, value } => {
            let mut c = cfg.clone();
            let num = || {
                value
                    .as_u64()
                    .ok_or_else(|| format!("{key} needs a number"))
            };
            match key.as_str() {
                "headroom" => c.headroom = num()?,
                "default_estimate" => c.default_estimate = num()?,
                "wait_cap_s" => c.wait_cap_s = num()?,
                "backfill" => c.backfill = value.as_bool().ok_or("backfill needs true or false")?,
                "kill_mode" => {
                    c.kill_mode = serde_json::from_value::<KillMode>(value.clone())
                        .map_err(|e| e.to_string())?
                }
                "start_at_login" => {
                    return value
                        .as_bool()
                        .map(Do::Login)
                        .ok_or_else(|| "start_at_login needs true or false".into());
                }
                other => return Err(format!("unknown setting {other}")),
            }
            configure(c)
        }
    })
}

/// The Settings page's "would this be gated?" answer.
pub fn classify_json(text: &str, cfg: &Config) -> Value {
    match classify(text, cfg) {
        Class::Heavy { cmd, .. } => json!({ "text": text, "result": "heavy", "key": cmd }),
        Class::Excluded { why } => json!({ "text": text, "result": "excluded", "why": why }),
        Class::Wrapped | Class::Light => json!({ "text": text, "result": "light" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use headroom_core::model::GIB;

    fn t(js: &str) -> Result<Do, String> {
        let cmd: UiCmd = serde_json::from_str(js).map_err(|e| e.to_string())?;
        translate(cmd, &Config::default())
    }

    fn config_of(d: Result<Do, String>) -> Config {
        match d {
            Ok(Do::Control(Control::Configure { config })) => *config,
            other => panic!("not a configure: {other:?}"),
        }
    }

    #[test]
    fn queue_intents_become_controls_with_numeric_ids() {
        assert_eq!(
            translate(UiCmd::ClearData, &Config::default()),
            Ok(Do::Control(Control::ClearData))
        );
        assert_eq!(
            t(r#"{"cmd":"pin","id":"4","on":true}"#),
            Ok(Do::Control(Control::Pin { id: 4, on: true }))
        );
        assert_eq!(
            t(r#"{"cmd":"move","id":"7","to":2}"#),
            Ok(Do::Control(Control::Move { id: 7, to: 2 }))
        );
        assert_eq!(
            t(r#"{"cmd":"hold","on":false}"#),
            Ok(Do::Control(Control::Hold { on: false }))
        );
        assert_eq!(t(r#"{"cmd":"quit"}"#), Ok(Do::Quit));
    }

    #[test]
    fn bad_ids_and_unknown_commands_are_errors_not_panics() {
        assert!(
            t(r#"{"cmd":"kill","id":"j41"}"#)
                .unwrap_err()
                .contains("bad job id")
        );
        assert!(t(r#"{"cmd":"self_destruct"}"#).is_err());
        assert!(t(r#"{"cmd":"pin","id":"4"}"#).is_err(), "missing field");
    }

    #[test]
    fn finishing_setup_is_an_app_action() {
        assert_eq!(t(r#"{"cmd":"finish_setup"}"#), Ok(Do::FinishSetup));
    }

    #[test]
    fn settings_edits_produce_a_whole_new_config() {
        let c = config_of(t(r#"{"cmd":"set","key":"headroom","value":3221225472}"#));
        assert_eq!(c.headroom, 3 * GIB);
        assert_eq!(
            c.patterns,
            Config::default().patterns,
            "everything else kept"
        );
        let c = config_of(t(r#"{"cmd":"set","key":"kill_mode","value":"freeze"}"#));
        assert_eq!(c.kill_mode, KillMode::Freeze);
        assert!(t(r#"{"cmd":"set","key":"tune","value":false}"#).is_err());
        let c = config_of(t(r#"{"cmd":"set","key":"backfill","value":false}"#));
        assert!(!c.backfill);
        assert_eq!(
            t(r#"{"cmd":"set","key":"start_at_login","value":false}"#),
            Ok(Do::Login(false))
        );
    }

    #[test]
    fn bad_settings_are_refused() {
        assert!(t(r#"{"cmd":"set","key":"headroom","value":"lots"}"#).is_err());
        assert!(t(r#"{"cmd":"set","key":"kill_mode","value":"nuke"}"#).is_err());
        assert!(
            t(r#"{"cmd":"set","key":"colour","value":1}"#)
                .unwrap_err()
                .contains("unknown setting")
        );
        assert!(t(r#"{"cmd":"set","key":"start_at_login","value":"yes"}"#).is_err());
    }

    #[test]
    fn patterns_are_added_once_and_removed() {
        let c = config_of(t(r#"{"cmd":"add_pattern","pattern":"  bun test "}"#));
        assert_eq!(c.patterns.last().map(String::as_str), Some("bun test"));
        let c = config_of(t(r#"{"cmd":"add_pattern","pattern":"vitest"}"#));
        assert_eq!(c.patterns, Config::default().patterns, "already there");
        let c = config_of(t(r#"{"cmd":"remove_pattern","pattern":"vitest"}"#));
        assert!(!c.patterns.contains(&"vitest".to_string()));
        assert!(t(r#"{"cmd":"add_pattern","pattern":"   "}"#).is_err());
    }

    #[test]
    fn classify_answers_in_the_pages_terms() {
        let cfg = Config::default();
        assert_eq!(
            classify_json("pnpm vitest run x", &cfg)["key"],
            "vitest run"
        );
        assert_eq!(classify_json("pnpm dev", &cfg)["result"], "excluded");
        assert_eq!(classify_json("ls", &cfg)["result"], "light");
    }
}
