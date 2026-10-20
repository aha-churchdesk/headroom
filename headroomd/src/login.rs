//! Start at login: a LaunchAgent, or Homebrew's when installed that way.
//!
//! A LaunchAgent rather than SMAppService, for the reason hour-tracker
//! gives: the app is ad-hoc signed, and SMAppService ties registration to
//! the signing identity, so it would break on every rebuild. A plist is
//! also inspectable and removable by hand.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::view::Login;

pub const LABEL: &str = "dev.headroom";
const BREW_LABEL: &str = "homebrew.mxcl.headroom";

fn agents(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
}

pub fn state(home: &Path) -> Login {
    if agents(home).join(format!("{BREW_LABEL}.plist")).exists() {
        Login::Brew
    } else if agents(home).join(format!("{LABEL}.plist")).exists() {
        Login::On
    } else {
        Login::Off
    }
}

/// Not kept alive: Quit in the menu should stay quit. Interactive: it
/// draws a menu bar item, so it needs the GUI session.
pub fn plist(exe: &str) -> String {
    let exe = exe.replace('&', "&amp;").replace('<', "&lt;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>            <string>{LABEL}</string>
  <key>ProgramArguments</key> <array><string>{exe}</string></array>
  <key>RunAtLoad</key>        <true/>
  <key>KeepAlive</key>        <false/>
  <key>ProcessType</key>      <string>Interactive</string>
</dict>
</plist>
"#
    )
}

/// Turn our LaunchAgent on or off. A Homebrew service is left to brew.
pub fn set(home: &Path, on: bool, exe: &str) -> io::Result<Login> {
    if state(home) == Login::Brew {
        return Ok(Login::Brew);
    }
    let path = agents(home).join(format!("{LABEL}.plist"));
    if on {
        fs::create_dir_all(agents(home))?;
        fs::write(&path, plist(exe))?;
    } else {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(state(home))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("headroom-login-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn toggles_our_launch_agent() {
        let h = home("toggle");
        assert_eq!(state(&h), Login::Off);
        assert_eq!(
            set(
                &h,
                true,
                "/Applications/Headroom.app/Contents/MacOS/headroomd"
            )
            .unwrap(),
            Login::On
        );
        let p = fs::read_to_string(h.join("Library/LaunchAgents/dev.headroom.plist")).unwrap();
        assert!(p.contains("<string>/Applications/Headroom.app/Contents/MacOS/headroomd</string>"));
        assert!(p.contains("<key>KeepAlive</key>        <false/>"));
        assert_eq!(set(&h, true, "x").unwrap(), Login::On, "twice is fine");
        assert_eq!(set(&h, false, "x").unwrap(), Login::Off);
        assert_eq!(
            set(&h, false, "x").unwrap(),
            Login::Off,
            "off twice is fine"
        );
    }

    #[test]
    fn a_brew_service_is_left_to_brew() {
        let h = home("brew");
        fs::create_dir_all(h.join("Library/LaunchAgents")).unwrap();
        fs::write(
            h.join("Library/LaunchAgents/homebrew.mxcl.headroom.plist"),
            "x",
        )
        .unwrap();
        assert_eq!(state(&h), Login::Brew);
        assert_eq!(set(&h, false, "x").unwrap(), Login::Brew);
        assert!(
            h.join("Library/LaunchAgents/homebrew.mxcl.headroom.plist")
                .exists()
        );
    }

    #[test]
    fn paths_are_escaped_for_xml() {
        assert!(plist("/a & b/<x>").contains("/a &amp; b/&lt;x>"));
    }
}
