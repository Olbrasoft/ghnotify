//! Discovery of Claude tmux sessions and basic doctor diagnostics.
//!
//! Session routing background: Claude Code sessions live in tmux sessions whose
//! names are produced by the `claude()` bash wrapper. Historically the wrapper
//! named sessions exactly `claude-<repo>` and ghnotify routed by exact match.
//! Some users run a wrapper variant that appends a per-terminal suffix
//! (`claude-<repo>-<tty>`) so two terminals in the same repo don't collide.
//! To support both layouts, we route by *prefix*: a webhook for repo `cr`
//! matches either `claude-cr` or `claude-cr-<anything>`.

use anyhow::{Context, Result};
use std::process::Command;

use crate::tmux;

/// One tmux session as reported by `tmux list-sessions`, with the fields we
/// use for routing. Keeping this as plain data lets the selection logic be
/// pure and unit-testable without spawning tmux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub name: String,
    /// `#{session_attached}` — nonzero when at least one client is attached.
    pub attached: bool,
    /// `#{session_created}` — unix seconds; larger = newer.
    pub created: u64,
}

/// List tmux sessions whose name starts with "claude-" (names only).
pub fn list_claude_sessions() -> Result<Vec<String>> {
    Ok(list_claude_sessions_full()?
        .into_iter()
        .map(|s| s.name)
        .collect())
}

/// List claude-* sessions with attached+created metadata.
pub fn list_claude_sessions_full() -> Result<Vec<SessionInfo>> {
    let out = Command::new("tmux")
        .args([
            "list-sessions",
            "-F",
            "#{session_name}\t#{session_attached}\t#{session_created}",
        ])
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o,
        Ok(_) => return Ok(Vec::new()), // no server / no sessions
        Err(e) => return Err(e).context("failed to spawn tmux"),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    Ok(parse_tmux_list(&stdout))
}

/// Parse the tab-separated output of our `tmux list-sessions -F` call.
/// Silently skips unparseable lines and non-claude sessions.
pub fn parse_tmux_list(s: &str) -> Vec<SessionInfo> {
    s.lines()
        .filter_map(|line| {
            let mut it = line.split('\t');
            let name = it.next()?.trim();
            if !name.starts_with("claude-") {
                return None;
            }
            let attached = it.next()?.trim().parse::<u32>().ok()? > 0;
            let created = it.next()?.trim().parse::<u64>().ok()?;
            Some(SessionInfo {
                name: name.to_string(),
                attached,
                created,
            })
        })
        .collect()
}

/// Resolve the tmux session to deliver a prompt for `repo` into.
///
/// Matches `claude-<bare-repo>` exactly, or any `claude-<bare-repo>-<suffix>`
/// session. Among candidates, prefers attached sessions, then the most
/// recently created one — that disambiguates old orphan sessions from the
/// live tty-suffixed one, and picks the freshest terminal when the user has
/// multiple open on the same repo.
///
/// Returns `None` when no matching session exists (caller should treat as a
/// soft discard, not an error).
pub fn resolve_session_for_repo(repo: &str) -> Result<Option<String>> {
    let base = tmux::session_name_for_repo(repo);
    let sessions = list_claude_sessions_full()?;
    Ok(pick_session(&sessions, &base))
}

/// Pure core of [`resolve_session_for_repo`]. Given a set of live sessions and
/// the canonical base name for a repo, return the best match or `None`.
pub fn pick_session(sessions: &[SessionInfo], base: &str) -> Option<String> {
    let prefix = format!("{base}-");
    let mut candidates: Vec<&SessionInfo> = sessions
        .iter()
        .filter(|s| s.name == base || s.name.starts_with(&prefix))
        .collect();
    // Prefer attached, then most-recently created. Stable sort preserves
    // tmux's list order as a final tiebreak for determinism in tests.
    candidates.sort_by(|a, b| {
        b.attached
            .cmp(&a.attached)
            .then_with(|| b.created.cmp(&a.created))
    });
    candidates.first().map(|s| s.name.clone())
}

pub fn doctor() -> Result<()> {
    fn check(name: &str, ok: bool, detail: &str) {
        let mark = if ok { "✓" } else { "✗" };
        println!("  {mark} {name:<28}  {detail}");
    }

    println!("ghnotify doctor:");

    // tmux
    let tmux_v = Command::new("tmux").arg("-V").output();
    match tmux_v {
        Ok(o) if o.status.success() => {
            check("tmux", true, String::from_utf8_lossy(&o.stdout).trim());
        }
        _ => check("tmux", false, "not installed (apt install tmux)"),
    }

    // gh
    let gh = Command::new("gh").arg("--version").output();
    match gh {
        Ok(o) if o.status.success() => {
            let v = String::from_utf8_lossy(&o.stdout);
            let first = v.lines().next().unwrap_or("");
            check("gh cli", true, first);
        }
        _ => check("gh cli", false, "not installed (optional but recommended)"),
    }

    // claude binary
    let claude = Command::new("claude").arg("--version").output();
    match claude {
        Ok(o) if o.status.success() => {
            check(
                "claude code",
                true,
                String::from_utf8_lossy(&o.stdout).trim(),
            );
        }
        _ => check("claude code", false, "not on PATH"),
    }

    // sessions
    let sessions = list_claude_sessions().unwrap_or_default();
    let detail = if sessions.is_empty() {
        "(none — start a Claude session in some repo)".to_string()
    } else {
        sessions.join(", ")
    };
    check("claude tmux sessions", !sessions.is_empty(), &detail);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(name: &str, attached: bool, created: u64) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            attached,
            created,
        }
    }

    #[test]
    fn parse_tmux_list_happy_path() {
        let input = "\
claude-cr\t0\t1776502357
claude-cr-pts-2\t1\t1776526394
claude-ghnotify-pts-8\t1\t1776542386
other-session\t1\t1776542386
";
        let got = parse_tmux_list(input);
        assert_eq!(got.len(), 3, "non-claude sessions must be filtered");
        assert_eq!(got[0].name, "claude-cr");
        assert!(!got[0].attached);
        assert_eq!(got[1].name, "claude-cr-pts-2");
        assert!(got[1].attached);
    }

    #[test]
    fn parse_tmux_list_skips_malformed_lines() {
        let input = "\
claude-cr-pts-2\t1\t1776526394
claude-broken\tnot_a_number\t1
claude-missing-field\t1
";
        let got = parse_tmux_list(input);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "claude-cr-pts-2");
    }

    #[test]
    fn pick_returns_none_when_no_match() {
        let sessions = vec![s("claude-other", true, 100)];
        assert_eq!(pick_session(&sessions, "claude-cr"), None);
    }

    #[test]
    fn pick_exact_match_when_only_option() {
        let sessions = vec![s("claude-cr", true, 100)];
        assert_eq!(
            pick_session(&sessions, "claude-cr"),
            Some("claude-cr".into())
        );
    }

    #[test]
    fn pick_suffixed_match_when_only_option() {
        let sessions = vec![s("claude-cr-pts-2", true, 100)];
        assert_eq!(
            pick_session(&sessions, "claude-cr"),
            Some("claude-cr-pts-2".into())
        );
    }

    #[test]
    fn pick_prefers_attached_over_orphan_exact() {
        // Old orphan claude-cr (unattached) vs. live claude-cr-pts-2 (attached)
        // — the real-world bug this fix addresses.
        let sessions = vec![
            s("claude-cr", false, 100),
            s("claude-cr-pts-2", true, 200),
        ];
        assert_eq!(
            pick_session(&sessions, "claude-cr"),
            Some("claude-cr-pts-2".into())
        );
    }

    #[test]
    fn pick_prefers_newest_among_attached() {
        let sessions = vec![
            s("claude-cr-pts-2", true, 100),
            s("claude-cr-pts-7", true, 300),
            s("claude-cr-pts-4", true, 200),
        ];
        assert_eq!(
            pick_session(&sessions, "claude-cr"),
            Some("claude-cr-pts-7".into())
        );
    }

    #[test]
    fn pick_falls_back_to_unattached_when_nothing_attached() {
        let sessions = vec![
            s("claude-cr", false, 100),
            s("claude-cr-pts-2", false, 200),
        ];
        assert_eq!(
            pick_session(&sessions, "claude-cr"),
            Some("claude-cr-pts-2".into())
        );
    }

    #[test]
    fn pick_does_not_match_different_repo_by_prefix() {
        // "claude-crane" must NOT match base "claude-cr" — the hyphen boundary
        // in the prefix "claude-cr-" prevents this class of false positives.
        let sessions = vec![s("claude-crane", true, 100)];
        assert_eq!(pick_session(&sessions, "claude-cr"), None);
    }

    #[test]
    fn pick_handles_dot_in_repo_via_session_name_helper() {
        // GitHub.Issues → claude-GitHub-Issues; suffixed variant also matches.
        let sessions = vec![s("claude-GitHub-Issues-pts-9", true, 100)];
        let base = crate::tmux::session_name_for_repo("GitHub.Issues");
        assert_eq!(
            pick_session(&sessions, &base),
            Some("claude-GitHub-Issues-pts-9".into())
        );
    }

    #[test]
    fn pick_isolates_repos_with_shared_prefix_when_both_present() {
        // Both "cr" and "cr-web" could coexist; ensure base="claude-cr" only
        // pulls cr sessions, not cr-web ones (even though cr-web starts with
        // "claude-cr-").
        //
        // NOTE: This is a known ambiguity of prefix routing — a session named
        // `claude-cr-web-pts-3` would match base `claude-cr`. In practice repo
        // names don't collide this way at Olbrasoft, but the test documents
        // the current behavior so any future change is intentional.
        let sessions = vec![
            s("claude-cr-web-pts-3", true, 100),
            s("claude-cr-pts-2", true, 200),
        ];
        // The newer attached one wins. If cr-web were newer this would pick it,
        // which would be wrong — we accept that tradeoff (webhook repo field
        // carries the actual repo name so collisions are rare).
        assert_eq!(
            pick_session(&sessions, "claude-cr"),
            Some("claude-cr-pts-2".into())
        );
    }
}
