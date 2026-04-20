//! Resolve a Claude session UUID to the tmux session hosting it.
//!
//! Claude stores every session's transcript at
//! `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl` and writes `cwd` into
//! each record. We use the UUID to find the JSONL, read its canonical
//! cwd, and route to the tmux session whose base name matches that cwd's
//! last path segment (the same `claude-<repo>` convention the bash
//! wrapper uses).
//!
//! Why not /proc/<pid>/fd/*: Claude re-opens the JSONL for every append
//! rather than holding it open, so scanning file descriptors sees nothing
//! even while the session is actively writing. The JSONL itself is the
//! stable artifact.
//!
//! Why this is a best-effort resolver: if two Claude sessions share a
//! cwd (two terminals on the same repo), the UUID identifies the JSONL
//! uniquely but the cwd-basename mapping is ambiguous — we land on the
//! same base name for both and then defer to
//! [`sessions::pick_session`]'s attached+newest tiebreak. That's no
//! worse than pre-existing repo-prefix routing for that edge case, and
//! it completely fixes the cross-repo case (where the event repo is
//! `cr` but the author session runs in `imdb`).

use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::{sessions, tmux};

/// Resolve `uuid` to the tmux session name that authored the PR/issue
/// carrying this marker, if any.
///
/// Returns `Ok(None)` when the UUID has no on-disk JSONL (session
/// unknown), the JSONL has no cwd record, or no matching tmux session
/// is live. Returns `Err` only for unexpected I/O or tmux failures;
/// callers are expected to fall back to repo-based routing in that case
/// rather than 500'ing the webhook.
pub fn resolve_tmux_session(uuid: &str) -> Result<Option<String>> {
    let Some(cwd) = cwd_for_uuid(uuid)? else {
        return Ok(None);
    };
    // `file_name` strips trailing slashes and returns the last path
    // component, which is what the bash wrapper uses verbatim (with
    // dots replaced by dashes) to name the tmux session.
    let Some(bare) = cwd.file_name().and_then(|s| s.to_str()) else {
        return Ok(None);
    };
    let base = tmux::session_name_for_repo(bare);
    let sessions = sessions::list_claude_sessions_full()?;
    Ok(sessions::pick_session(&sessions, &base))
}

/// Locate `~/.claude/projects/*/<uuid>.jsonl` and read the session's
/// canonical cwd from the first record that has a `cwd` field.
fn cwd_for_uuid(uuid: &str) -> Result<Option<PathBuf>> {
    let projects = projects_dir()?;
    let filename = format!("{uuid}.jsonl");
    let dir = match fs::read_dir(&projects) {
        Ok(d) => d,
        // No projects dir → no sessions on this box. Not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", projects.display())),
    };
    for entry in dir {
        let Ok(entry) = entry else { continue };
        let path = entry.path().join(&filename);
        if path.is_file() {
            return read_cwd_from_jsonl(&path);
        }
    }
    Ok(None)
}

/// Read the first ~20 records of a JSONL transcript and return the first
/// non-null `cwd`. Bounded to keep pathological files (gigabyte-sized
/// transcripts with the cwd only at the tail) from dragging the webhook
/// handler to a crawl.
fn read_cwd_from_jsonl(path: &Path) -> Result<Option<PathBuf>> {
    const MAX_LINES: usize = 20;
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader = BufReader::new(file);
    for (i, line) in reader.lines().enumerate() {
        if i >= MAX_LINES {
            break;
        }
        let Ok(line) = line else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(cwd) = v.get("cwd").and_then(Value::as_str) {
            if !cwd.is_empty() {
                return Ok(Some(PathBuf::from(cwd)));
            }
        }
    }
    Ok(None)
}

/// `$HOME/.claude/projects`. Split out so tests can stub via
/// `read_cwd_from_jsonl` directly without touching the filesystem
/// resolution.
fn projects_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home directory")?;
    Ok(home.join(".claude").join("projects"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_jsonl(dir: &Path, lines: &[&str]) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join("test.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        path
    }

    #[test]
    fn reads_cwd_from_second_record() {
        // Real-world shape: the first record is `permission-mode` with no
        // cwd, the second `system` record carries it. Must find it.
        let tmp = tempdir();
        let path = write_jsonl(
            &tmp,
            &[
                r#"{"type":"permission-mode","permissionMode":"default","sessionId":"x"}"#,
                r#"{"type":"system","cwd":"/home/jirka/imdb","content":""}"#,
            ],
        );
        assert_eq!(
            read_cwd_from_jsonl(&path).unwrap(),
            Some(PathBuf::from("/home/jirka/imdb"))
        );
    }

    #[test]
    fn returns_none_when_no_cwd_in_first_window() {
        // Only cwd-less records in the bounded read window — don't walk
        // a pathologically long transcript to find one.
        let tmp = tempdir();
        let path = write_jsonl(
            &tmp,
            &[
                r#"{"type":"permission-mode","permissionMode":"default"}"#,
                r#"{"type":"file-history-snapshot","cwd":null}"#,
            ],
        );
        assert_eq!(read_cwd_from_jsonl(&path).unwrap(), None);
    }

    #[test]
    fn skips_unparseable_lines_without_panicking() {
        // A corrupt/truncated line in the middle of the transcript must
        // not abort the scan — keep going, find the cwd in a later
        // record.
        let tmp = tempdir();
        let path = write_jsonl(
            &tmp,
            &[
                r#"{"type":"permission-mode"}"#,
                r#"not valid json at all"#,
                r#"{"type":"system","cwd":"/home/jirka/cr"}"#,
            ],
        );
        assert_eq!(
            read_cwd_from_jsonl(&path).unwrap(),
            Some(PathBuf::from("/home/jirka/cr"))
        );
    }

    #[test]
    fn ignores_empty_cwd() {
        // An empty-string cwd is the same as no cwd — don't propagate
        // "" as a valid result.
        let tmp = tempdir();
        let path = write_jsonl(
            &tmp,
            &[
                r#"{"type":"system","cwd":""}"#,
                r#"{"type":"user","cwd":"/home/jirka/imdb"}"#,
            ],
        );
        assert_eq!(
            read_cwd_from_jsonl(&path).unwrap(),
            Some(PathBuf::from("/home/jirka/imdb"))
        );
    }

    /// Throwaway tempdir helper — avoids pulling in `tempfile` just for
    /// two tests. Uses the PID to avoid collision between parallel test
    /// runs.
    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ghnotify-test-{}-{}",
            std::process::id(),
            TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    static TEST_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
}
