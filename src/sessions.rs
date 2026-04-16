//! Discovery of Claude tmux sessions and basic doctor diagnostics.

use anyhow::{Context, Result};
use std::process::Command;

/// List tmux sessions whose name starts with "claude-".
pub fn list_claude_sessions() -> Result<Vec<String>> {
    let out = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o,
        Ok(_) => return Ok(Vec::new()), // no server / no sessions
        Err(e) => return Err(e).context("failed to spawn tmux"),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    Ok(stdout
        .lines()
        .filter(|l| l.starts_with("claude-"))
        .map(|s| s.to_string())
        .collect())
}

pub fn doctor() -> Result<()> {
    fn check(name: &str, ok: bool, detail: &str) {
        let mark = if ok { "✓" } else { "✗" };
        println!("  {mark} {name:<28}  {detail}");
    }

    println!("ghnotify doctor:");

    // tmux
    let tmux = Command::new("tmux").arg("-V").output();
    match tmux {
        Ok(o) if o.status.success() => {
            check("tmux", true, &String::from_utf8_lossy(&o.stdout).trim());
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
            check("claude code", true, String::from_utf8_lossy(&o.stdout).trim());
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
