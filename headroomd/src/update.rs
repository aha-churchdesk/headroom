//! A newer Headroom in the Homebrew tap, for a copy Homebrew installed.
//!
//! The tap's formula, not the latest GitHub release, says what
//! `brew upgrade` would install: the tap picks a release up within the
//! hour, and until then upgrading does nothing.

use std::cmp::Ordering;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::{Value, json};

const FORMULA: &str =
    "https://raw.githubusercontent.com/aha-churchdesk/homebrew-tap/HEAD/Formula/headroom.rb";
/// How often the tap is looked at.
pub const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);

/// Whether `exe` is a copy Homebrew installed: it resolves into the Cellar.
pub fn brew_installed(exe: &Path) -> bool {
    let p = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
    p.to_string_lossy().contains("/Cellar/headroom/")
}

fn parse(v: &str) -> Option<Vec<u64>> {
    if v.is_empty() {
        return None;
    }
    v.split('.').map(|p| p.parse().ok()).collect()
}

/// The release a formula downloads: `…/releases/download/v0.4.13/…`.
pub fn formula_version(formula: &str) -> Option<&str> {
    let v = formula
        .split("/releases/download/v")
        .nth(1)?
        .split('/')
        .next()?;
    parse(v).map(|_| v)
}

/// Whether `latest` is a later version than `current`. A version either
/// side can't read, such as a pre-release, never is.
pub fn newer(latest: &str, current: &str) -> bool {
    let (Some(mut a), Some(mut b)) = (parse(latest), parse(current)) else {
        return false;
    };
    let n = a.len().max(b.len());
    a.resize(n, 0);
    b.resize(n, 0);
    a.cmp(&b) == Ordering::Greater
}

/// The formula as the tap has it now.
fn fetch() -> Result<String, String> {
    let out = Command::new("curl")
        .args(["-fsSL", "--max-time", "15", FORMULA])
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "update check: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("update check: {e}"))
}

/// The tap's version when it is newer than `current`.
pub fn check(current: &str) -> Result<Option<String>, String> {
    let formula = fetch()?;
    let latest = formula_version(&formula).ok_or("update check: no version in the formula")?;
    Ok(newer(latest, current).then(|| latest.to_string()))
}

/// The page's `update`. A running app keeps its old binary until it is
/// started again, which `brew services` does when it manages it.
pub fn json(latest: &str, brew_service: bool) -> Value {
    json!({
        "latest": latest,
        "command": if brew_service {
            "brew upgrade aha-churchdesk/tap/headroom && brew services restart headroom"
        } else {
            "brew upgrade aha-churchdesk/tap/headroom"
        },
        "restart": !brew_service,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_formula_names_the_release_it_downloads() {
        let f = r#"  url "https://github.com/aha-churchdesk/headroom/releases/download/v0.4.13/headroom-0.4.13-aarch64-apple-darwin.tar.gz""#;
        assert_eq!(formula_version(f), Some("0.4.13"));
    }

    #[test]
    fn a_formula_without_a_readable_release_has_no_version() {
        assert_eq!(formula_version(""), None);
        assert_eq!(formula_version("<html>404: Not Found</html>"), None);
        assert_eq!(
            formula_version("/releases/download/vnext/headroom.tar.gz"),
            None
        );
        assert_eq!(formula_version("/releases/download/v/x"), None);
    }

    #[test]
    fn only_a_later_version_is_newer() {
        assert!(newer("0.4.13", "0.4.11"));
        assert!(newer("0.5.0", "0.4.13"));
        assert!(newer("1.0.0", "0.99.99"));
        assert!(!newer("0.4.13", "0.4.13"), "the same version");
        assert!(!newer("0.4.11", "0.4.13"), "an older tap");
        // Numbers, not text: 10 is after 9.
        assert!(newer("0.4.10", "0.4.9"));
        // A missing part is a zero.
        assert!(!newer("1.0", "1.0.0"));
        assert!(newer("1.0.1", "1.0"));
    }

    #[test]
    fn unreadable_versions_are_never_newer() {
        assert!(!newer("0.5.0-rc1", "0.4.13"));
        assert!(!newer("0.5.0", "development"));
        assert!(!newer("", "0.4.13"));
    }

    #[test]
    fn a_copy_in_the_cellar_is_homebrews_even_through_a_link() {
        let root = std::env::temp_dir().join(format!("hr-update-{}", std::process::id()));
        let bin = root.join("Cellar/headroom/0.4.11/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("headroomd"), "").unwrap();
        let link = root.join("headroomd");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(bin.join("headroomd"), &link).unwrap();
        assert!(brew_installed(&bin.join("headroomd")));
        assert!(brew_installed(&link));
        assert!(!brew_installed(Path::new(
            "/Users/me/Projects/headroom/target/release/headroomd"
        )));
        assert!(!brew_installed(Path::new("/nonexistent/headroomd")));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_upgrade_command_restarts_a_brew_service() {
        let s = json("0.4.13", true);
        assert_eq!(
            s["command"],
            "brew upgrade aha-churchdesk/tap/headroom && brew services restart headroom"
        );
        assert_eq!(s["restart"], false);
        let s = json("0.4.13", false);
        assert_eq!(s["command"], "brew upgrade aha-churchdesk/tap/headroom");
        assert_eq!(s["restart"], true, "the user starts it again");
        assert_eq!(s["latest"], "0.4.13");
    }
}
