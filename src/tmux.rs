//! tmux send-keys wrapper. The single mechanism by which we deliver prompts
//! into a running Claude Code session.

use anyhow::{anyhow, Context, Result};
use std::process::Command;

/// Convert a repo name (with possible dots) to the tmux session name our
/// `claude()` bash wrapper uses. Must stay in lockstep with `~/.bashrc`.
///
/// E.g. "GitHub.Issues" → "claude-GitHub-Issues".
pub fn session_name_for_repo(repo: &str) -> String {
    // Strip "owner/" if present.
    let bare = repo.rsplit('/').next().unwrap_or(repo);
    format!("claude-{}", bare.replace('.', "-"))
}

/// Returns true if a tmux session with this name exists.
pub fn has_session(name: &str) -> Result<bool> {
    let status = Command::new("tmux")
        .args(["has-session", "-t", name])
        .status()
        .context("failed to spawn tmux (is tmux installed?)")?;
    Ok(status.success())
}

/// Send a prompt into a running tmux session.
///
/// Sends in two steps to work around Claude Code TUI input handling:
///   1. type the literal prompt text (no auto-submit)
///   2. brief sleep, then send `Enter` separately
///
/// Without the split, the TUI sometimes accepts the text but discards the Enter
/// (verified empirically on claude-code 2.1.111).
pub fn send_prompt(session: &str, prompt: &str) -> Result<()> {
    if !has_session(session)? {
        return Err(anyhow!(
            "tmux session '{session}' not found. Is the Claude session running? \
             Use `ghnotify list` to see available sessions."
        ));
    }

    // Step 1: type the prompt text (no Enter)
    let typed = Command::new("tmux")
        .args(["send-keys", "-t", session, "-l", prompt])
        .status()
        .context("tmux send-keys (text) failed")?;
    if !typed.success() {
        return Err(anyhow!("tmux send-keys returned non-zero for text"));
    }

    // Step 2: tiny pause to let the TUI register the typed buffer, then Enter
    std::thread::sleep(std::time::Duration::from_millis(150));

    let enter = Command::new("tmux")
        .args(["send-keys", "-t", session, "C-m"])
        .status()
        .context("tmux send-keys (Enter) failed")?;
    if !enter.success() {
        return Err(anyhow!("tmux send-keys returned non-zero for Enter"));
    }

    Ok(())
}
