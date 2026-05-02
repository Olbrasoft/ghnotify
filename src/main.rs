//! ghnotify — GitHub webhook → Claude Code session forwarder via tmux send-keys.

use anyhow::Result;
use clap::{Parser, Subcommand};

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
    ///   1. If `--commit` is given, look up the PR for that SHA via
    ///      `gh api`, extract the `<!-- claude-session: UUID -->`
    ///      marker from the PR body, and route to the session that
    ///      authored the PR (resolved via the pid index — works
    ///      regardless of how the user's bashrc names sessions).
    ///   2. Fall back to repo-name routing: `claude-<repo>` prefix
    ///      match. Used for testing and any wake invocation that has
    ///      no PR-author context.
    ///
    /// Deploy workflows that fire after a merge should pass
    /// `--commit "$GITHUB_SHA"` (or the merge commit SHA) so the
    /// wake lands on the PR author session, not on whichever
    /// `claude-<repo>-*` session happens to be open. Without it, a
    /// machine whose tmux sessions are named `claude-<user>-<tty>`
    /// (instead of `claude-<repo>-<tty>`) silently drops every
    /// deploy wake.
    Send {
        /// Repo, e.g. `Olbrasoft/VirtualAssistant` or just
        /// `VirtualAssistant`. The owner is stripped for the
        /// repo-name fallback; the full `OWNER/NAME` form is
        /// required for `--commit` lookups.
        #[arg(long)]
        repo: String,

        /// Prompt text to type into the session. A newline (Enter) is appended.
        #[arg(long)]
        prompt: String,

        /// Commit SHA. When set, routes via the PR-author UUID
        /// extracted from the PR body, falling back to repo-name
        /// routing if no PR / no marker / lookup fails.
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

/// Routing core for `ghnotify send`. Tries PR-author UUID first when
/// `--commit` is supplied, falls back to repo-name routing on miss.
async fn send_command(repo: String, prompt: String, commit: Option<String>) -> Result<()> {
    let session = if let Some(sha) = commit.as_deref() {
        match resolve_session_via_commit(&repo, sha).await {
            Some(s) => s,
            None => {
                // Lookup miss is non-fatal: any of "PR not yet
                // created", "no marker in body", "session not on this
                // host", "tmux not running" lands here. Repo routing
                // is the historical behavior, so degrading to it keeps
                // wakes flowing for repos whose tmux sessions are
                // named `claude-<repo>-*` even when the marker path
                // can't resolve.
                eprintln!(
                    "note: --commit lookup did not yield a UUID; falling back to repo-name routing"
                );
                resolve_session_via_repo_or_exit(&repo)
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

/// `gh api commits/SHA/pulls` → extract marker → resolve via pid
/// index. Returns `None` for any miss along the chain so the caller
/// can fall back to repo routing.
async fn resolve_session_via_commit(repo: &str, sha: &str) -> Option<String> {
    // The commits/SHA/pulls endpoint requires the full OWNER/NAME
    // form. Reject a bare repo name immediately rather than letting
    // gh return a 404.
    if !repo.contains('/') {
        eprintln!("note: --commit requires --repo OWNER/NAME (got '{repo}'); skipping UUID lookup");
        return None;
    }
    let body = gh_lookup::fetch_pr_body_by_commit(repo, sha).await?;
    let uuid = session_marker::extract_uuid(&body)?;
    session_by_uuid::resolve_tmux_session(&uuid).ok().flatten()
}

/// Repo-name routing: prefix-match `claude-<repo>` against live tmux
/// sessions. Exits the process on miss — same behavior as the
/// pre-`--commit` `Send` implementation.
fn resolve_session_via_repo_or_exit(repo: &str) -> String {
    let base = tmux::session_name_for_repo(repo);
    match sessions::resolve_session_for_repo(repo) {
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
            for s in sessions::list_claude_sessions()? {
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
