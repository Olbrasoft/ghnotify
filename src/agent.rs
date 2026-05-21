//! Per-agent constants for routing wakes to either Claude Code or Codex.
//!
//! Historically ghnotify was hardcoded to a single agent (Claude Code): tmux
//! session names always started with `claude-`, the PR marker was always
//! `<!-- claude-session: UUID -->`, and the comment-trigger heuristics looked
//! for `@claude` / `/claude`. With Codex now running side-by-side in its own
//! tmux sessions, routing needs to branch on which agent a webhook belongs to.
//!
//! Rather than scatter `if claude { … } else { … }` across the codebase, this
//! module captures the per-agent constants as data and exposes them as
//! `&'static Agent` references. Downstream modules accept `&Agent` and read
//! the prefix / marker / triggers off of it. Adding a third agent later is
//! one new `static` and one entry in [`ALL`].
//!
//! The values here MUST stay in lockstep with the bash wrappers in
//! `~/.bashrc` (which name the tmux sessions) and with the documented PR
//! marker convention in the user's global Claude/Codex instructions.

// This module is the foundation for the dual-agent refactor (parent #23).
// Its items become live as later sub-issues wire them into sessions /
// session_marker / session_by_uuid / event / webhook. Until then the unused
// statics and helpers would emit `dead_code` warnings on every build — silence
// them at the module boundary so this staged refactor doesn't add noise.
#![allow(dead_code)]

/// Which coding agent a session belongs to.
///
/// `Copy` because it's a tiny tag we pass alongside session info; carrying
/// it by value rather than by reference simplifies struct layouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Claude,
    Codex,
}

/// Bundle of per-agent constants used for routing decisions.
///
/// All fields are `&'static` so the per-agent value can itself be a `static`
/// — no allocation, no clones, callers borrow.
#[derive(Debug)]
pub struct Agent {
    pub kind: AgentKind,
    /// Prefix of every tmux session name owned by this agent. Includes the
    /// trailing hyphen so `starts_with(prefix)` never matches a different
    /// agent whose name happens to share an initial substring (e.g. a
    /// hypothetical `claude2-` would not be misclassified as Claude).
    pub tmux_prefix: &'static str,
    /// Tag inside the `<!-- TAG: UUID -->` HTML comment that the agent
    /// embeds at the top of every PR body. Used to recover the session
    /// UUID from a webhook payload.
    pub pr_marker_tag: &'static str,
    /// Substrings in an issue-comment body that explicitly address this
    /// agent. Matching is via `body.contains(trigger)` upstream, so order
    /// here is purely documentary (longest-most-specific first by
    /// convention).
    pub mention_triggers: &'static [&'static str],
}

pub static CLAUDE: Agent = Agent {
    kind: AgentKind::Claude,
    tmux_prefix: "claude-",
    pr_marker_tag: "claude-session",
    mention_triggers: &["@claude-cr", "@claude", "/claude"],
};

pub static CODEX: Agent = Agent {
    kind: AgentKind::Codex,
    tmux_prefix: "codex-",
    pr_marker_tag: "codex-session",
    mention_triggers: &["@codex-cr", "@codex", "/codex"],
};

/// Every agent ghnotify knows about. Iterate this when a routing decision
/// must consider all agents — e.g. parsing a mixed `tmux list-sessions`
/// output, or probing a PR body for any marker.
///
/// Order is the routing-precedence default for ambiguous cases: when a PR
/// body contains markers from multiple agents (which should never happen but
/// could if a body is hand-edited), the earlier entry wins. Claude is first
/// for historical-compatibility reasons — Claude-only setups must behave
/// identically to before this change.
pub static ALL: &[&Agent] = &[&CLAUDE, &CODEX];

/// Identify the agent that owns a tmux session, based on its name prefix.
///
/// Returns `None` for non-agent sessions (anything that doesn't start with
/// one of the known prefixes), so callers can filter `tmux list-sessions`
/// output through this without a separate "is it ours?" check.
pub fn from_tmux_session_name(name: &str) -> Option<&'static Agent> {
    ALL.iter().copied().find(|a| name.starts_with(a.tmux_prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_constants_match_legacy_hardcoded_values() {
        // Pre-refactor, these values were literals scattered across
        // sessions.rs, session_marker.rs, event.rs. This test pins them so
        // any drift between the bash wrapper convention and ghnotify's
        // routing is caught locally instead of by misrouted wakes in prod.
        assert_eq!(CLAUDE.kind, AgentKind::Claude);
        assert_eq!(CLAUDE.tmux_prefix, "claude-");
        assert_eq!(CLAUDE.pr_marker_tag, "claude-session");
        assert!(CLAUDE.mention_triggers.contains(&"@claude"));
        assert!(CLAUDE.mention_triggers.contains(&"@claude-cr"));
        assert!(CLAUDE.mention_triggers.contains(&"/claude"));
    }

    #[test]
    fn codex_constants_match_bash_wrapper_convention() {
        // The codex() wrapper in ~/.bashrc names sessions `codex-<repo>-<tty>`;
        // this asserts ghnotify's view of that prefix doesn't drift from the
        // wrapper. Same for the PR marker convention documented in
        // ~/.codex/AGENTS.md.
        assert_eq!(CODEX.kind, AgentKind::Codex);
        assert_eq!(CODEX.tmux_prefix, "codex-");
        assert_eq!(CODEX.pr_marker_tag, "codex-session");
        assert!(CODEX.mention_triggers.contains(&"@codex"));
        assert!(CODEX.mention_triggers.contains(&"/codex"));
    }

    #[test]
    fn from_tmux_session_name_identifies_claude_sessions() {
        let agent = from_tmux_session_name("claude-cr-pts-2").unwrap();
        assert_eq!(agent.kind, AgentKind::Claude);
    }

    #[test]
    fn from_tmux_session_name_identifies_codex_sessions() {
        let agent = from_tmux_session_name("codex-ghnotify-pts-7").unwrap();
        assert_eq!(agent.kind, AgentKind::Codex);
    }

    #[test]
    fn from_tmux_session_name_returns_none_for_unrelated_sessions() {
        // A name that shares no prefix with any registered agent must NOT
        // match. This is the routing's main false-positive guardrail —
        // anything else in `tmux list-sessions` (a user's `work`, `dotfiles`,
        // etc.) must be ignored by ghnotify.
        assert!(from_tmux_session_name("work").is_none());
        assert!(from_tmux_session_name("dotfiles-pts-1").is_none());
    }

    #[test]
    fn from_tmux_session_name_rejects_prefix_without_trailing_hyphen() {
        // The trailing hyphen in each agent's prefix is load-bearing: it
        // ensures a hypothetical `claudex-foo` session doesn't get
        // misclassified as Claude. The prefix is `"claude-"`, not `"claude"`.
        assert!(from_tmux_session_name("claudex-foo").is_none());
        assert!(from_tmux_session_name("codexy-bar").is_none());
    }

    #[test]
    fn all_lists_both_agents_in_routing_precedence_order() {
        // Claude first preserves backward compatibility for the
        // double-marker edge case documented in [`ALL`].
        assert_eq!(ALL.len(), 2);
        assert_eq!(ALL[0].kind, AgentKind::Claude);
        assert_eq!(ALL[1].kind, AgentKind::Codex);
    }
}
