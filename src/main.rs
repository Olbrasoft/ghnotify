//! ghnotify — GitHub webhook → Claude Code session forwarder via tmux send-keys.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod agent;
mod config;
mod event;
mod gh_lookup;
mod install;
mod install_hook;
mod session_by_uuid;
mod session_marker;
mod sessions;
mod tmux;
mod webhook;

#[derive(Parser)]
#[command(name = "ghnotify", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Path to ghnotify.toml. Defaults to ./ghnotify.toml then $XDG_CONFIG_HOME/ghnotify/config.toml.
    #[arg(long, global = true, env = "GHNOTIFY_CONFIG")]
    config: Option<std::path::PathBuf>,

    /// Tracing filter (e.g. "ghnotify=debug,tower_http=info").
    #[arg(
        long,
        global = true,
        env = "GHNOTIFY_LOG",
        default_value = "ghnotify=info"
    )]
    log: String,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP webhook receiver. Routes GitHub events to tmux sessions.
    ///
    /// Picks a listening socket in this order:
    ///   1. systemd socket activation (LISTEN_FDS env)
    ///   2. --bind override
    ///   3. config `server.bind` (default 127.0.0.1:9877)
    Serve {
        /// Bind address. Ignored when a systemd socket is passed.
        #[arg(long, env = "GHNOTIFY_BIND")]
        bind: Option<String>,

        /// Exit after serving exactly one request. Intended to be paired with
        /// systemd socket activation so nothing is running between webhooks.
        #[arg(long)]
        one_shot: bool,
    },

    /// Send a one-shot prompt to a Claude tmux session.
    ///
    /// Routing strategy, in order:
    ///   1. If `--commit` is given (requires `--repo OWNER/NAME`):
    ///      look up the PR for that SHA via `gh api`, extract the
    ///      `<!-- claude-session: UUID -->` marker, and route via
    ///      the strict pid-index resolver. On a clean miss (no PR,
    ///      no marker, session not on this host) falls back to
    ///      repo-name routing. Operational resolver failures
    ///      (tmux probe error, etc.) exit non-zero rather than
    ///      silently degrading.
    ///   2. Without `--commit`: repo-name routing only —
    ///      `claude-<repo>` prefix match.
    ///
    /// Deploy workflows that fire after a merge should pass
    /// `--commit "$GITHUB_SHA"` (the full 40-char SHA, NOT a short
    /// SHA — `commits/SHA/pulls` is unreliable with abbreviations)
    /// so the wake lands on the PR author session, not on whichever
    /// `claude-<repo>-*` session happens to be open.
    Send {
        /// Repo. Bare name (e.g. `VirtualAssistant`) is OK for
        /// repo-name routing; `--commit` requires the full
        /// `OWNER/NAME` form.
        #[arg(long)]
        repo: String,

        /// Prompt text to type into the session. A newline (Enter) is appended.
        #[arg(long)]
        prompt: String,

        /// Full 40-char commit SHA. When set, routes via the
        /// PR-author UUID extracted from the PR body. Hard-errors
        /// on misshaped `--repo`; clean misses (no PR, no marker)
        /// fall back to repo-name routing; operational resolver
        /// errors exit non-zero.
        #[arg(long)]
        commit: Option<String>,
    },

    /// List discovered Claude tmux sessions on this machine.
    List,

    /// Diagnostics: tmux installed, gh-cli auth, config reachable, sessions present.
    Doctor,

    /// Resolve a Claude session UUID to the tmux session hosting it.
    /// Useful for debugging cross-repo wake routing.
    ResolveUuid {
        /// Session UUID (e.g. from the `<!-- claude-session: ... -->`
        /// marker in a PR body).
        uuid: String,
    },

    /// Install the `claude()` shell wrapper into ~/.bashrc or ~/.zshrc.
    /// Idempotent: re-running updates the managed block in place.
    Install {
        /// Which shell rc to target. Default: detect from $SHELL.
        #[arg(long, value_parser = ["bash", "zsh"])]
        shell: Option<String>,

        /// Explicit rc file path. Overrides --shell and $SHELL detection.
        #[arg(long)]
        rc: Option<std::path::PathBuf>,

        /// Print the planned change without writing.
        #[arg(long)]
        dry_run: bool,
    },

    /// Register (or update) a GitHub webhook that will POST events to the
    /// given public URL. Supports both repository-scoped (`--repo OWNER/NAME`,
    /// needs `admin:repo_hook`) and org-scoped (`--org NAME`, needs
    /// `admin:org_hook`) hooks. An org hook fires for every current and future
    /// repo in the org — prefer it over per-repo hooks when you own the org.
    /// Note: personal user accounts cannot host webhooks that cover all their
    /// repos; use a GitHub App for that case.
    InstallHook {
        /// Target repository, e.g. `Olbrasoft/ghnotify`. Repeatable.
        #[arg(long)]
        repo: Vec<String>,

        /// Target organization. Repeatable. Mutually combinable with --repo.
        #[arg(long)]
        org: Vec<String>,

        /// Public URL GitHub should POST to (e.g. https://tunnel.example.com/gh-webhook).
        #[arg(long)]
        url: String,

        /// HMAC shared secret. If omitted, reads `github.webhook_secret` from config.
        #[arg(long)]
        secret: Option<String>,

        /// Event types to subscribe to (comma-separated).
        #[arg(
            long,
            default_value = "check_suite,pull_request_review,pull_request,issues,issue_comment"
        )]
        events: String,
    },
}

/// Routing core for `ghnotify send`. With `--commit`, routes via the
/// PR-author UUID using the strict pid-index resolver and falls back
/// to repo-name routing on a clean miss; surfaces operational errors
/// loudly. Without `--commit`, repo-name routing only.
async fn send_command(repo: String, prompt: String, commit: Option<String>) -> Result<()> {
    let session = if let Some(sha) = commit.as_deref() {
        // Hard-error on a misshaped --repo for --commit: the commits/
        // SHA/pulls endpoint requires the full OWNER/NAME form. Soft
        // downgrade hides automation misconfiguration and silently
        // reintroduces the exact misrouting --commit was added to
        // prevent.
        if let Err(msg) = validate_repo_for_commit(&repo) {
            eprintln!("{msg}");
            std::process::exit(2);
        }
        match resolve_session_via_commit(&repo, sha).await {
            CommitRouting::Resolved(s) => s,
            CommitRouting::CleanMiss(reason) => {
                // Clean miss — no PR yet, no marker in body, session
                // truly absent on this host. Falling through to repo
                // routing is the historical behavior; on a machine
                // whose sessions don't follow `claude-<repo>-*` it
                // will then fail loudly rather than misrouting.
                eprintln!("note: --commit lookup did not yield a UUID ({reason}); falling back to repo-name routing");
                resolve_session_via_repo_or_exit(&repo)
            }
            CommitRouting::ResolverError(e) => {
                // Operational failure in the local UUID resolver
                // (tmux probing, /proc access, ~/.claude/sessions
                // unreadable). Surface it instead of swallowing —
                // this used to silently degrade to repo routing,
                // which masks regressions in the resolver itself.
                eprintln!("error: UUID resolver failed: {e}");
                std::process::exit(2);
            }
        }
    } else {
        resolve_session_via_repo_or_exit(&repo)
    };

    match tmux::send_prompt(&session, &prompt)? {
        tmux::Delivery::Delivered => {
            println!("delivered prompt to tmux session: {session}");
            Ok(())
        }
        tmux::Delivery::NoSession => {
            eprintln!("tmux session '{session}' disappeared before send. Use `ghnotify list` to see available sessions.");
            std::process::exit(2);
        }
    }
}

/// Pure validation: `--commit` is only meaningful when paired with
/// `OWNER/NAME` form because the gh-api endpoint requires it.
/// Returns the user-facing error message on rejection so the caller
/// can print it consistently.
fn validate_repo_for_commit(repo: &str) -> Result<(), String> {
    if repo.is_empty() {
        return Err("error: --repo is empty".into());
    }
    if !repo.contains('/') {
        return Err(format!(
            "error: --commit requires --repo OWNER/NAME (got '{repo}'); refusing to silently downgrade to repo-name routing"
        ));
    }
    Ok(())
}

/// Result of the `--commit` UUID-routing path. Distinguishes a clean
/// miss (where falling back to repo routing is reasonable) from an
/// operational error (where falling back would silently hide a real
/// problem). The variants exist as data so [`send_command`]'s policy
/// — fall back vs. exit — is testable and explicit.
enum CommitRouting {
    Resolved(String),
    CleanMiss(&'static str),
    ResolverError(anyhow::Error),
}

/// `gh api commits/SHA/pulls` → extract marker → resolve via the
/// strict pid-index resolver. Caller must validate `repo` shape via
/// [`validate_repo_for_commit`] first.
async fn resolve_session_via_commit(repo: &str, sha: &str) -> CommitRouting {
    let Some(body) = gh_lookup::fetch_pr_body_by_commit(repo, sha).await else {
        return CommitRouting::CleanMiss("no PR found for this commit");
    };
    // `send --commit` resolves via the Claude-only strict pid-index path
    // (see [`session_by_uuid::resolve_tmux_session_strict`]) so only the
    // Claude marker is meaningful here. Sub-issues #27 / #29 wire the
    // parallel Codex resolver and switch this call to `extract_marker`.
    let Some(uuid) = session_marker::extract_uuid(&body, agent::Agent::claude().pr_marker_tag)
    else {
        return CommitRouting::CleanMiss("PR body has no claude-session marker");
    };
    match session_by_uuid::resolve_tmux_session_strict(&uuid) {
        Ok(Some(name)) => CommitRouting::Resolved(name),
        Ok(None) => CommitRouting::CleanMiss("UUID has no live pid-indexed tmux session"),
        Err(e) => CommitRouting::ResolverError(e),
    }
}

/// Repo-name routing: prefix-match `claude-<repo>` against live tmux
/// sessions. Exits the process on miss — same behavior as the
/// pre-`--commit` `Send` implementation.
///
/// Currently scoped to Claude. The agent-agnostic / Codex-aware variant
/// arrives in sub-issue #29 (webhook orchestration); this CLI path is
/// dispatch-aware only when the caller knows the agent, which `ghnotify send`
/// doesn't (yet). Until #29 lands, `send` keeps its historical Claude-only
/// semantics so the user-visible behavior is unchanged.
fn resolve_session_via_repo_or_exit(repo: &str) -> String {
    let agent = agent::Agent::claude();
    let base = tmux::session_name_for_repo(repo, agent);
    match sessions::resolve_session_for_repo(repo, agent) {
        Ok(Some(s)) => s,
        Ok(None) => {
            eprintln!("no tmux session for repo '{repo}' (looked for '{base}' or '{base}-*'). Use `ghnotify list` to see available sessions.");
            std::process::exit(2);
        }
        Err(e) => {
            eprintln!("failed to enumerate tmux sessions: {e}");
            std::process::exit(2);
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&cli.log))
        .with_target(false)
        .init();

    match cli.command {
        Command::Serve { bind, one_shot } => {
            let cfg = config::load(cli.config.as_deref())?;
            webhook::serve(cfg, bind, one_shot).await
        }
        Command::Send {
            repo,
            prompt,
            commit,
        } => send_command(repo, prompt, commit).await,
        Command::List => {
            // Lists every agent-owned session (Claude + Codex). Per-agent
            // filtering isn't useful for the `list` UX — users want to see
            // every session ghnotify could possibly deliver to.
            for s in sessions::list_agent_sessions()? {
                println!("{s}");
            }
            Ok(())
        }
        Command::Doctor => sessions::doctor(),
        Command::ResolveUuid { uuid } => match session_by_uuid::resolve_tmux_session(&uuid)? {
            Some(name) => {
                println!("{name}");
                Ok(())
            }
            None => {
                // Lumped miss reasons — any of these produce Ok(None):
                //   * no ~/.claude/projects/*/UUID.jsonl on this host
                //     (unknown UUID, session on another machine, or typo)
                //   * JSONL found but no cwd in the first records
                //     (malformed / partial transcript)
                //   * cwd known but no live tmux session matches the
                //     basename (session terminated, running outside tmux)
                // Enumerated rather than a single generic message so an
                // operator can narrow the diagnosis without instrumenting
                // the resolver.
                eprintln!(
                    "no tmux session for UUID {uuid}. Possible causes: \
                     UUID unknown on this host (no matching JSONL under ~/.claude/projects), \
                     JSONL transcript has no cwd record, \
                     or the session's cwd has no live claude-<repo>[-<tty>] tmux session."
                );
                std::process::exit(2);
            }
        },
        Command::Install { shell, rc, dry_run } => {
            let shell = match shell.as_deref() {
                Some(s) => install::Shell::from_name(s)?,
                None => install::Shell::detect(),
            };
            let rc_path = install::resolve_rc_path(rc, shell)?;
            install::run(rc_path, dry_run).map(|_| ())
        }
        Command::InstallHook {
            repo,
            org,
            url,
            secret,
            events,
        } => {
            if repo.is_empty() && org.is_empty() {
                anyhow::bail!("at least one --repo or --org is required");
            }
            let cfg = config::load(cli.config.as_deref())?;
            let secret = secret.or(cfg.github.webhook_secret);
            let mut scopes: Vec<install_hook::Scope> = Vec::new();
            scopes.extend(repo.into_iter().map(install_hook::Scope::Repo));
            scopes.extend(org.into_iter().map(install_hook::Scope::Org));
            install_hook::run(scopes, &url, secret.as_deref(), &events).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_repo_accepts_owner_name() {
        assert!(validate_repo_for_commit("Olbrasoft/VirtualAssistant").is_ok());
    }

    #[test]
    fn validate_repo_rejects_bare_name() {
        // Bare repo with --commit is the misconfiguration that
        // silently downgraded to repo routing in the previous
        // behavior. Must produce a user-facing error so automation
        // failures are loud.
        let err = validate_repo_for_commit("VirtualAssistant").unwrap_err();
        assert!(
            err.contains("OWNER/NAME"),
            "error message must explain the required form, got: {err}"
        );
        assert!(
            err.contains("'VirtualAssistant'"),
            "must echo the bad input"
        );
    }

    #[test]
    fn validate_repo_rejects_empty() {
        assert!(validate_repo_for_commit("").is_err());
    }

    #[test]
    fn commit_routing_clean_miss_carries_reason() {
        // Sanity: the variant payload is the human-readable reason
        // we emit on stderr. Asserts the type's contract so
        // refactors can't accidentally silence the diagnostic.
        let r = CommitRouting::CleanMiss("PR body has no claude-session marker");
        match r {
            CommitRouting::CleanMiss(reason) => {
                assert!(reason.contains("marker"));
            }
            _ => panic!("expected CleanMiss"),
        }
    }
}
