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
//! `&'static Agent` references via `Agent::claude()`, `Agent::codex()`,
//! `Agent::all()`, and `Agent::from_tmux_session_name(name)`. Downstream
//! modules accept `&Agent` and read the prefix / marker / triggers off of it.
//! Adding a third agent later is one new `static` and one entry in [`ALL`].
//!
//! The values here MUST stay in lockstep with the bash wrappers in
//! `~/.bashrc` (which name the tmux sessions) and with the documented PR
//! marker convention in the user's global Claude/Codex instructions.

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
// The `pub` fields are read by sub-issues #25–#29 (sessions.rs / session_marker.rs
// / event.rs). Until those land, suppress dead-code at field granularity; the
// warnings will surface again automatically for any field added later that
// nothing consumes.
#[derive(Debug)]
#[allow(dead_code)]
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
    /// agent. The current consumer in `event.rs` applies two different
    /// match modes by trigger shape — `@`-mention triggers are matched
    /// with `body.contains(trigger)`, `/`-slash triggers with
    /// `body.lines().any(|l| l.trim_start().starts_with(trigger))`. This
    /// list captures the trigger strings only; sub-issue #28 will migrate
    /// the consumer to drive matching off this list directly and may at
    /// that point introduce an explicit per-trigger match mode (e.g.
    /// `enum TriggerMatch { Contains, LineStartsWith }`). Order here is
    /// purely documentary (longest-most-specific first by convention).
    pub mention_triggers: &'static [&'static str],
}

static CLAUDE: Agent = Agent {
    kind: AgentKind::Claude,
    tmux_prefix: "claude-",
    pr_marker_tag: "claude-session",
    mention_triggers: &["@claude-cr", "@claude", "/claude"],
};

static CODEX: Agent = Agent {
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
static ALL: &[&Agent] = &[&CLAUDE, &CODEX];

// Most of this impl is wired in by sub-issues #25-#29 of parent #23. Until
// then the associated functions look unused. Allow is scoped to this block
// (not the whole module) so dead-code warnings come back automatically for
// anything else we add later but forget to wire up.
#[allow(dead_code)]
impl Agent {
    /// The Claude Code agent.
    pub fn claude() -> &'static Agent {
        &CLAUDE
    }

    /// The Codex agent.
    pub fn codex() -> &'static Agent {
        &CODEX
    }

    /// Every agent ghnotify routes for. See [`ALL`] for ordering semantics.
    pub fn all() -> &'static [&'static Agent] {
        ALL
    }

    /// Identify the agent that owns a tmux session, based on its name prefix.
    ///
    /// Returns `None` for non-agent sessions (anything that doesn't start
    /// with one of the known prefixes), so callers can filter
    /// `tmux list-sessions` output through this without a separate
    /// "is it ours?" check.
    pub fn from_tmux_session_name(name: &str) -> Option<&'static Agent> {
        ALL.iter()
            .copied()
            .find(|a| name.starts_with(a.tmux_prefix))
    }
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
        let claude = Agent::claude();
        assert_eq!(claude.kind, AgentKind::Claude);
        assert_eq!(claude.tmux_prefix, "claude-");
        assert_eq!(claude.pr_marker_tag, "claude-session");
        assert!(claude.mention_triggers.contains(&"@claude"));
        assert!(claude.mention_triggers.contains(&"@claude-cr"));
        assert!(claude.mention_triggers.contains(&"/claude"));
    }

    #[test]
    fn codex_constants_match_bash_wrapper_convention() {
        // The codex() wrapper in ~/.bashrc names sessions `codex-<repo>-<tty>`;
        // this asserts ghnotify's view of that prefix doesn't drift from the
        // wrapper. Same for the PR marker convention documented in
        // ~/.codex/AGENTS.md.
        let codex = Agent::codex();
        assert_eq!(codex.kind, AgentKind::Codex);
        assert_eq!(codex.tmux_prefix, "codex-");
        assert_eq!(codex.pr_marker_tag, "codex-session");
        assert!(codex.mention_triggers.contains(&"@codex"));
        assert!(codex.mention_triggers.contains(&"/codex"));
    }

    #[test]
    fn from_tmux_session_name_identifies_claude_sessions() {
        let agent = Agent::from_tmux_session_name("claude-cr-pts-2").unwrap();
        assert_eq!(agent.kind, AgentKind::Claude);
    }

    #[test]
    fn from_tmux_session_name_identifies_codex_sessions() {
        let agent = Agent::from_tmux_session_name("codex-ghnotify-pts-7").unwrap();
        assert_eq!(agent.kind, AgentKind::Codex);
    }

    #[test]
    fn from_tmux_session_name_returns_none_for_unrelated_sessions() {
        // A name that shares no prefix with any registered agent must NOT
        // match. This is the routing's main false-positive guardrail —
        // anything else in `tmux list-sessions` (a user's `work`, `dotfiles`,
        // etc.) must be ignored by ghnotify.
        assert!(Agent::from_tmux_session_name("work").is_none());
        assert!(Agent::from_tmux_session_name("dotfiles-pts-1").is_none());
    }

    #[test]
    fn from_tmux_session_name_rejects_prefix_without_trailing_hyphen() {
        // The trailing hyphen in each agent's prefix is load-bearing: it
        // ensures a hypothetical `claudex-foo` session doesn't get
        // misclassified as Claude. The prefix is `"claude-"`, not `"claude"`.
        assert!(Agent::from_tmux_session_name("claudex-foo").is_none());
        assert!(Agent::from_tmux_session_name("codexy-bar").is_none());
    }

    #[test]
    fn all_lists_both_agents_in_routing_precedence_order() {
        // Claude first preserves backward compatibility for the
        // double-marker edge case documented in [`ALL`].
        let all = Agent::all();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].kind, AgentKind::Claude);
        assert_eq!(all[1].kind, AgentKind::Codex);
    }
}
