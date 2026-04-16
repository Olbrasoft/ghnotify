//! `ghnotify install-hook` — register a webhook pointing at our public endpoint.
//!
//! Two scopes are supported, matching GitHub's own webhook API:
//!   * Repository — `POST /repos/{owner}/{repo}/hooks` (needs `admin:repo_hook`)
//!   * Organization — `POST /orgs/{org}/hooks` (needs `admin:org_hook`).
//!     An org-level webhook fires for every existing AND future repo in the
//!     org, so this is the right answer if you own the org: register once,
//!     new repos are covered automatically.
//!
//! Idempotent either way: if a hook with the same URL already exists, its
//! configuration is patched rather than duplicated (GitHub rejects duplicates
//! at the same URL with HTTP 422).

use anyhow::{anyhow, Context, Result};
use serde_json::json;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{info, warn};

pub enum Scope {
    Repo(String),
    Org(String),
}

impl Scope {
    fn label(&self) -> &str {
        match self {
            Scope::Repo(_) => "repo",
            Scope::Org(_) => "org",
        }
    }

    fn name(&self) -> &str {
        match self {
            Scope::Repo(r) | Scope::Org(r) => r,
        }
    }

    fn hooks_path(&self) -> String {
        match self {
            Scope::Repo(r) => format!("repos/{r}/hooks"),
            Scope::Org(o) => format!("orgs/{o}/hooks"),
        }
    }

    fn hook_path(&self, id: u64) -> String {
        format!("{}/{id}", self.hooks_path())
    }
}

pub async fn run(
    scopes: Vec<Scope>,
    url: &str,
    secret: Option<&str>,
    events_csv: &str,
) -> Result<()> {
    let events: Vec<&str> = events_csv.split(',').map(str::trim).collect();
    for scope in &scopes {
        match install_one(scope, url, secret, &events).await {
            Ok(action) => info!(
                scope = scope.label(),
                name = scope.name(),
                url,
                action,
                "webhook configured"
            ),
            Err(e) => warn!(
                scope = scope.label(),
                name = scope.name(),
                error = %e,
                "failed to install webhook"
            ),
        }
    }
    Ok(())
}

async fn install_one(
    scope: &Scope,
    url: &str,
    secret: Option<&str>,
    events: &[&str],
) -> Result<&'static str> {
    let existing = find_hook(scope, url).await?;
    let mut config = serde_json::Map::new();
    config.insert("url".into(), json!(url));
    config.insert("content_type".into(), json!("json"));
    if let Some(s) = secret {
        config.insert("secret".into(), json!(s));
    }
    let body = json!({
        "name": "web",
        "active": true,
        "events": events,
        "config": config,
    });

    let body_str = serde_json::to_string(&body)?;
    match existing {
        Some(id) => {
            gh_api(
                &["api", "-X", "PATCH", &scope.hook_path(id)],
                Some(&body_str),
            )
            .await
            .with_context(|| format!("patching hook {id} on {}", scope.name()))?;
            Ok("patched")
        }
        None => {
            gh_api(&["api", "-X", "POST", &scope.hooks_path()], Some(&body_str))
                .await
                .with_context(|| format!("creating hook on {}", scope.name()))?;
            Ok("created")
        }
    }
}

async fn find_hook(scope: &Scope, url: &str) -> Result<Option<u64>> {
    let out = Command::new("gh")
        .args([
            "api",
            &scope.hooks_path(),
            "--jq",
            &format!(".[] | select(.config.url == \"{url}\") | .id"),
        ])
        .stderr(Stdio::inherit())
        .output()
        .await
        .with_context(|| format!("listing hooks for {}", scope.name()))?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh api {} exited {:?}",
            scope.hooks_path(),
            out.status.code()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok()))
}

async fn gh_api(args: &[&str], stdin_body: Option<&str>) -> Result<()> {
    let mut cmd = Command::new("gh");
    cmd.args(args)
        .args(["-H", "Accept: application/vnd.github+json"]);
    if stdin_body.is_some() {
        cmd.args(["--input", "-"]);
        cmd.stdin(Stdio::piped());
    }
    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    let mut child = cmd.spawn()?;
    if let Some(body) = stdin_body {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("gh stdin not captured"))?;
        stdin.write_all(body.as_bytes()).await?;
        stdin.shutdown().await?;
    }
    let status = child.wait().await?;
    if !status.success() {
        return Err(anyhow!("gh api failed with {:?}", status.code()));
    }
    Ok(())
}
