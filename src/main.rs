//! ghnotify — GitHub webhook → Claude Code session forwarder via tmux send-keys.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod config;
mod discover;
mod install;
mod sessions;
mod tmux;
mod watch;
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
    #[arg(long, global = true, env = "GHNOTIFY_LOG", default_value = "ghnotify=info")]
    log: String,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP webhook receiver. Routes GitHub events to tmux sessions.
    Serve {
        /// Bind address. Defaults to 127.0.0.1:9877.
        #[arg(long, env = "GHNOTIFY_BIND")]
        bind: Option<String>,
    },

    /// Send a one-shot prompt to a Claude tmux session by repo name.
    /// Useful for testing and manual triggers.
    Send {
        /// Repo name (without owner). E.g. "GitHub.Issues" → tmux session "claude-GitHub-Issues".
        #[arg(long)]
        repo: String,

        /// Prompt text to type into the session. A newline (Enter) is appended.
        #[arg(long)]
        prompt: String,
    },

    /// List discovered Claude tmux sessions on this machine.
    List,

    /// Diagnostics: tmux installed, gh-cli auth, config reachable, sessions present.
    Doctor,

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

    /// One-process mode: spawn `gh webhook forward` per repo AND run the
    /// local HTTP receiver in the same binary. Use this instead of running
    /// `serve` plus a separate gh forwarder.
    Watch {
        /// GitHub repo `owner/name` to subscribe to. Repeatable. If omitted,
        /// auto-discovers from running `claude` processes (Linux only).
        #[arg(long)]
        repo: Vec<String>,

        /// GitHub event types to subscribe to (comma-separated, no spaces).
        #[arg(long, default_value = watch::DEFAULT_EVENTS)]
        events: String,

        /// Bind address override for the local receiver. Defaults to config.
        #[arg(long, env = "GHNOTIFY_BIND")]
        bind: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&cli.log))
        .with_target(false)
        .init();

    match cli.command {
        Command::Serve { bind } => {
            let cfg = config::load(cli.config.as_deref())?;
            webhook::serve(cfg, bind).await
        }
        Command::Send { repo, prompt } => {
            let session = tmux::session_name_for_repo(&repo);
            match tmux::send_prompt(&session, &prompt)? {
                tmux::Delivery::Delivered => {
                    println!("delivered prompt to tmux session: {session}");
                    Ok(())
                }
                tmux::Delivery::NoSession => {
                    eprintln!("no tmux session named '{session}'. Use `ghnotify list` to see available sessions.");
                    std::process::exit(2);
                }
            }
        }
        Command::List => {
            for s in sessions::list_claude_sessions()? {
                println!("{s}");
            }
            Ok(())
        }
        Command::Doctor => sessions::doctor(),
        Command::Install { shell, rc, dry_run } => {
            let shell = match shell.as_deref() {
                Some(s) => install::Shell::from_name(s)?,
                None => install::Shell::detect(),
            };
            let rc_path = install::resolve_rc_path(rc, shell)?;
            install::run(rc_path, dry_run).map(|_| ())
        }
        Command::Watch { repo, events, bind } => {
            let cfg = config::load(cli.config.as_deref())?;
            watch::run(cfg, repo, events, bind).await
        }
    }
}
