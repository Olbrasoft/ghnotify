//! `ghnotify install` — idempotently writes the `claude()` shell wrapper into
//! the user's bash/zsh rc file. The wrapper is what makes every `claude` run
//! land inside a tmux session named `claude-<repo>`, which is the address
//! `ghnotify` uses to deliver prompts.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

const BEGIN_MARK: &str = "# >>> ghnotify managed block >>>";
const END_MARK: &str = "# <<< ghnotify managed block <<<";

/// The shell snippet we manage. Must stay in lockstep with
/// `tmux::session_name_for_repo` in this crate (dots → dashes, basename of repo).
///
/// We deliberately do NOT use `exec tmux …` so the parent shell survives after
/// the user types `/exit` inside Claude — without that, the launching terminal
/// closes too, which is hostile when launched from a desktop shortcut.
const SNIPPET: &str = r#"# Managed by `ghnotify install`. Do not edit between the markers — re-run
# `ghnotify install` to update. Wraps every `claude` invocation in a tmux
# session named `claude-<repo>`, so ghnotify can route webhook events to it.
claude() {
    if [ -n "$TMUX" ]; then
        command claude "$@"
        return
    fi

    for arg in "$@"; do
        case "$arg" in
            -p|--print|--version|--help|-h|-v)
                command claude "$@"
                return
                ;;
        esac
    done

    local name root
    if root=$(git rev-parse --show-toplevel 2>/dev/null); then
        name="claude-$(basename "$root")"
    else
        name="claude-home"
    fi
    # tmux session names cannot contain dots → replace with dashes
    name="${name//./-}"

    # NOTE: do NOT use `exec` here. We want bash to survive after tmux exits
    # (e.g. user types /exit inside Claude) so the launching terminal stays
    # open and the user can `cd` somewhere else and run `claude` again.
    if tmux has-session -t "$name" 2>/dev/null; then
        tmux attach -t "$name"
        return
    fi

    tmux new-session -s "$name" -- claude "$@"
}
"#;

/// Which shell rc file we're targeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
}

impl Shell {
    /// Detect from `$SHELL`. Defaults to bash when unset/unknown.
    pub fn detect() -> Self {
        match std::env::var("SHELL").ok().as_deref() {
            Some(s) if s.ends_with("/zsh") || s == "zsh" => Self::Zsh,
            _ => Self::Bash,
        }
    }

    pub fn from_name(s: &str) -> Result<Self> {
        match s {
            "bash" => Ok(Self::Bash),
            "zsh" => Ok(Self::Zsh),
            other => Err(anyhow!("unsupported shell '{other}' (use bash or zsh)")),
        }
    }

    /// Default rc file path under $HOME.
    fn rc_filename(self) -> &'static str {
        match self {
            Self::Bash => ".bashrc",
            Self::Zsh => ".zshrc",
        }
    }
}

/// Decide the rc file path: explicit `--rc`, else `$HOME/<.bashrc|.zshrc>`.
pub fn resolve_rc_path(explicit: Option<PathBuf>, shell: Shell) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    let home = dirs::home_dir().context("could not determine $HOME")?;
    Ok(home.join(shell.rc_filename()))
}

/// What an `install` invocation actually did. Useful for tests and dry-run output.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Created,   // file did not exist; created with snippet
    Inserted,  // file existed without our block; appended
    Replaced,  // file had our block; we replaced it (content differed)
    Unchanged, // file had our block already and content matched
}

pub fn run(rc_path: PathBuf, dry_run: bool) -> Result<Action> {
    let existing = read_or_empty(&rc_path)?;
    let (new_content, action) = compute_new_content(&existing);

    if dry_run {
        println!("--- dry-run: would write to {} ---", rc_path.display());
        println!("action: {action:?}");
        if action != Action::Unchanged {
            println!("--- new file contents ---");
            println!("{new_content}");
        }
        return Ok(action);
    }

    if action == Action::Unchanged {
        println!("✓ {} already up to date", rc_path.display());
        return Ok(action);
    }

    if let Some(parent) = rc_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir of {}", rc_path.display()))?;
    }
    std::fs::write(&rc_path, new_content)
        .with_context(|| format!("writing {}", rc_path.display()))?;

    let verb = match action {
        Action::Created => "created",
        Action::Inserted => "added claude() wrapper to",
        Action::Replaced => "updated claude() wrapper in",
        Action::Unchanged => unreachable!(),
    };
    println!("✓ {verb} {}", rc_path.display());
    println!();
    println!(
        "Open a new terminal (or `source {}`) for the wrapper to take effect.",
        rc_path.display()
    );
    Ok(action)
}

fn read_or_empty(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Pure function: given current rc contents, return what they should become
/// and which action that represents. Kept side-effect-free for unit tests.
fn compute_new_content(existing: &str) -> (String, Action) {
    let block = format!("{BEGIN_MARK}\n{SNIPPET}{END_MARK}\n");

    if let Some(start) = existing.find(BEGIN_MARK) {
        if let Some(end_rel) = existing[start..].find(END_MARK) {
            let end = start + end_rel + END_MARK.len();
            // Consume a single trailing newline if present so we don't accumulate blank lines.
            let after = if existing[end..].starts_with('\n') {
                &existing[end + 1..]
            } else {
                &existing[end..]
            };
            let mut out = String::with_capacity(existing.len() + block.len());
            out.push_str(&existing[..start]);
            out.push_str(&block);
            out.push_str(after);

            let action = if out == existing {
                Action::Unchanged
            } else {
                Action::Replaced
            };
            return (out, action);
        }
        // Begin marker without end marker: file is malformed; refuse to guess.
        // Append a fresh block; user can clean up the dangling marker.
    }

    if existing.is_empty() {
        return (block, Action::Created);
    }

    let needs_blank = !existing.ends_with('\n');
    let mut out = String::with_capacity(existing.len() + block.len() + 2);
    out.push_str(existing);
    if needs_blank {
        out.push('\n');
    }
    if !existing.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str(&block);
    (out, Action::Inserted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_when_empty() {
        let (out, action) = compute_new_content("");
        assert_eq!(action, Action::Created);
        assert!(out.contains(BEGIN_MARK));
        assert!(out.contains(END_MARK));
        assert!(out.contains("claude()"));
    }

    #[test]
    fn appends_when_no_block() {
        let original = "export PATH=/usr/bin:$PATH\n";
        let (out, action) = compute_new_content(original);
        assert_eq!(action, Action::Inserted);
        assert!(out.starts_with("export PATH=/usr/bin:$PATH"));
        assert!(out.contains(BEGIN_MARK));
    }

    #[test]
    fn unchanged_when_block_matches() {
        let (after_first, _) = compute_new_content("# user line\n");
        let (after_second, action) = compute_new_content(&after_first);
        assert_eq!(action, Action::Unchanged);
        assert_eq!(after_first, after_second);
    }

    #[test]
    fn replaces_existing_block() {
        let stale = format!("user line\n{BEGIN_MARK}\nold body\n{END_MARK}\ntrailing line\n",);
        let (out, action) = compute_new_content(&stale);
        assert_eq!(action, Action::Replaced);
        assert!(out.contains("user line\n"));
        assert!(out.contains("trailing line\n"));
        assert!(!out.contains("old body"));
        assert!(out.contains("claude()"));
    }
}
