//! Resolve an agent session UUID to the tmux session hosting it.
//!
//! Two-tier strategy, applied independently per agent (Claude or Codex)
//! since the on-disk layout differs:
//!
//! 1. **Pid index** (preferred). Every running agent writes a tiny JSON
//!    descriptor with its own `pid` and `sessionId`:
//!    - Claude: `~/.claude/sessions/<pid>.json`
//!    - Codex:  `~/.codex/sessions/pids/<pid>.json` (per sub-issue #30)
//!
//!    We scan the appropriate directory, find the entry whose `sessionId`
//!    matches the marker UUID, and walk that pid's parent chain in `/proc`
//!    until we hit a tmux pane's `pane_pid`. That pane determines the
//!    exact tmux session — even when two sessions share a cwd. This is
//!    the only routing path that distinguishes e.g. `claude-cr-pts-2`
//!    (PR author) from `claude-cr-pts-7` (a different session that
//!    happens to be open in the same repo cwd).
//!
//! 2. **Cwd basename** (fallback). When the pid index has no entry —
//!    older agent versions didn't write it, or the file was reaped —
//!    fall back to the historical heuristic: read the agent's `cwd` out
//!    of the UUID's session transcript, take its last path component,
//!    build `<agent-prefix><basename>`, and let [`sessions::pick_session`]
//!    pick the best prefix match (attached, then newest). The transcript
//!    layouts differ per agent:
//!    - Claude: `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl`, `cwd` is a
//!      top-level field on early records.
//!    - Codex:  `~/.codex/sessions/YYYY/MM/DD/rollout-*-<uuid>.jsonl`, `cwd`
//!      sits under `session_meta.payload.cwd` on the first record.
//!
//!    This tier is *intentionally* ambiguous when two sessions share a cwd,
//!    which is the bug tier 1 was added to fix; we keep it only as a
//!    last-resort safety net.
//!
//! Why not /proc/<pid>/fd/*: agents re-open their transcript for every
//! append rather than holding it open, so scanning file descriptors sees
//! nothing even while the session is actively writing. The transcript
//! itself is the stable artifact for tier 2, and the per-agent pid-index
//! JSON is the stable artifact for tier 1.

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::agent::AgentKind;
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

/// Strict variant: tier-1 (pid index) only, never falls back to the
/// cwd-basename heuristic. Returns `Ok(None)` whenever the pid index
/// can't deliver a definitive answer — no `<pid>.json` for this UUID,
/// stale pid, or no live tmux pane in the ancestor chain.
///
/// Use this when the caller has hard knowledge of which session
/// should receive the wake (e.g. `ghnotify send --commit <SHA>`
/// where the marker UUID came from the PR body of *this specific
/// commit's* PR). In that context, a tier-2 fallback would route to
/// "any session in the same cwd" which may be a completely
/// unrelated session — silently misrouting instead of cleanly
/// missing.
pub fn resolve_tmux_session_strict(uuid: &str) -> Result<Option<String>> {
    resolve_via_pid_index(uuid)
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
    let panes = list_panes_with_prefix(crate::agent::Agent::claude().tmux_prefix)?;
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
    // This resolver path reads JSONL from `~/.claude/projects/`, so the
    // target session is always Claude. The Codex-parallel resolver lives
    // in [`resolve_codex_via_cwd_basename`].
    let base = tmux::session_name_for_repo(bare, crate::agent::Agent::claude());
    let sessions = sessions::list_agent_sessions_full()?;
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

/// One agent-owned tmux pane as reported by `tmux list-panes -a`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneInfo {
    session_name: String,
    /// `#{pane_pid}` — the pid of the first process tmux ran in this
    /// pane (typically the user's shell). The agent (Claude or Codex)
    /// ends up as a descendant of this pid, which is what makes the
    /// parent-chain walk work.
    pane_pid: u32,
}

/// Enumerate every tmux pane whose session name starts with `prefix`.
/// Returns an empty vec in either soft-fail case — tmux binary not on
/// PATH, or tmux server not running — so the resolver falls through to
/// the cwd fallback rather than failing the webhook. Real failures
/// (e.g. a regression in the `-F` format string on older tmux) still
/// propagate as `Err`.
fn list_panes_with_prefix(prefix: &str) -> Result<Vec<PaneInfo>> {
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
    Ok(parse_pane_list(
        &String::from_utf8_lossy(&out.stdout),
        prefix,
    ))
}

/// Pure parser for `tmux list-panes -a -F "#{session_name}\t#{pane_pid}"`.
/// Skips panes whose session name doesn't start with `prefix` and any
/// lines that don't parse.
fn parse_pane_list(s: &str, prefix: &str) -> Vec<PaneInfo> {
    s.lines()
        .filter_map(|line| {
            let mut it = line.split('\t');
            let session_name = it.next()?.trim().to_string();
            if !session_name.starts_with(prefix) {
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

// ---- Codex resolvers ------------------------------------------------------
//
// Parallel to the Claude resolvers above, but reads the different on-disk
// layout: `~/.codex/sessions/pids/<pid>.json` for tier 1 (written by the
// `codex()` bash wrapper per sub-issue #30; this resolver gracefully
// returns None until that lands), and
// `~/.codex/sessions/YYYY/MM/DD/rollout-*-<uuid>.jsonl` for tier 2 (always
// present once a Codex session has produced its first rollout).

/// Codex counterpart of [`resolve_via_pid_index`]. Returns `Ok(None)` when
/// the pid index directory doesn't exist yet (the common case until
/// sub-issue #30 lands the bash-wrapper writer), or when the entry is
/// missing / stale, so the caller falls through to tier 2.
fn resolve_codex_via_pid_index(uuid: &str) -> Result<Option<String>> {
    let Some(pid) = codex_pid_for_session_uuid(uuid)? else {
        return Ok(None);
    };
    if !proc_is_codex(pid) {
        return Ok(None);
    }
    let panes = list_panes_with_prefix(crate::agent::Agent::codex().tmux_prefix)?;
    Ok(tmux_session_for_descendant_pid(pid, &panes))
}

/// Codex counterpart of [`resolve_via_cwd_basename`]. Reads `cwd` from the
/// rollout JSONL's `session_meta` payload and picks the best
/// `codex-<basename>` match.
fn resolve_codex_via_cwd_basename(uuid: &str) -> Result<Option<String>> {
    let Some(cwd) = cwd_for_codex_uuid(uuid)? else {
        return Ok(None);
    };
    let Some(bare) = cwd.file_name().and_then(|s| s.to_str()) else {
        return Ok(None);
    };
    let base = tmux::session_name_for_repo(bare, crate::agent::Agent::codex());
    let sessions = sessions::list_agent_sessions_full()?;
    Ok(sessions::pick_session(&sessions, &base))
}

/// Scan `~/.codex/sessions/pids/*.json` and return the pid recorded for
/// the entry whose `sessionId` matches `uuid`, if any. Same on-disk shape
/// as Claude's `<pid>.json`, so the existing pure parser
/// [`pid_from_session_json`] is reused verbatim.
fn codex_pid_for_session_uuid(uuid: &str) -> Result<Option<u32>> {
    let dir_path = codex_pid_state_dir()?;
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

/// True when `/proc/<pid>/comm` reads exactly `codex`.
fn proc_is_codex(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim() == "codex")
        .unwrap_or(false)
}

/// Locate the Codex rollout JSONL whose filename ends with `-<uuid>.jsonl`
/// inside `~/.codex/sessions/YYYY/MM/DD/`, and read the session's `cwd`
/// from its `session_meta` payload. Returns `Ok(None)` when the file
/// doesn't exist on this host (session was on a different machine, or
/// the rollout dir was rotated away).
fn cwd_for_codex_uuid(uuid: &str) -> Result<Option<PathBuf>> {
    let Some(path) = find_codex_rollout_for_uuid(uuid)? else {
        return Ok(None);
    };
    read_cwd_from_codex_jsonl(&path)
}

/// Recursive (year/month/day = 3-deep) scan of `~/.codex/sessions` for a
/// file whose name ends with `-<uuid>.jsonl`. The Codex rollout filename
/// embeds both the start timestamp and the UUID, so a UUID alone doesn't
/// tell us the date — we have to walk. The directory tree is shallow and
/// pruned by the per-day partitioning, so this is acceptably fast in
/// practice (low thousands of files per year of history). Bounded to 3
/// levels of nesting to keep an unexpected layout change (or a symlink
/// loop) from chewing CPU.
fn find_codex_rollout_for_uuid(uuid: &str) -> Result<Option<PathBuf>> {
    let root = codex_sessions_dir()?;
    let needle = format!("-{uuid}.jsonl");
    walk_for_suffix(&root, &needle, 3)
}

/// Generic bounded-depth filesystem walker. Skips entries that error out
/// (e.g. permission denied on a stray file) rather than aborting the
/// whole scan — the goal is best-effort resolution, not strict
/// validation of the directory tree.
fn walk_for_suffix(root: &Path, suffix: &str, max_depth: usize) -> Result<Option<PathBuf>> {
    let dir = match fs::read_dir(root) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", root.display())),
    };
    for entry in dir {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let Ok(ftype) = entry.file_type() else {
            continue;
        };
        if ftype.is_file() {
            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                if name.ends_with(suffix) {
                    return Ok(Some(path));
                }
            }
        } else if ftype.is_dir() && max_depth > 0 {
            if let Some(hit) = walk_for_suffix(&path, suffix, max_depth - 1)? {
                return Ok(Some(hit));
            }
        }
    }
    Ok(None)
}

/// Read the first record of a Codex rollout JSONL and extract
/// `payload.cwd` from the `session_meta` event.
///
/// **Strict on schema, loud on corruption.** The Codex rollout schema
/// puts the `session_meta` record as the very first line, every time.
/// We allow up to 5 lines of look-ahead to tolerate a future format
/// where leading metadata gets reordered, but the rules are:
///
/// - The first non-empty line MUST parse as JSON. If it doesn't, the
///   file is corrupt and we return `Err` — silently dropping a malformed
///   rollout would misroute the wake into the cwd-basename fallback or
///   an unrelated session.
/// - Subsequent lines can fail to parse without error (file truncation
///   in the tail is normal; we're only after the head).
/// - When a `session_meta` record is found, its `payload.cwd` MUST be
///   a non-empty string. A present-but-malformed record is a schema
///   break worth surfacing.
/// - If no `session_meta` shows up in the first MAX_LINES, return
///   `Ok(None)`: the file existed but didn't carry the cwd we needed.
///   The caller treats this as a soft miss (tier-2 falls through to
///   the agent-agnostic repo route).
fn read_cwd_from_codex_jsonl(path: &Path) -> Result<Option<PathBuf>> {
    const MAX_LINES: usize = 5;
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut first_content_line = true;
    for (i, line) in reader.lines().enumerate() {
        if i >= MAX_LINES {
            break;
        }
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        let v = match serde_json::from_str::<Value>(&line) {
            Ok(v) => v,
            Err(e) if first_content_line => {
                return Err(e).with_context(|| {
                    format!(
                        "codex rollout JSONL has unparseable first line: {}",
                        path.display()
                    )
                });
            }
            Err(_) => continue,
        };
        first_content_line = false;
        // Codex schema: top-level `type == "session_meta"`, the cwd lives
        // under `payload.cwd`. The check on `type` keeps us from
        // accidentally matching a future event variant that reuses the
        // word `cwd` somewhere else in its payload.
        if v.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let cwd = v.pointer("/payload/cwd").and_then(Value::as_str);
        match cwd {
            Some(s) if !s.is_empty() => return Ok(Some(PathBuf::from(s))),
            _ => {
                return Err(anyhow!(
                    "codex rollout JSONL has session_meta with missing/empty payload.cwd: {}",
                    path.display()
                ));
            }
        }
    }
    Ok(None)
}

/// `$HOME/.codex/sessions/pids`. The pid-index location is documented by
/// sub-issue #30; the resolver here exists in advance so the writer side
/// can drop files in without further code changes on this end.
fn codex_pid_state_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home directory")?;
    Ok(home.join(".codex").join("sessions").join("pids"))
}

/// `$HOME/.codex/sessions`. Root of the YYYY/MM/DD rollout tree we walk
/// to recover a UUID's `cwd`. Split out for the same reason as
/// [`projects_dir`] — keeps tests injectable at the parser layer.
fn codex_sessions_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home directory")?;
    Ok(home.join(".codex").join("sessions"))
}

// ---- Public agent-aware dispatch -----------------------------------------

/// Resolve a session UUID for the given agent. Two-tier strategy
/// (pid index → cwd-basename fallback), choosing the per-agent paths
/// internally. This is the API new callers should use; the existing
/// Claude-only [`resolve_tmux_session`] / [`resolve_tmux_session_strict`]
/// remain as thin shims and will be migrated to call this in sub-issue
/// #29.
#[allow(dead_code)]
pub fn resolve_tmux_session_for_marker(uuid: &str, kind: AgentKind) -> Result<Option<String>> {
    match kind {
        AgentKind::Claude => resolve_tmux_session(uuid),
        AgentKind::Codex => {
            if let Some(name) = resolve_codex_via_pid_index(uuid)? {
                return Ok(Some(name));
            }
            resolve_codex_via_cwd_basename(uuid)
        }
    }
}

/// Strict variant of [`resolve_tmux_session_for_marker`] — tier 1 only,
/// never the cwd fallback. Use this when the caller has hard knowledge
/// of which session should receive the wake (e.g. `ghnotify send
/// --commit <SHA>`); tier 2 would be silent-misroute territory in that
/// context.
#[allow(dead_code)]
pub fn resolve_tmux_session_for_marker_strict(
    uuid: &str,
    kind: AgentKind,
) -> Result<Option<String>> {
    match kind {
        AgentKind::Claude => resolve_tmux_session_strict(uuid),
        AgentKind::Codex => resolve_codex_via_pid_index(uuid),
    }
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

    // -- codex JSONL parser ----------------------------------------------

    #[test]
    fn codex_jsonl_reads_payload_cwd_from_session_meta() {
        // Real-world shape: first record is `session_meta`, `cwd` sits
        // under `payload.cwd`. This is the load-bearing test for the
        // Codex tier-2 path.
        let tmp = tempdir();
        let path = write_jsonl(
            &tmp,
            &[
                r#"{"timestamp":"2026-05-20T11:37:42.417Z","type":"session_meta","payload":{"id":"019e452d-3bbd-7453-ae8f-5587f7749fb0","cwd":"/home/jirka/Olbrasoft/ghnotify"}}"#,
                r#"{"timestamp":"2026-05-20T11:37:50.000Z","type":"user_input"}"#,
            ],
        );
        assert_eq!(
            read_cwd_from_codex_jsonl(&path).unwrap(),
            Some(PathBuf::from("/home/jirka/Olbrasoft/ghnotify"))
        );
    }

    #[test]
    fn codex_jsonl_ignores_non_session_meta_events_with_cwd_field() {
        // Future Codex versions might emit other event types that happen
        // to carry a `payload.cwd` (e.g. a shell command record). Those
        // are NOT the session's canonical cwd; the resolver must look
        // only at `session_meta`. Without the type check we'd silently
        // mis-route to whatever the most recent command's working
        // directory was.
        let tmp = tempdir();
        let path = write_jsonl(
            &tmp,
            &[
                r#"{"type":"shell_call","payload":{"cwd":"/tmp/some-subdir"}}"#,
                r#"{"type":"session_meta","payload":{"id":"x","cwd":"/home/jirka/cr"}}"#,
            ],
        );
        assert_eq!(
            read_cwd_from_codex_jsonl(&path).unwrap(),
            Some(PathBuf::from("/home/jirka/cr"))
        );
    }

    #[test]
    fn codex_jsonl_empty_payload_cwd_is_an_error() {
        // session_meta is the contract: its cwd field is supposed to be a
        // non-empty path. An empty-string cwd would otherwise be returned
        // as `Some(PathBuf::from(""))` and the downstream basename routine
        // would fall over silently. Treat it as a schema break — loud
        // failure beats silent misroute.
        let tmp = tempdir();
        let path = write_jsonl(&tmp, &[r#"{"type":"session_meta","payload":{"cwd":""}}"#]);
        let err = read_cwd_from_codex_jsonl(&path).unwrap_err();
        assert!(
            err.to_string().contains("missing/empty payload.cwd"),
            "expected schema-break error, got: {err}"
        );
    }

    #[test]
    fn codex_jsonl_session_meta_without_payload_cwd_is_an_error() {
        // Same loud-failure path: a session_meta record that omits cwd
        // entirely must surface, not silently fall through.
        let tmp = tempdir();
        let path = write_jsonl(&tmp, &[r#"{"type":"session_meta","payload":{"id":"x"}}"#]);
        let err = read_cwd_from_codex_jsonl(&path).unwrap_err();
        assert!(
            err.to_string().contains("missing/empty payload.cwd"),
            "expected schema-break error, got: {err}"
        );
    }

    #[test]
    fn codex_jsonl_unparseable_first_line_is_an_error() {
        // A corrupted rollout (truncated write, encoding garble, whatever)
        // must fail loudly. Silently treating it as "no session_meta found"
        // would push the resolver into the cwd-basename fallback against
        // a file that *should* have answered authoritatively.
        let tmp = tempdir();
        let path = write_jsonl(&tmp, &["not valid json at all"]);
        let err = read_cwd_from_codex_jsonl(&path).unwrap_err();
        assert!(
            err.to_string().contains("unparseable first line"),
            "expected unparseable-line error, got: {err}"
        );
    }

    #[test]
    fn codex_jsonl_unparseable_tail_line_is_tolerated() {
        // File corruption in the tail (after a valid session_meta) is
        // routine — Codex appends without fsync between events. The
        // session_meta we care about is the FIRST record, so a broken
        // line later doesn't change what we can answer.
        let tmp = tempdir();
        let path = write_jsonl(
            &tmp,
            &[
                r#"{"type":"session_meta","payload":{"cwd":"/home/jirka/cr"}}"#,
                "garbage line",
                r#"{"type":"user_input"}"#,
            ],
        );
        assert_eq!(
            read_cwd_from_codex_jsonl(&path).unwrap(),
            Some(PathBuf::from("/home/jirka/cr"))
        );
    }

    #[test]
    fn codex_jsonl_returns_none_when_no_session_meta_in_window() {
        // The file is valid JSON throughout but doesn't carry a
        // session_meta in the look-ahead window. That's a "soft miss" —
        // tier-2 falls through gracefully, no error. Important: this is
        // different from a corrupted first line (which is an error)
        // because here every line was well-formed; the rollout schema
        // just didn't yield the answer we wanted.
        let tmp = tempdir();
        let path = write_jsonl(
            &tmp,
            &[
                r#"{"type":"user_input"}"#,
                r#"{"type":"shell_call","payload":{}}"#,
            ],
        );
        assert_eq!(read_cwd_from_codex_jsonl(&path).unwrap(), None);
    }

    // -- rollout walker --------------------------------------------------

    #[test]
    fn walk_for_suffix_finds_file_nested_three_deep() {
        // Models the actual `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`
        // layout. The walker must traverse three directory levels and
        // match by filename suffix.
        let root = tempdir();
        let nested = root.join("2026").join("05").join("20");
        fs::create_dir_all(&nested).unwrap();
        let target = nested.join("rollout-2026-05-20T13-37-32-abc-uuid.jsonl");
        fs::write(&target, "").unwrap();
        // Decoy files at various depths that should NOT match.
        fs::write(root.join("README"), "").unwrap();
        fs::write(nested.join("rollout-2026-05-20T01-other.jsonl"), "").unwrap();

        let found = walk_for_suffix(&root, "-abc-uuid.jsonl", 3).unwrap();
        assert_eq!(found, Some(target));
    }

    #[test]
    fn walk_for_suffix_respects_max_depth() {
        // A file deeper than the depth budget must not be found. Without
        // this guard, an unexpected layout change or a symlink loop
        // could chew CPU walking arbitrarily deep.
        let root = tempdir();
        let deep = root.join("a").join("b").join("c").join("d").join("e");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("hit-uuid.jsonl"), "").unwrap();

        // max_depth = 3 means we traverse root → a → b → c, but not
        // deeper. The hit lives 5 levels down.
        assert_eq!(walk_for_suffix(&root, "-uuid.jsonl", 3).unwrap(), None);
    }

    #[test]
    fn walk_for_suffix_returns_none_when_root_missing() {
        // Tier-1 callers always probe even when the directory tree
        // doesn't exist yet (e.g. before sub-issue #30's bash wrapper
        // has written any pid files). Missing root must not error.
        let missing = std::env::temp_dir().join(format!(
            "ghnotify-test-missing-{}-{}",
            std::process::id(),
            TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        assert_eq!(walk_for_suffix(&missing, ".jsonl", 3).unwrap(), None);
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
codex-cr-pts-3\t11500
other-session\t99999
";
        let got = parse_pane_list(input, "claude-");
        assert_eq!(
            got.len(),
            2,
            "non-claude panes (including codex-*) must be filtered out"
        );
        assert_eq!(got[0].session_name, "claude-cr-pts-2");
        assert_eq!(got[0].pane_pid, 11209);
        assert_eq!(got[1].session_name, "claude-cr-pts-7");
        assert_eq!(got[1].pane_pid, 544135);
    }

    #[test]
    fn parse_pane_list_filters_to_codex_sessions() {
        // Codex-prefix variant of the filter test. Same parser, different
        // prefix — both agents are first-class.
        let input = "\
claude-cr-pts-2\t11209
codex-ghnotify-pts-7\t544135
codex-cr-pts-3\t11500
other-session\t99999
";
        let got = parse_pane_list(input, "codex-");
        assert_eq!(got.len(), 2, "only codex-* panes must survive");
        assert_eq!(got[0].session_name, "codex-ghnotify-pts-7");
        assert_eq!(got[0].pane_pid, 544135);
        assert_eq!(got[1].session_name, "codex-cr-pts-3");
        assert_eq!(got[1].pane_pid, 11500);
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
        let got = parse_pane_list(input, "claude-");
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
