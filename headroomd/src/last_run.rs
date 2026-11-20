//! When a coding agent last worked in a folder, from the agents' own records.
//!
//! Claude Code keeps one folder of session transcripts per working directory
//! under `~/.claude/projects`, named after the directory's path. Codex keeps
//! a thread table, with each thread's working directory and last update, in
//! `~/.codex/state_<n>.sqlite`. Both are the agents' private formats, so
//! anything missing or unreadable reads as "not known", never as an error.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::UNIX_EPOCH;

/// What the agents' records said at one moment.
#[derive(Debug, Default)]
pub struct LastRuns {
    /// Claude Code: project folder name to the unix seconds of its newest
    /// transcript.
    claude: HashMap<String, u64>,
    /// Codex: working directory to the unix seconds of its newest thread.
    codex: Vec<(String, u64)>,
}

impl LastRuns {
    /// Read both agents' records under `home`.
    pub fn read(home: &Path) -> Self {
        Self {
            claude: claude_runs(&home.join(".claude/projects")),
            codex: newest_state_db(&home.join(".codex"))
                .map(|db| parse_codex(&query_codex(&db)))
                .unwrap_or_default(),
        }
    }

    /// When Claude Code last wrote a transcript for a session started in
    /// `path`. Sessions started in a subfolder have a folder of their own
    /// whose name can't be told apart from a sibling's, so they don't count.
    pub fn claude(&self, path: &str) -> Option<u64> {
        self.claude.get(&claude_folder(path)).copied()
    }

    /// When a Codex thread working in `path`, or anywhere below it, was
    /// last updated.
    pub fn codex(&self, path: &str) -> Option<u64> {
        let below = format!("{}/", path.trim_end_matches('/'));
        self.codex
            .iter()
            .filter(|(cwd, _)| cwd == path || cwd.starts_with(&below))
            .map(|&(_, at)| at)
            .max()
    }
}

/// The folder Claude Code keeps a working directory's sessions in: the path
/// with every character other than a letter, digit or `-` replaced by `-`.
pub fn claude_folder(path: &str) -> String {
    path.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Per project folder, the modification time of its newest transcript.
fn claude_runs(projects: &Path) -> HashMap<String, u64> {
    let Ok(folders) = std::fs::read_dir(projects) else {
        return HashMap::new();
    };
    folders
        .flatten()
        .filter_map(|folder| {
            let newest = std::fs::read_dir(folder.path())
                .ok()?
                .flatten()
                .filter(|f| f.path().extension().is_some_and(|e| e == "jsonl"))
                .filter_map(|f| f.metadata().ok()?.modified().ok())
                .filter_map(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .max()?;
            Some((folder.file_name().to_string_lossy().into_owned(), newest))
        })
        .collect()
}

/// Codex numbers its state database by schema version: the highest is the
/// one in use.
fn newest_state_db(codex: &Path) -> Option<PathBuf> {
    std::fs::read_dir(codex)
        .ok()?
        .flatten()
        .filter_map(|f| {
            let name = f.file_name().to_string_lossy().into_owned();
            let n: u32 = name
                .strip_prefix("state_")?
                .strip_suffix(".sqlite")?
                .parse()
                .ok()?;
            Some((n, f.path()))
        })
        .max_by_key(|(n, _)| *n)
        .map(|(_, p)| p)
}

/// Each working directory's newest thread, as `cwd<TAB>unix seconds` lines.
/// Through macOS's own sqlite3, read-only: no database library to carry for
/// one query, and Codex keeps writing while we read.
fn query_codex(db: &Path) -> String {
    Command::new("/usr/bin/sqlite3")
        .args(["-readonly", "-separator", "\t"])
        .arg(db)
        .arg("SELECT cwd, MAX(updated_at) FROM threads GROUP BY cwd")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// `query_codex`'s lines. A line that isn't a folder and a time is skipped.
fn parse_codex(out: &str) -> Vec<(String, u64)> {
    out.lines()
        .filter_map(|line| {
            let (cwd, at) = line.rsplit_once('\t')?;
            Some((cwd.to_string(), at.trim().parse().ok()?))
        })
        .filter(|(cwd, _)| cwd.starts_with('/'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hr-last-run-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn touch(path: &Path, secs: u64) {
        fs::write(path, "{}\n").unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    #[test]
    fn claude_names_a_folder_after_its_path() {
        assert_eq!(
            claude_folder("/Users/me/worktrees/repo/sc-1_fix.x"),
            "-Users-me-worktrees-repo-sc-1-fix-x"
        );
        assert_eq!(
            claude_folder("/private/tmp/claude-501/-Users-me-x"),
            "-private-tmp-claude-501--Users-me-x"
        );
        assert_eq!(claude_folder(""), "");
    }

    #[test]
    fn claude_last_ran_when_its_newest_transcript_was_written() {
        let home = dir("claude");
        let wt = "/Users/me/worktrees/repo/sc-1";
        let folder = home.join(".claude/projects").join(claude_folder(wt));
        fs::create_dir_all(folder.join("subagents")).unwrap();
        touch(&folder.join("a.jsonl"), 1_000);
        touch(&folder.join("b.jsonl"), 2_000);
        touch(&folder.join("notes.txt"), 9_000);
        touch(&folder.join("subagents/c.jsonl"), 9_000);
        let runs = LastRuns::read(&home);
        assert_eq!(runs.claude(wt), Some(2_000), "only its own transcripts");
        assert_eq!(runs.claude("/Users/me/worktrees/repo/sc-2"), None);
        assert_eq!(runs.claude("/Users/me/worktrees/repo"), None, "a parent");
    }

    #[test]
    fn an_empty_claude_folder_is_no_run() {
        let home = dir("claude-empty");
        let wt = "/w/a";
        fs::create_dir_all(home.join(".claude/projects").join(claude_folder(wt))).unwrap();
        assert_eq!(LastRuns::read(&home).claude(wt), None);
    }

    #[test]
    fn codex_counts_threads_in_the_folder_and_below_it() {
        let runs = LastRuns {
            codex: parse_codex(
                "/w/a\t100\n/w/a/apps/web\t300\n/w/ab\t900\n/w\t800\nnot a path\t5\n/w/b\tsoon\n",
            ),
            ..LastRuns::default()
        };
        assert_eq!(runs.codex("/w/a"), Some(300));
        assert_eq!(runs.codex("/w/a/"), Some(300), "a trailing slash");
        assert_eq!(runs.codex("/w/b"), None, "an unreadable time");
        assert_eq!(runs.codex("/w/c"), None);
        assert!(parse_codex("").is_empty());
    }

    #[test]
    fn codex_reads_the_newest_state_database() {
        let home = dir("codex");
        let codex = home.join(".codex");
        fs::create_dir_all(&codex).unwrap();
        for name in [
            "state_4.sqlite",
            "state_12.sqlite",
            "state_x.sqlite",
            "logs_2.sqlite",
        ] {
            fs::write(codex.join(name), "").unwrap();
        }
        assert_eq!(newest_state_db(&codex), Some(codex.join("state_12.sqlite")));
        assert_eq!(newest_state_db(&home.join("nowhere")), None);
    }

    #[test]
    fn codex_threads_are_read_from_a_real_database() {
        let home = dir("codex-db");
        let codex = home.join(".codex");
        fs::create_dir_all(&codex).unwrap();
        let db = codex.join("state_5.sqlite");
        let made = Command::new("/usr/bin/sqlite3")
            .arg(&db)
            .arg(
                "CREATE TABLE threads (id TEXT, cwd TEXT, updated_at INTEGER);
                 INSERT INTO threads VALUES ('1', '/w/a', 100), ('2', '/w/a', 250), ('3', '/w/b', 50);",
            )
            .status()
            .unwrap();
        assert!(made.success());
        let runs = LastRuns::read(&home);
        assert_eq!(
            (runs.codex("/w/a"), runs.codex("/w/b")),
            (Some(250), Some(50))
        );
    }

    #[test]
    fn a_codex_database_of_another_shape_reads_as_unknown() {
        let home = dir("codex-odd");
        let codex = home.join(".codex");
        fs::create_dir_all(&codex).unwrap();
        fs::write(codex.join("state_5.sqlite"), "not a database").unwrap();
        assert_eq!(LastRuns::read(&home).codex("/w/a"), None);
    }

    #[test]
    fn nothing_installed_reads_as_unknown() {
        let runs = LastRuns::read(&dir("none"));
        assert_eq!((runs.claude("/w/a"), runs.codex("/w/a")), (None, None));
    }
}
