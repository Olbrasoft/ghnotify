//! Parse the `<!-- claude-session: UUID -->` marker that Claude embeds at
//! the top of every PR body via `gh pr create`.
//!
//! Why this exists: ghnotify's default routing picks the tmux session by
//! repo name (`claude-<repo>` prefix). That breaks when a Claude session
//! running in one directory creates a PR in a *different* repo — e.g. a
//! session in `/home/jirka/imdb` opening a PR on `Olbrasoft/cr`. The event
//! repo is `cr`, so repo-routing delivers the review wake to any
//! `claude-cr-*` session (usually a wrong, unrelated one) instead of the
//! author. The author marker makes the routing author-scoped: the wake
//! goes to the session that opened the PR, not to whichever session
//! happens to have the event's repo cwd.
//!
//! The marker is a conventional HTML comment so it renders invisibly on
//! GitHub. The UUID is the Claude session id — the filename (without
//! `.jsonl`) of the session's JSONL transcript under
//! `~/.claude/projects/<encoded-cwd>/`.

/// Extract the claude-session UUID from a PR/issue body.
///
/// Looks for the exact form `<!-- claude-session: <uuid> -->`. The UUID is
/// validated by shape (standard 8-4-4-4-12 hex-with-hyphens) so malformed
/// markers — or an unrelated HTML comment that happens to mention the
/// phrase — don't yield a false positive that would misroute the wake.
///
/// Returns `None` if the tag is absent, malformed, or contains a value that
/// doesn't look like a UUID.
pub fn extract_uuid(body: &str) -> Option<String> {
    const TAG: &str = "<!-- claude-session: ";
    const END: &str = " -->";
    let start = body.find(TAG)? + TAG.len();
    let rest = &body[start..];
    let end = rest.find(END)?;
    let candidate = rest[..end].trim();
    if is_uuid(candidate) {
        Some(candidate.to_ascii_lowercase())
    } else {
        None
    }
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

    #[test]
    fn extracts_uuid_from_canonical_marker() {
        let body = format!("<!-- claude-session: {UUID} -->\n\nCloses #520");
        assert_eq!(extract_uuid(&body), Some(UUID.to_string()));
    }

    #[test]
    fn extracts_uuid_when_marker_not_first_line() {
        // Guard rail: the global CLAUDE.md mandates the marker as the first
        // line, but a human could edit the PR body afterwards and push it
        // down. Routing must still work — the marker is the load-bearing
        // signal regardless of line position.
        let body = format!("## Summary\n\nlorem ipsum\n\n<!-- claude-session: {UUID} -->\n");
        assert_eq!(extract_uuid(&body), Some(UUID.to_string()));
    }

    #[test]
    fn returns_none_when_marker_absent() {
        assert_eq!(extract_uuid("## Summary\n\nJust a regular PR body"), None);
    }

    #[test]
    fn returns_none_for_malformed_uuid() {
        // Missing a hex char in the 12-char tail segment.
        let bad = "<!-- claude-session: 74da902e-6000-4680-8b0a-b1bb3db8128 -->";
        assert_eq!(extract_uuid(bad), None);
        // Non-hex char.
        let bad2 = "<!-- claude-session: 74da902e-6000-4680-8b0a-b1bb3db8128Z -->";
        assert_eq!(extract_uuid(bad2), None);
        // Wrong number of segments.
        let bad3 = "<!-- claude-session: 74da902e60004680 -->";
        assert_eq!(extract_uuid(bad3), None);
        // Extra trailing segment.
        let bad4 = "<!-- claude-session: 74da902e-6000-4680-8b0a-b1bb3db8128b-extra -->";
        assert_eq!(extract_uuid(bad4), None);
    }

    #[test]
    fn returns_none_for_missing_closing() {
        // Opens the marker but never closes it — treat as malformed rather
        // than greedy-match to the end of the body.
        let body = format!("<!-- claude-session: {UUID}\n\nbody without terminator");
        assert_eq!(extract_uuid(&body), None);
    }

    #[test]
    fn normalizes_uppercase_hex_to_lowercase() {
        // JSONL filenames on disk are lowercase. A marker typed or pasted
        // in upper/mixed case must resolve to the same canonical id so the
        // downstream /proc lookup doesn't miss the match.
        let body = format!("<!-- claude-session: {} -->", UUID.to_ascii_uppercase());
        assert_eq!(extract_uuid(&body), Some(UUID.to_string()));
    }

    #[test]
    fn trims_inner_whitespace_around_uuid() {
        // Tolerant parse: someone hand-editing the marker might leave an
        // extra space. The is_uuid check itself is strict, so this just
        // normalizes the surrounding whitespace before validation.
        let body = format!("<!-- claude-session:   {UUID}   -->");
        assert_eq!(extract_uuid(&body), Some(UUID.to_string()));
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
        assert_eq!(extract_uuid(&body), Some(UUID.to_string()));
    }

    #[test]
    fn returns_none_when_tag_name_differs() {
        // Similar-but-not-identical prefix must not match — the prefix is
        // the contract with `gh pr create`.
        let body = format!("<!-- claude_session: {UUID} -->");
        assert_eq!(extract_uuid(&body), None);
        let body2 = format!("<!-- session: {UUID} -->");
        assert_eq!(extract_uuid(&body2), None);
    }
}
