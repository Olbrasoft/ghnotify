//! ghnotify — GitHub webhook → Claude Code session forwarder via tmux send-keys.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod config;
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
            tmux::send_prompt(&session, &prompt)?;
            println!("delivered prompt to tmux session: {session}");
            Ok(())
        }
        Command::List => {
            for s in sessions::list_claude_sessions()? {
                println!("{s}");
            }
            Ok(())
        }
        Command::Doctor => sessions::doctor(),
    }
}
