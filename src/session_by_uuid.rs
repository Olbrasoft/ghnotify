//! Resolve a Claude session UUID to the tmux session hosting it.
//!
//! Two-tier strategy, in order:
//!
//! 1. **Pid index** (preferred). Every running Claude writes
//!    `~/.claude/sessions/<pid>.json` containing its own `pid` and
//!    `sessionId`. We scan that directory, find the entry whose
//!    `sessionId` matches the marker UUID, and walk that pid's parent
//!    chain in `/proc` until we hit a tmux pane's `pane_pid`. That pane
//!    determines the exact tmux session — even when two Claude sessions
//!    share a cwd. This is the only routing path that distinguishes
//!    `claude-cr-pts-2` (PR author) from `claude-cr-pts-7` (a different
//!    session that happens to be open in the same repo cwd).
//!
//! 2. **Cwd basename** (fallback). When the pid index has no entry —
//!    older Claude versions didn't write it, or the file was reaped —
//!    fall back to the historical heuristic: read `cwd` out of the
//!    UUID's JSONL transcript, take its last path component, build
//!    `claude-<basename>`, and let [`sessions::pick_session`] pick the
//!    best prefix match (attached, then newest). This is
//!    *intentionally* ambiguous when two sessions share a cwd, which is
//!    the bug tier 1 was added to fix; we keep it only as a
//!    last-resort safety net.
//!
//! Why not /proc/<pid>/fd/*: Claude re-opens the JSONL for every append
//! rather than holding it open, so scanning file descriptors sees nothing
//! even while the session is actively writing. The JSONL itself is the
//! stable artifact for the cwd-basename fallback, and the
//! `~/.claude/sessions/<pid>.json` index is the stable artifact for the
//! pid path.

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{sessions, tmux};

/// Resolve `uuid` to the tmux session name that authored the PR/issue
/// carrying this marker, if any.
///
/// Returns `Ok(None)` when neither tier finds a live tmux session —
/// the marker's session might be on a different host, or already
/// exited. Returns `Err` only for unexpected I/O or tmux failures;
/// callers are expected to fall back to repo-based routing in that case
/// rather than 500'ing the webhook.
pub fn resolve_tmux_session(uuid: &str) -> Result<Option<String>> {
    if let Some(name) = resolve_via_pid_index(uuid)? {
        return Ok(Some(name));
    }
    resolve_via_cwd_basename(uuid)
}

/// Tier 1 — find the tmux session whose `pane_pid` is an ancestor of
/// the Claude pid that owns this UUID. Returns `Ok(None)` whenever any
/// step misses (no `<pid>.json`, recycled pid, no live tmux pane in
/// that ancestor chain) so the caller can fall through to tier 2.
fn resolve_via_pid_index(uuid: &str) -> Result<Option<String>> {
    let Some(pid) = pid_for_session_uuid(uuid)? else {
        return Ok(None);
    };
    if !proc_is_claude(pid) {
        // Stale `~/.claude/sessions/<pid>.json` from a Claude that
        // exited without cleaning up — or even worse, the kernel
        // recycled the pid and it now belongs to a different program.
        // Don't trust the entry; fall through to the cwd fallback.
        return Ok(None);
    }
    let panes = list_claude_panes()?;
    Ok(tmux_session_for_descendant_pid(pid, &panes))
}

/// Tier 2 — historical behavior. Read `cwd` from the UUID's JSONL,
/// take the basename, ask `pick_session` for the best `claude-<base>*`
/// match. Ambiguous when two sessions share a cwd, but the only thing
/// we can do for older Claude versions or sessions whose pid index
/// entry has been removed.
fn resolve_via_cwd_basename(uuid: &str) -> Result<Option<String>> {
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

/// Scan `~/.claude/sessions/*.json` and return the pid recorded for
/// the entry whose `sessionId` matches `uuid`, if any.
///
/// Each running Claude writes a tiny JSON descriptor here on startup
/// (`{"pid":N,"sessionId":"...","cwd":"...","status":"...","updatedAt":N}`).
/// Stale files from dead Claudes are common; we filter those out at
/// the caller via `proc_is_claude`.
fn pid_for_session_uuid(uuid: &str) -> Result<Option<u32>> {
    let dir_path = sessions_state_dir()?;
    let dir = match fs::read_dir(&dir_path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", dir_path.display())),
    };
    for entry in dir {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(pid) = pid_from_session_json(&text, uuid) {
            return Ok(Some(pid));
        }
    }
    Ok(None)
}

/// Pure parser for one `~/.claude/sessions/<pid>.json` blob. Returns
/// the recorded pid iff `sessionId` matches `uuid`. Split out so the
/// matching rule is unit-testable without filesystem fixtures.
///
/// The pid is read as `u64` from JSON and narrowed via `u32::try_from`
/// rather than `as`: a malformed or hostile file with a value above
/// `u32::MAX` would otherwise truncate to a small valid pid and could
/// misroute a wake to an unrelated process. Out-of-range values yield
/// `None` so the caller falls through to the cwd fallback.
fn pid_from_session_json(text: &str, uuid: &str) -> Option<u32> {
    let v: Value = serde_json::from_str(text).ok()?;
    if v.get("sessionId").and_then(Value::as_str) != Some(uuid) {
        return None;
    }
    v.get("pid")
        .and_then(Value::as_u64)
        .and_then(|p| u32::try_from(p).ok())
}

/// True when `/proc/<pid>/comm` reads exactly `claude`. Used to reject
/// stale `<pid>.json` entries whose owning process has exited (and
/// possibly had its pid recycled by an unrelated program).
fn proc_is_claude(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim() == "claude")
        .unwrap_or(false)
}

/// One `claude-*` tmux pane as reported by `tmux list-panes -a`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneInfo {
    session_name: String,
    /// `#{pane_pid}` — the pid of the first process tmux ran in this
    /// pane (typically the user's shell). Claude ends up as a
    /// descendant of this pid, which is what makes the parent-chain
    /// walk work.
    pane_pid: u32,
}

/// Enumerate every `claude-*` pane in the tmux server. Returns an
/// empty vec in either soft-fail case — tmux binary not on PATH, or
/// tmux server not running — so the resolver falls through to the
/// cwd fallback rather than failing the webhook. Real failures (e.g.
/// a regression in the `-F` format string on older tmux) still
/// propagate as `Err`.
fn list_claude_panes() -> Result<Vec<PaneInfo>> {
    let out = match Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{session_name}\t#{pane_pid}"])
        .output()
    {
        Ok(o) => o,
        // tmux binary not installed → behave the same as no server
        // running. The webhook still works; tier 2 takes over.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).context("failed to spawn tmux list-panes"),
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("no server running") {
            return Ok(Vec::new());
        }
        return Err(anyhow!(
            "tmux list-panes failed ({}): {}",
            out.status,
            stderr.trim()
        ));
    }
    Ok(parse_pane_list(&String::from_utf8_lossy(&out.stdout)))
}

/// Pure parser for `tmux list-panes -a -F "#{session_name}\t#{pane_pid}"`.
/// Skips non-`claude-*` panes and unparseable lines.
fn parse_pane_list(s: &str) -> Vec<PaneInfo> {
    s.lines()
        .filter_map(|line| {
            let mut it = line.split('\t');
            let session_name = it.next()?.trim().to_string();
            if !session_name.starts_with("claude-") {
                return None;
            }
            let pane_pid = it.next()?.trim().parse::<u32>().ok()?;
            Some(PaneInfo {
                session_name,
                pane_pid,
            })
        })
        .collect()
}

/// Walk parents of `pid` up the proc tree; return the session whose
/// `pane_pid` first appears in that chain. Bounded to 16 hops so a
/// pathological /proc state can't loop. Returns `None` when no
/// ancestor matches — that's the signal to fall back to cwd routing.
fn tmux_session_for_descendant_pid(pid: u32, panes: &[PaneInfo]) -> Option<String> {
    let by_pid: HashMap<u32, &str> = panes
        .iter()
        .map(|p| (p.pane_pid, p.session_name.as_str()))
        .collect();

    let mut current = pid;
    for _ in 0..16 {
        if let Some(&name) = by_pid.get(&current) {
            return Some(name.to_string());
        }
        match read_ppid(current) {
            Some(ppid) if ppid > 1 && ppid != current => current = ppid,
            _ => return None,
        }
    }
    None
}

/// Read PPID from `/proc/<pid>/stat`. The `comm` field is parenthesized
/// and may contain spaces or right-parens itself, so we split on the
/// *last* `)` and parse the field after `state` as ppid.
fn read_ppid(pid: u32) -> Option<u32> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = s.rsplit_once(')')?.1;
    let mut fields = after_comm.split_whitespace();
    let _state = fields.next()?;
    fields.next()?.parse::<u32>().ok()
}

/// `$HOME/.claude/sessions`. Split out for symmetry with `projects_dir`.
fn sessions_state_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home directory")?;
    Ok(home.join(".claude").join("sessions"))
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

    // -- pid index parsers ------------------------------------------------

    #[test]
    fn pid_from_session_json_returns_pid_on_match() {
        // Real-world shape verbatim from `~/.claude/sessions/<pid>.json`.
        let raw = r#"{"pid":11209,"sessionId":"98d65447-0cc0-4c98-a612-a9b5c0699023","cwd":"/home/jirka/Olbrasoft/cr","startedAt":1777630142909,"procStart":"35738","version":"2.1.126","peerProtocol":1,"kind":"interactive","entrypoint":"cli","status":"busy","updatedAt":1777666655882}"#;
        assert_eq!(
            pid_from_session_json(raw, "98d65447-0cc0-4c98-a612-a9b5c0699023"),
            Some(11209)
        );
    }

    #[test]
    fn pid_from_session_json_returns_none_on_uuid_mismatch() {
        // Different session in same project dir — must not falsely route.
        let raw = r#"{"pid":544135,"sessionId":"92e9fcb2-5661-4ec9-9bf5-a5e2a68b4a0d","cwd":"/home/jirka/Olbrasoft/cr"}"#;
        assert_eq!(
            pid_from_session_json(raw, "98d65447-0cc0-4c98-a612-a9b5c0699023"),
            None
        );
    }

    #[test]
    fn pid_from_session_json_handles_malformed_json() {
        // Truncated write or future schema change — degrade to None,
        // never panic.
        assert_eq!(pid_from_session_json("not json", "x"), None);
        assert_eq!(pid_from_session_json("{}", "x"), None);
        assert_eq!(
            pid_from_session_json(r#"{"sessionId":"x"}"#, "x"),
            None,
            "missing pid field must yield None, not 0"
        );
    }

    #[test]
    fn pid_from_session_json_rejects_pid_above_u32_max() {
        // A malformed/hostile file with a giant pid must not silently
        // truncate down into the valid pid space — that could route a
        // wake to an unrelated live process. Return None instead so
        // the caller falls through to the cwd fallback.
        let raw = format!(
            r#"{{"pid":{huge},"sessionId":"x"}}"#,
            huge = u64::from(u32::MAX) + 1
        );
        assert_eq!(pid_from_session_json(&raw, "x"), None);
    }

    // -- pane parser ------------------------------------------------------

    #[test]
    fn parse_pane_list_filters_to_claude_sessions() {
        let input = "\
claude-cr-pts-2\t11209
claude-cr-pts-7\t544135
other-session\t99999
";
        let got = parse_pane_list(input);
        assert_eq!(got.len(), 2, "non-claude panes must be filtered out");
        assert_eq!(got[0].session_name, "claude-cr-pts-2");
        assert_eq!(got[0].pane_pid, 11209);
        assert_eq!(got[1].session_name, "claude-cr-pts-7");
        assert_eq!(got[1].pane_pid, 544135);
    }

    #[test]
    fn parse_pane_list_skips_malformed_lines() {
        // Pid not parseable, missing column, blank line — none must abort.
        let input = "\
claude-cr-pts-2\tnot_a_number
claude-only-name
\t12345
claude-good\t777
";
        let got = parse_pane_list(input);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].session_name, "claude-good");
        assert_eq!(got[0].pane_pid, 777);
    }

    // -- ancestor walk ----------------------------------------------------

    fn pane(name: &str, pid: u32) -> PaneInfo {
        PaneInfo {
            session_name: name.to_string(),
            pane_pid: pid,
        }
    }

    #[test]
    fn ancestor_walk_matches_pane_pid_directly() {
        // pid IS the pane_pid (e.g. shell == claude isn't realistic, but
        // exercises the zero-hop case).
        let panes = vec![pane("claude-cr-pts-2", 100)];
        assert_eq!(
            tmux_session_for_descendant_pid_test(100, &panes, &[]),
            Some("claude-cr-pts-2".into())
        );
    }

    #[test]
    fn ancestor_walk_finds_match_through_chain() {
        // claude (200) → bash (150) → tmux pane (100). Walking up from 200
        // must land on 100 → claude-cr-pts-2.
        let panes = vec![pane("claude-cr-pts-2", 100), pane("claude-cr-pts-7", 999)];
        let parents = [(200u32, 150u32), (150u32, 100u32)];
        assert_eq!(
            tmux_session_for_descendant_pid_test(200, &panes, &parents),
            Some("claude-cr-pts-2".into())
        );
    }

    #[test]
    fn ancestor_walk_picks_correct_session_among_siblings() {
        // The bug from the field: two `claude-cr-*` panes in the same cwd.
        // The cwd-basename fallback would pick whichever is newer; the pid
        // walk must pick the one whose ancestor matches.
        let panes = vec![
            pane("claude-cr-pts-2", 11209),
            pane("claude-cr-pts-7", 544135),
        ];
        let parents = [(11500u32, 11209u32)]; // a claude pid under pts-2's pane
        assert_eq!(
            tmux_session_for_descendant_pid_test(11500, &panes, &parents),
            Some("claude-cr-pts-2".into()),
            "must route to pts-2 (the actual ancestor), not pts-7"
        );
    }

    #[test]
    fn ancestor_walk_returns_none_when_chain_escapes_panes() {
        // The pid's ancestor chain goes to init without ever touching a
        // tmux pane — caller should fall through to cwd routing.
        let panes = vec![pane("claude-cr-pts-2", 100)];
        let parents = [(500u32, 400u32), (400u32, 1u32)];
        assert_eq!(
            tmux_session_for_descendant_pid_test(500, &panes, &parents),
            None
        );
    }

    /// Pure-data variant of [`tmux_session_for_descendant_pid`]. Production
    /// reads parents from `/proc/<pid>/stat`; tests inject a parent table
    /// instead. Keeps the algorithm itself in lockstep with prod (same
    /// 16-hop bound, same HashMap lookup, same ppid<=1 termination).
    fn tmux_session_for_descendant_pid_test(
        pid: u32,
        panes: &[PaneInfo],
        parents: &[(u32, u32)],
    ) -> Option<String> {
        let by_pid: HashMap<u32, &str> = panes
            .iter()
            .map(|p| (p.pane_pid, p.session_name.as_str()))
            .collect();
        let parent_of: HashMap<u32, u32> = parents.iter().copied().collect();

        let mut current = pid;
        for _ in 0..16 {
            if let Some(&name) = by_pid.get(&current) {
                return Some(name.to_string());
            }
            match parent_of.get(&current).copied() {
                Some(ppid) if ppid > 1 && ppid != current => current = ppid,
                _ => return None,
            }
        }
        None
    }
}
