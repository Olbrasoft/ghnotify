//! `ghnotify watch` — runs the local HTTP listener AND spawns one
//! `gh webhook forward` subprocess per repo, in a single tokio runtime.
//!
//! End state: one always-running binary. No systemd unit per repo, no separate
//! gh process to babysit, no HTTP loopback the user has to wire up by hand.
//!
//! `gh webhook forward` is still a runtime dep — replicating its WebSocket
//! relay client natively is a future step.

use crate::{config::Config, discover, webhook};
use anyhow::{anyhow, Context, Result};
use std::process::Stdio;
use tokio::process::Command;
use tokio::signal;
use tracing::{info, warn};

/// URL that GitHub assigns to hooks created by `gh webhook forward`.
/// Stable string, used to identify zombie hooks left behind by previous runs.
const FORWARDER_HOOK_URL: &str = "https://webhook-forwarder.github.com/hook";

/// Default GitHub events to subscribe to. Each subscribed event is then
/// passed through `event::classify` to filter out noise (per-file
/// `workflow_run`, queued/in_progress `check_suite` transitions, etc.) and
/// reformatted into a meaningful prompt before delivery.
///
/// `workflow_run` is intentionally absent — `check_suite` is the aggregate
/// of every workflow that ran for a commit, so subscribing to both would
/// double up. `ping` is sent by GitHub on hook creation regardless of
/// subscription, no need to list it.
pub const DEFAULT_EVENTS: &str =
    "check_suite,pull_request_review,pull_request,issues,issue_comment";

pub async fn run(
    cfg: Config,
    repos: Vec<String>,
    events: String,
    bind_override: Option<String>,
) -> Result<()> {
    // 1. Resolve repo list.
    let repos = if repos.is_empty() {
        let discovered = discover::active_repos()?;
        if discovered.is_empty() {
            return Err(anyhow!(
                "no --repo given and no active claude sessions found. \
                 Pass --repo owner/name (repeatable)."
            ));
        }
        info!(repos = ?discovered, "auto-discovered from running claude processes");
        discovered
    } else {
        repos
    };

    let bind = bind_override
        .clone()
        .unwrap_or_else(|| cfg.server.bind.clone());

    // 2. Clean up zombie forwarder hooks left by a previous run (or by the
    //    legacy systemd unit). Without this, `gh webhook forward` errors with
    //    HTTP 422 "Hook already exists" and immediately exits.
    for repo in &repos {
        if let Err(e) = cleanup_zombie_hooks(repo).await {
            warn!(repo, error = %e, "failed to clean up old forwarder hooks (continuing)");
        }
    }

    // 3. Spawn one `gh webhook forward` per repo. kill_on_drop guarantees we
    //    SIGKILL them when this task winds down (Ctrl+C or listener exit).
    let mut children = Vec::new();
    for repo in &repos {
        let url = format!("http://{bind}/webhook");
        let child = Command::new("gh")
            .args([
                "webhook", "forward", "--repo", repo, "--events", &events, "--url", &url,
            ])
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!("spawning `gh webhook forward` for {repo} (is `gh` on PATH and authed?)")
            })?;
        info!(repo, pid = child.id(), "started gh webhook forward");
        children.push(child);
    }

    // 3. Run the listener in a task so we can race it against Ctrl+C.
    let listener = tokio::spawn(webhook::serve(cfg, bind_override));

    // 4. Wait for shutdown trigger.
    tokio::select! {
        _ = signal::ctrl_c() => {
            info!("received Ctrl+C, terminating gh subprocesses");
        }
        result = listener => {
            warn!(?result, "listener task ended unexpectedly");
        }
    }

    // 5. Drop children → kill_on_drop SIGKILLs each `gh webhook forward`.
    drop(children);
    Ok(())
}

/// Delete any leftover `https://webhook-forwarder.github.com/hook` entries on
/// the given repo. Each `gh webhook forward` run leaks one if it doesn't shut
/// down cleanly, and GitHub refuses to create a duplicate.
async fn cleanup_zombie_hooks(repo: &str) -> Result<()> {
    let list = Command::new("gh")
        .args([
            "api",
            &format!("repos/{repo}/hooks"),
            "--jq",
            &format!(".[] | select(.config.url == \"{FORWARDER_HOOK_URL}\") | .id"),
        ])
        .stderr(Stdio::inherit())
        .output()
        .await
        .with_context(|| format!("listing hooks for {repo}"))?;

    if !list.status.success() {
        return Err(anyhow!(
            "gh api repos/{repo}/hooks failed (status {:?})",
            list.status.code()
        ));
    }

    let stdout = String::from_utf8_lossy(&list.stdout);
    for id in stdout.split_whitespace() {
        let status = Command::new("gh")
            .args(["api", "-X", "DELETE", &format!("repos/{repo}/hooks/{id}")])
            .stderr(Stdio::inherit())
            .status()
            .await
            .with_context(|| format!("deleting hook {id} from {repo}"))?;
        if status.success() {
            info!(repo, hook_id = id, "deleted zombie forwarder hook");
        } else {
            warn!(repo, hook_id = id, ?status, "failed to delete zombie hook");
        }
    }
    Ok(())
}
