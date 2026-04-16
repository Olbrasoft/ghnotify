//! Config loading. Looks for `./ghnotify.toml` then `$XDG_CONFIG_HOME/ghnotify/config.toml`.
//! All keys are optional; defaults are sensible for a single-user dev box.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub server: Server,

    #[serde(default)]
    pub github: Github,
}

#[derive(Debug, Deserialize)]
pub struct Server {
    /// Bind address. Default: 127.0.0.1:9877.
    pub bind: String,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:9877".into(),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct Github {
    /// Optional shared HMAC secret for GitHub webhook signature verification (X-Hub-Signature-256).
    /// If unset, signatures are NOT verified — only acceptable for `gh webhook forward`-style local relay.
    pub webhook_secret: Option<String>,

    /// Logins whose actions should NOT wake your Claude session — typically
    /// your own GitHub username plus any bots that act on your behalf
    /// (`github-actions[bot]`, `claude[bot]`, etc.). When the webhook payload
    /// has `sender.login` matching any entry, the event is dropped.
    ///
    /// Example: `own_logins = ["jirka", "github-actions[bot]", "claude[bot]"]`
    #[serde(default)]
    pub own_logins: Vec<String>,
}

pub fn load(explicit: Option<&Path>) -> Result<Config> {
    let path = explicit
        .map(PathBuf::from)
        .or_else(|| {
            let cwd = PathBuf::from("./ghnotify.toml");
            if cwd.exists() {
                Some(cwd)
            } else {
                dirs::config_dir().map(|d| d.join("ghnotify/config.toml"))
            }
        });

    let Some(path) = path else {
        return Ok(Config::default());
    };

    if !path.exists() {
        return Ok(Config::default());
    }

    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading config {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("parsing config {}", path.display()))
}
