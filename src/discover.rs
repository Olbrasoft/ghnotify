//! Auto-discover GitHub repos from running `claude` processes (Linux-only,
//! reads `/proc`). Mirrors the logic in the legacy
//! `start-webhook-forwards.sh` script.
//!
//! On non-Linux platforms this returns an empty vec and the user must pass
//! `--repo owner/name` explicitly.

use anyhow::Result;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

/// Return a deduplicated, sorted list of `owner/repo` strings derived from
/// every running `claude` process whose cwd is inside a git repo with a
/// GitHub origin.
pub fn active_repos() -> Result<Vec<String>> {
    if !cfg!(target_os = "linux") {
        return Ok(Vec::new());
    }

    let mut out: BTreeSet<String> = BTreeSet::new();
    let entries = match std::fs::read_dir("/proc") {
        Ok(e) => e,
        Err(_) => return Ok(Vec::new()),
    };

    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let comm_path = format!("/proc/{name}/comm");
        let comm = match std::fs::read_to_string(&comm_path) {
            Ok(s) => s.trim().to_string(),
            Err(_) => continue,
        };
        // `claude` is the binary; `node` shows up if the user runs it via the
        // node wrapper. We restrict to `claude` to avoid false positives.
        if comm != "claude" {
            continue;
        }

        let cwd_path = format!("/proc/{name}/cwd");
        let cwd = match std::fs::read_link(&cwd_path) {
            Ok(p) => p,
            Err(_) => continue,
        };

        if let Some(repo) = repo_for_cwd(&cwd) {
            out.insert(repo);
        }
    }

    Ok(out.into_iter().collect())
}

fn repo_for_cwd(cwd: &Path) -> Option<String> {
    let cwd_str = cwd.to_str()?;
    let out = Command::new("git")
        .args(["-C", cwd_str, "remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8(out.stdout).ok()?;
    parse_github_repo(url.trim())
}

/// Extract `owner/repo` from any GitHub remote URL form:
///
///   git@github.com:owner/repo.git
///   ssh://git@github.com/owner/repo.git
///   https://github.com/owner/repo.git
///   https://github.com/owner/repo
pub fn parse_github_repo(url: &str) -> Option<String> {
    let (_, after) = url.split_once("github.com")?;
    let path = after.trim_start_matches([':', '/']);
    let path = path.trim_end_matches(".git");
    let mut parts = path.splitn(3, '/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

#[cfg(test)]
mod tests {
    use super::parse_github_repo;

    #[test]
    fn parses_ssh_url() {
        assert_eq!(
            parse_github_repo("git@github.com:Olbrasoft/cr.git").as_deref(),
            Some("Olbrasoft/cr"),
        );
    }

    #[test]
    fn parses_https_url_with_dot_git() {
        assert_eq!(
            parse_github_repo("https://github.com/Olbrasoft/GitHub.Issues.git").as_deref(),
            Some("Olbrasoft/GitHub.Issues"),
        );
    }

    #[test]
    fn parses_https_url_no_suffix() {
        assert_eq!(
            parse_github_repo("https://github.com/Olbrasoft/ghnotify").as_deref(),
            Some("Olbrasoft/ghnotify"),
        );
    }

    #[test]
    fn parses_ssh_full_form() {
        assert_eq!(
            parse_github_repo("ssh://git@github.com/Olbrasoft/ghnotify.git").as_deref(),
            Some("Olbrasoft/ghnotify"),
        );
    }

    #[test]
    fn rejects_non_github_url() {
        assert_eq!(parse_github_repo("https://gitlab.com/foo/bar.git"), None);
    }

    #[test]
    fn rejects_truncated_url() {
        assert_eq!(parse_github_repo("https://github.com/owner-only"), None);
    }
}
