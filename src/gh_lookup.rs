//! Shared `gh api` helpers for resolving a PR body (and thus the
//! `<!-- claude-session: UUID -->` marker) from various event-side
//! identifiers — a PR number, a commit SHA, etc.
//!
//! Used by:
//! * `webhook` — resolves UUID from incoming `check_suite` payloads
//!   that don't carry the PR body inline.
//! * `main::Send` — resolves UUID from a `--commit <SHA>` arg on the
//!   `ghnotify send` CLI, so deploy-side wake invocations can route
//!   to the PR author session even when the user's tmux session name
//!   doesn't match the repo prefix.
//!
//! Every lookup runs `gh` under a strict timeout with `kill_on_drop`,
//! so a stuck `gh` (auth prompt, network stall) can't jam the caller.

use std::time::Duration;
use tracing::warn;

/// Hard cap for any single `gh api` call. Webhook delivery and CLI
/// wake invocations both want to fail fast rather than block on a
/// hung subprocess; 5s is well above any healthy `gh api` round-trip
/// while still being short enough to keep webhook latency bounded.
pub const GH_API_TIMEOUT: Duration = Duration::from_secs(5);

/// `gh api repos/OWNER/NAME/pulls/N --jq '.body'`. Returns `None` for
/// any failure mode (timeout, gh error, empty body).
pub async fn fetch_pr_body_by_number(full_name: &str, number: u64) -> Option<String> {
    // Bind formatted strings to locals rather than relying on
    // temporary-lifetime extension across the `.await` inside
    // `run_gh_for_body`. Today's Rust rules do keep the temporaries
    // alive for the whole call expression, but the binding makes the
    // control flow obvious to readers and is robust against future
    // refactors that might move the await behind an additional layer.
    let endpoint = format!("repos/{full_name}/pulls/{number}");
    let context = format!("pr={number}");
    run_gh_for_body(&["api", &endpoint, "--jq", ".body"], full_name, &context).await
}

/// `gh api repos/OWNER/NAME/commits/SHA/pulls --jq '.[0].body'`. Used
/// when only the commit SHA is known — post-merge default-branch
/// `check_suite` events, deploy-side wake invocations triggered after
/// merge, etc. GitHub still associates the PR with the resulting
/// merge/squash commit, so this recovers the original PR body (and
/// thus the claude-session marker) for author routing.
pub async fn fetch_pr_body_by_commit(full_name: &str, sha: &str) -> Option<String> {
    let endpoint = format!("repos/{full_name}/commits/{sha}/pulls");
    let context = format!("sha={sha}");
    run_gh_for_body(
        &["api", &endpoint, "--jq", ".[0].body"],
        full_name,
        &context,
    )
    .await
}

/// Spawn `gh` with the supplied args, enforce [`GH_API_TIMEOUT`] with
/// `kill_on_drop`, and return stdout trimmed. `context` is a short
/// identifier (e.g. `pr=536` or `sha=abc12345`) included in warnings
/// so a journal-tail diagnoses UUID-routing regressions without
/// having to reconstruct which call failed.
async fn run_gh_for_body(args: &[&str], repo: &str, context: &str) -> Option<String> {
    let spawn = tokio::process::Command::new("gh")
        .args(args)
        .kill_on_drop(true)
        .output();
    let out = match tokio::time::timeout(GH_API_TIMEOUT, spawn).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            warn!(
                error = %e,
                repo,
                context,
                "failed to spawn `gh` for UUID resolution"
            );
            return None;
        }
        Err(_) => {
            warn!(
                timeout_secs = GH_API_TIMEOUT.as_secs(),
                repo, context, "`gh api` timed out during UUID resolution; killed"
            );
            return None;
        }
    };
    if !out.status.success() {
        warn!(
            status = %out.status,
            stderr = %String::from_utf8_lossy(&out.stderr).trim(),
            repo,
            context,
            "`gh api` failed during UUID resolution"
        );
        return None;
    }
    let body = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if body.is_empty() {
        None
    } else {
        Some(body)
    }
}
