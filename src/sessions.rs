//! Discovery of agent tmux sessions and basic doctor diagnostics.
//!
//! Session routing background: Each agent (Claude Code, Codex) runs in tmux
//! sessions whose names are produced by the matching bash wrapper. Historically
//! the wrapper named sessions exactly `claude-<repo>` and ghnotify routed by
//! exact match. Some users run a wrapper variant that appends a per-terminal
//! suffix (`claude-<repo>-<tty>`) so two terminals in the same repo don't
//! collide. The Codex wrapper mirrors the same scheme with the `codex-` prefix.
//! To support both layouts, we route by *prefix*: a webhook for repo `cr`
//! matches either `claude-cr` / `claude-cr-<anything>` (for a Claude target)
//! or `codex-cr` / `codex-cr-<anything>` (for a Codex target). The agent that
//! owns a given session is recovered from the prefix via
//! [`Agent::from_tmux_session_name`].

use anyhow::{anyhow, Context, Result};
use std::process::Command;

use crate::agent::{Agent, AgentKind};
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
    /// Which agent owns this session, recovered from the name's prefix at
    /// parse time. Carried alongside the session so downstream routing can
    /// log "delivered to Codex" without re-doing the prefix lookup, and so
    /// agent-agnostic selection can compare candidates across agents in one
    /// pass.
    pub kind: AgentKind,
}

/// List tmux sessions owned by any known agent (names only).
pub fn list_agent_sessions() -> Result<Vec<String>> {
    Ok(list_agent_sessions_full()?
        .into_iter()
        .map(|s| s.name)
        .collect())
}

/// List agent-owned tmux sessions with attached+created metadata. The result
/// includes both `claude-*` and `codex-*` sessions; per-agent filtering is
/// the caller's responsibility (see [`resolve_session_for_repo`]).
pub fn list_agent_sessions_full() -> Result<Vec<SessionInfo>> {
    let out = Command::new("tmux")
        .args([
            "list-sessions",
            "-F",
            "#{session_name}\t#{session_attached}\t#{session_created}",
        ])
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            // Distinguish "tmux isn't running" (soft-empty, expected) from
            // real failures (malformed -F spec on old tmux, permission
            // issues, …). Without this split, webhook delivery would
            // silently soft-discard every webhook whenever the format
            // string regresses on some host, hiding the routing bug
            // instead of surfacing it.
            let stderr = String::from_utf8_lossy(&o.stderr);
            if stderr.contains("no server running") {
                return Ok(Vec::new());
            }
            return Err(anyhow!(
                "tmux list-sessions failed ({}): {}",
                o.status,
                stderr.trim()
            ));
        }
        Err(e) => return Err(e).context("failed to spawn tmux"),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    Ok(parse_tmux_list(&stdout))
}

/// Parse the tab-separated output of our `tmux list-sessions -F` call.
/// Silently skips unparseable lines and non-agent sessions (anything whose
/// name doesn't start with a registered agent's `tmux_prefix`).
pub fn parse_tmux_list(s: &str) -> Vec<SessionInfo> {
    s.lines()
        .filter_map(|line| {
            let mut it = line.split('\t');
            let name = it.next()?.trim();
            // Recover the owning agent from the name's prefix; non-agent
            // sessions (the user's `work`, `dotfiles`, …) drop out here.
            let agent = Agent::from_tmux_session_name(name)?;
            let attached = it.next()?.trim().parse::<u32>().ok()? > 0;
            let created = it.next()?.trim().parse::<u64>().ok()?;
            Some(SessionInfo {
                name: name.to_string(),
                attached,
                created,
                kind: agent.kind,
            })
        })
        .collect()
}

/// Resolve the tmux session to deliver a prompt for `repo` into, scoped to a
/// specific agent.
///
/// Matches `<prefix><bare-repo>` exactly, or any `<prefix><bare-repo>-<suffix>`
/// session, where `<prefix>` is the agent's `tmux_prefix`. Among candidates,
/// prefers attached sessions, then the most recently created one — that
/// disambiguates old orphan sessions from the live tty-suffixed one, and
/// picks the freshest terminal when the user has multiple open on the same
/// repo.
///
/// Returns `None` when no matching session exists (caller should treat as a
/// soft discard, not an error).
pub fn resolve_session_for_repo(repo: &str, agent: &Agent) -> Result<Option<String>> {
    let base = tmux::session_name_for_repo(repo, agent);
    let sessions = list_agent_sessions_full()?;
    Ok(pick_session(&sessions, &base))
}

/// Agent-agnostic counterpart to [`resolve_session_for_repo`]. Considers
/// candidates from *every* registered agent for the same repo and picks the
/// best one by the standard attached → newest rule.
///
/// This is the routing path for webhooks that arrive **without** a PR marker
/// — there's no signal telling us which agent the wake belongs to, so the
/// recency rule decides. When only one agent has a session for the repo (the
/// common case), the result is identical to calling [`resolve_session_for_repo`]
/// with that agent. When both Claude and Codex have a session for the repo,
/// the more recently attached one wins, which preserves "current focus"
/// semantics that the user actually has on screen.
pub fn resolve_session_for_repo_any(repo: &str) -> Result<Option<String>> {
    let sessions = list_agent_sessions_full()?;
    Ok(pick_session_any(&sessions, repo))
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

/// Pure core of [`resolve_session_for_repo_any`]. Looks at every registered
/// agent's base name for `repo` and applies the standard selection rule
/// across the *union* of matches.
///
/// Implementation note: we deliberately iterate by agent and collect into a
/// single candidate pool rather than running [`pick_session`] per-agent and
/// then comparing the per-agent winners. The latter would break the
/// "attached beats unattached" guarantee in edge cases where one agent has
/// an attached but stale session and the other has an unattached but fresh
/// one — we want one ordered choice across the whole pool, not a tournament.
pub fn pick_session_any(sessions: &[SessionInfo], repo: &str) -> Option<String> {
    let mut candidates: Vec<&SessionInfo> = Vec::new();
    for agent in Agent::all() {
        let base = tmux::session_name_for_repo(repo, agent);
        let prefix = format!("{base}-");
        for s in sessions {
            if s.name == base || s.name.starts_with(&prefix) {
                candidates.push(s);
            }
        }
    }
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

    // codex binary
    let codex = Command::new("codex").arg("--version").output();
    match codex {
        Ok(o) if o.status.success() => {
            check("codex", true, String::from_utf8_lossy(&o.stdout).trim());
        }
        _ => check("codex", false, "not on PATH (optional)"),
    }

    // sessions
    let sessions = list_agent_sessions().unwrap_or_default();
    let detail = if sessions.is_empty() {
        "(none — start a Claude or Codex session in some repo)".to_string()
    } else {
        sessions.join(", ")
    };
    check("agent tmux sessions", !sessions.is_empty(), &detail);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test helper. Builds a Claude-owned SessionInfo by default; the agent
    /// kind is inferred from the name's prefix so test inputs stay terse.
    /// For tests that need to mix Claude and Codex sessions in one input,
    /// prefer using [`s_kind`] explicitly.
    fn s(name: &str, attached: bool, created: u64) -> SessionInfo {
        let kind = Agent::from_tmux_session_name(name)
            .map(|a| a.kind)
            // Tests passing a non-agent name are exercising the "ignore
            // anything not ours" path, so defaulting to Claude is safe —
            // such inputs never make it past the parse filter and never
            // reach the candidate pool.
            .unwrap_or(AgentKind::Claude);
        SessionInfo {
            name: name.to_string(),
            attached,
            created,
            kind,
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
        assert_eq!(got.len(), 3, "non-agent sessions must be filtered");
        assert_eq!(got[0].name, "claude-cr");
        assert_eq!(got[0].kind, AgentKind::Claude);
        assert!(!got[0].attached);
        assert_eq!(got[1].name, "claude-cr-pts-2");
        assert!(got[1].attached);
    }

    #[test]
    fn parse_tmux_list_recognizes_codex_sessions() {
        // Mixed Claude + Codex tmux output. Both must come through with the
        // correct AgentKind; the dual-prefix filter is the single biggest
        // behavioral change in this sub-issue, so guard it explicitly.
        let input = "\
claude-cr-pts-2\t1\t100
codex-ghnotify-pts-7\t1\t200
codex-cr\t0\t150
work\t1\t300
";
        let got = parse_tmux_list(input);
        assert_eq!(got.len(), 3, "only claude-* and codex-* must survive");
        assert_eq!(got[0].kind, AgentKind::Claude);
        assert_eq!(got[1].kind, AgentKind::Codex);
        assert_eq!(got[1].name, "codex-ghnotify-pts-7");
        assert_eq!(got[2].kind, AgentKind::Codex);
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
        let sessions = vec![s("claude-cr", false, 100), s("claude-cr-pts-2", true, 200)];
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
        let sessions = vec![s("claude-cr", false, 100), s("claude-cr-pts-2", false, 200)];
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
        let base = crate::tmux::session_name_for_repo("GitHub.Issues", Agent::claude());
        assert_eq!(
            pick_session(&sessions, &base),
            Some("claude-GitHub-Issues-pts-9".into())
        );
    }

    #[test]
    fn pick_prefers_newer_attached_session_among_shared_prefix_matches() {
        // Prefix routing intentionally treats both the exact base session and
        // any `base-...` suffixed variant as matches. That means a session
        // like `claude-cr-web-pts-3` is also a candidate for base `claude-cr`
        // because it starts with `claude-cr-`.
        //
        // This test documents that accepted ambiguity: once multiple sessions
        // match by prefix, the normal selection rule still applies and the
        // newer attached session wins. Olbrasoft repo names don't collide
        // this way in practice (webhook repo field carries the actual repo
        // name) — if that ever changes, this test fixes the current behavior
        // so any rule change is intentional.
        let sessions = vec![
            s("claude-cr-web-pts-3", true, 100),
            s("claude-cr-pts-2", true, 200),
        ];
        assert_eq!(
            pick_session(&sessions, "claude-cr"),
            Some("claude-cr-pts-2".into())
        );
    }

    #[test]
    fn pick_session_any_falls_through_to_codex_when_only_codex_exists() {
        // No Claude session for "cr"; only Codex. Agent-agnostic resolution
        // must still find it.
        let sessions = vec![s("codex-cr-pts-7", true, 100)];
        assert_eq!(
            pick_session_any(&sessions, "cr"),
            Some("codex-cr-pts-7".into())
        );
    }

    #[test]
    fn pick_session_any_picks_newer_attached_across_agents() {
        // Both agents have an attached session for the same repo. The
        // recency tiebreak (newest `created`) decides — here Codex's
        // session is fresher, so it wins.
        let sessions = vec![
            s("claude-cr-pts-2", true, 100),
            s("codex-cr-pts-7", true, 200),
        ];
        assert_eq!(
            pick_session_any(&sessions, "cr"),
            Some("codex-cr-pts-7".into())
        );
    }

    #[test]
    fn pick_session_any_prefers_attached_across_agents() {
        // Cross-agent variant of the "attached beats orphan" rule. Here
        // Codex is detached but newer, Claude is attached but older —
        // attached wins, so Claude is the answer. Documents that recency
        // is the *secondary* key, not primary.
        let sessions = vec![
            s("claude-cr-pts-2", true, 100),
            s("codex-cr-pts-7", false, 999),
        ];
        assert_eq!(
            pick_session_any(&sessions, "cr"),
            Some("claude-cr-pts-2".into())
        );
    }

    #[test]
    fn pick_session_any_returns_none_when_no_agent_owns_the_repo() {
        let sessions = vec![
            s("claude-other-pts-1", true, 100),
            s("codex-different-pts-2", true, 200),
        ];
        assert_eq!(pick_session_any(&sessions, "cr"), None);
    }
}
