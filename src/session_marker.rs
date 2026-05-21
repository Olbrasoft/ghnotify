//! Parse the `<!-- <agent>-session: UUID -->` marker that Claude (and Codex,
//! once its bash-wrapper PR convention lands per sub-issue #30) embeds at the
//! top of every PR body via `gh pr create`.
//!
//! Why this exists: ghnotify's default routing picks the tmux session by
//! repo name (`<prefix><repo>` for the relevant agent). That breaks when a
//! session running in one directory creates a PR in a *different* repo —
//! e.g. a session in `/home/jirka/imdb` opening a PR on `Olbrasoft/cr`. The
//! event repo is `cr`, so repo-routing delivers the review wake to any
//! `<prefix>-cr-*` session (usually a wrong, unrelated one) instead of the
//! author. The author marker makes the routing author-scoped: the wake
//! goes to the session that opened the PR, not to whichever session
//! happens to have the event's repo cwd.
//!
//! The marker is a conventional HTML comment so it renders invisibly on
//! GitHub. The UUID is the agent's session id — for Claude, the filename
//! (without `.jsonl`) of the session's JSONL transcript under
//! `~/.claude/projects/<encoded-cwd>/`; for Codex, the `id` inside the
//! `session_meta` payload of `~/.codex/sessions/.../rollout-*.jsonl`.
//!
//! Two agents → two marker tags (`claude-session`, `codex-session`).
//! [`extract_uuid`] is the tag-parametrized primitive; [`extract_marker`]
//! is the dispatch wrapper that probes every known agent and returns the
//! UUID together with the agent it belongs to.

use crate::agent::{Agent, AgentKind};

/// Extract a session UUID from a PR/issue body, scoped to a specific
/// marker `tag` (`"claude-session"` or `"codex-session"`).
///
/// Looks for the exact form `<!-- <tag>: <uuid> -->`. The UUID is
/// validated by shape (standard 8-4-4-4-12 hex-with-hyphens) so malformed
/// markers — or an unrelated HTML comment that happens to mention the
/// phrase — don't yield a false positive that would misroute the wake.
///
/// Returns `None` if the tag is absent, malformed, or contains a value that
/// doesn't look like a UUID. Callers that don't already know which agent
/// they're looking for should prefer [`extract_marker`] instead.
pub fn extract_uuid(body: &str, tag: &str) -> Option<String> {
    let open = format!("<!-- {tag}: ");
    const END: &str = " -->";
    let start = body.find(&open)? + open.len();
    let rest = &body[start..];
    let end = rest.find(END)?;
    let candidate = rest[..end].trim();
    if is_uuid(candidate) {
        Some(candidate.to_ascii_lowercase())
    } else {
        None
    }
}

/// Probe a PR/issue body for *any* known agent's session marker and return
/// the UUID together with the agent it identifies.
///
/// Dispatch order follows [`Agent::all`] (Claude first, then Codex). When
/// a body contains markers for multiple agents — should never happen in
/// practice but possible via hand-edit — the earliest agent in `Agent::all`
/// wins, which preserves backward compatibility with Claude-only
/// deployments.
///
/// Returns `None` when no registered agent has a marker in this body —
/// caller should fall back to repo-name routing.
// Wired into webhook.rs by sub-issue #29 (`uuid_from_payload` and
// `uuid_from_check_suite`). Until then, marker-extraction call sites still
// use the Claude-only `extract_uuid(body, "claude-session")` shim so this
// fn is unused on the binary path. The dead-code warning returns
// automatically once #29 migrates the callers.
#[allow(dead_code)]
pub fn extract_marker(body: &str) -> Option<(String, AgentKind)> {
    Agent::all()
        .iter()
        .find_map(|agent| extract_uuid(body, agent.pr_marker_tag).map(|uuid| (uuid, agent.kind)))
}

/// UUID shape check: `XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX`, hex only.
/// We don't verify the version/variant nibbles — Claude session ids are
/// RFC-4122 v4 in practice, but a stricter check buys nothing for routing
/// and would reject anything a future version tweak produces.
fn is_uuid(s: &str) -> bool {
    let lens = [8usize, 4, 4, 4, 12];
    let mut parts = s.split('-');
    for &expected in &lens {
        match parts.next() {
            Some(part) if part.len() == expected && part.bytes().all(|b| b.is_ascii_hexdigit()) => {
            }
            _ => return false,
        }
    }
    // Reject trailing segments like "...-abc" that would otherwise pass
    // the fixed-length prefix loop.
    parts.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "74da902e-6000-4680-8b0a-b1bb3db8128b";
    const CODEX_UUID: &str = "019e452d-3bbd-7453-ae8f-5587f7749fb0";

    #[test]
    fn extracts_uuid_from_canonical_claude_marker() {
        let body = format!("<!-- claude-session: {UUID} -->\n\nCloses #520");
        assert_eq!(
            extract_uuid(&body, "claude-session"),
            Some(UUID.to_string())
        );
    }

    #[test]
    fn extracts_uuid_from_canonical_codex_marker() {
        // Mirror of the Claude canonical test. Codex's PR-marker convention
        // is added in sub-issue #30 (~/.codex/AGENTS.md); ghnotify's
        // marker-extraction side has to recognize it from this sub-issue
        // forward so a Codex PR body parses correctly even before the
        // external convention lands.
        let body = format!("<!-- codex-session: {CODEX_UUID} -->\n\nCloses #520");
        assert_eq!(
            extract_uuid(&body, "codex-session"),
            Some(CODEX_UUID.to_string())
        );
    }

    #[test]
    fn extract_uuid_is_tag_scoped_and_ignores_the_other_agents_marker() {
        // A Claude marker must not match when the caller asked for a
        // Codex tag, and vice versa. Otherwise the marker would silently
        // get attributed to the wrong agent and routed to the wrong
        // session.
        let claude_body = format!("<!-- claude-session: {UUID} -->");
        assert_eq!(extract_uuid(&claude_body, "codex-session"), None);
        let codex_body = format!("<!-- codex-session: {CODEX_UUID} -->");
        assert_eq!(extract_uuid(&codex_body, "claude-session"), None);
    }

    #[test]
    fn extracts_uuid_when_marker_not_first_line() {
        // Guard rail: the global CLAUDE.md mandates the marker as the first
        // line, but a human could edit the PR body afterwards and push it
        // down. Routing must still work — the marker is the load-bearing
        // signal regardless of line position.
        let body = format!("## Summary\n\nlorem ipsum\n\n<!-- claude-session: {UUID} -->\n");
        assert_eq!(
            extract_uuid(&body, "claude-session"),
            Some(UUID.to_string())
        );
    }

    #[test]
    fn returns_none_when_marker_absent() {
        assert_eq!(
            extract_uuid("## Summary\n\nJust a regular PR body", "claude-session"),
            None
        );
        assert_eq!(
            extract_uuid("## Summary\n\nJust a regular PR body", "codex-session"),
            None
        );
    }

    #[test]
    fn returns_none_for_malformed_uuid() {
        // Missing a hex char in the 12-char tail segment.
        let bad = "<!-- claude-session: 74da902e-6000-4680-8b0a-b1bb3db8128 -->";
        assert_eq!(extract_uuid(bad, "claude-session"), None);
        // Non-hex char.
        let bad2 = "<!-- claude-session: 74da902e-6000-4680-8b0a-b1bb3db8128Z -->";
        assert_eq!(extract_uuid(bad2, "claude-session"), None);
        // Wrong number of segments.
        let bad3 = "<!-- claude-session: 74da902e60004680 -->";
        assert_eq!(extract_uuid(bad3, "claude-session"), None);
        // Extra trailing segment.
        let bad4 = "<!-- claude-session: 74da902e-6000-4680-8b0a-b1bb3db8128b-extra -->";
        assert_eq!(extract_uuid(bad4, "claude-session"), None);
    }

    #[test]
    fn returns_none_for_malformed_codex_uuid() {
        // Same shape validation must apply to the Codex tag — a malformed
        // value after a codex-session tag must NOT yield a false positive
        // that would route the wake to a wrong session.
        let bad = "<!-- codex-session: 019e452d-3bbd-7453-ae8f-5587f7749fb -->";
        assert_eq!(extract_uuid(bad, "codex-session"), None);
        let bad2 = "<!-- codex-session: not-a-uuid -->";
        assert_eq!(extract_uuid(bad2, "codex-session"), None);
    }

    #[test]
    fn returns_none_for_missing_closing() {
        // Opens the marker but never closes it — treat as malformed rather
        // than greedy-match to the end of the body.
        let body = format!("<!-- claude-session: {UUID}\n\nbody without terminator");
        assert_eq!(extract_uuid(&body, "claude-session"), None);
    }

    #[test]
    fn normalizes_uppercase_hex_to_lowercase() {
        // JSONL filenames on disk are lowercase. A marker typed or pasted
        // in upper/mixed case must resolve to the same canonical id so the
        // downstream /proc lookup doesn't miss the match.
        let body = format!("<!-- claude-session: {} -->", UUID.to_ascii_uppercase());
        assert_eq!(
            extract_uuid(&body, "claude-session"),
            Some(UUID.to_string())
        );
        // Same normalization for the Codex tag.
        let codex_body = format!(
            "<!-- codex-session: {} -->",
            CODEX_UUID.to_ascii_uppercase()
        );
        assert_eq!(
            extract_uuid(&codex_body, "codex-session"),
            Some(CODEX_UUID.to_string())
        );
    }

    #[test]
    fn trims_inner_whitespace_around_uuid() {
        // Tolerant parse: someone hand-editing the marker might leave an
        // extra space. The is_uuid check itself is strict, so this just
        // normalizes the surrounding whitespace before validation.
        let body = format!("<!-- claude-session:   {UUID}   -->");
        assert_eq!(
            extract_uuid(&body, "claude-session"),
            Some(UUID.to_string())
        );
    }

    #[test]
    fn only_first_marker_is_extracted_when_body_has_two() {
        // A PR body amended by a second session (unlikely, but possible)
        // could in principle contain two markers. Routing is a single-
        // destination operation, so take the first — matches normal reader
        // expectations and is deterministic.
        let body = format!(
            "<!-- claude-session: {UUID} -->\n<!-- claude-session: 11111111-2222-3333-4444-555555555555 -->",
        );
        assert_eq!(
            extract_uuid(&body, "claude-session"),
            Some(UUID.to_string())
        );
    }

    #[test]
    fn returns_none_when_tag_name_differs() {
        // Similar-but-not-identical prefix must not match — the prefix is
        // the contract with `gh pr create`.
        let body = format!("<!-- claude_session: {UUID} -->");
        assert_eq!(extract_uuid(&body, "claude-session"), None);
        let body2 = format!("<!-- session: {UUID} -->");
        assert_eq!(extract_uuid(&body2, "claude-session"), None);
    }

    #[test]
    fn extract_marker_finds_claude_uuid_with_kind() {
        let body = format!("<!-- claude-session: {UUID} -->\n");
        assert_eq!(
            extract_marker(&body),
            Some((UUID.to_string(), AgentKind::Claude))
        );
    }

    #[test]
    fn extract_marker_finds_codex_uuid_with_kind() {
        let body = format!("<!-- codex-session: {CODEX_UUID} -->\n");
        assert_eq!(
            extract_marker(&body),
            Some((CODEX_UUID.to_string(), AgentKind::Codex))
        );
    }

    #[test]
    fn extract_marker_prefers_claude_when_body_contains_both() {
        // Backward-compat guard for the rare/unlikely double-marker case:
        // pre-Codex behavior was to honor the Claude marker, and that
        // remains the deterministic choice now that Agent::all() lists
        // Claude first.
        let body =
            format!("<!-- claude-session: {UUID} -->\n<!-- codex-session: {CODEX_UUID} -->\n");
        assert_eq!(
            extract_marker(&body),
            Some((UUID.to_string(), AgentKind::Claude))
        );
    }

    #[test]
    fn extract_marker_returns_none_when_no_agent_marker_present() {
        assert_eq!(
            extract_marker("## Summary\n\nJust a regular PR body without a marker"),
            None
        );
    }

    #[test]
    fn extract_marker_skips_malformed_uuid_under_a_known_tag() {
        // If the body has a Claude tag with a bogus UUID and a Codex tag
        // with a valid UUID, the dispatch must skip the malformed one and
        // return the Codex match — not silently fail because the
        // higher-priority agent's marker happened to be malformed.
        let body =
            format!("<!-- claude-session: not-a-uuid -->\n<!-- codex-session: {CODEX_UUID} -->\n");
        assert_eq!(
            extract_marker(&body),
            Some((CODEX_UUID.to_string(), AgentKind::Codex))
        );
    }
}
