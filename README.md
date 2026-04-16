# ghnotify

Cross-platform single-binary GitHub webhook → Claude Code session forwarder.

When a GitHub webhook arrives (CI complete, code review done, deploy result, etc.),
ghnotify delivers a prompt directly into the Claude Code session running for that
repo, via `tmux send-keys`. No MCP channels, no kernel hacks, no per-session flags.

## Why

Claude Code sessions are long-lived REPLs in your terminal. Anthropic's MCP
Channels feature (`notifications/claude/channel`) silently drops events on
non-allowlisted local servers, even with `--dangerously-load-development-channels`
enabled (verified empirically on claude-code 2.1.111). The official "push prompt
into running session" path is broken for self-hosted setups.

ghnotify takes the same approach as
[agent-of-empires](https://github.com/njbrake/agent-of-empires) (1.6k★) and
[Claude-Code-Remote](https://github.com/JessyTsui/Claude-Code-Remote) (1.2k★):
each Claude session lives inside a tmux session named `claude-<repo>`, and
external events route in via `tmux send-keys`.

## Install

```bash
cargo install --git https://github.com/Olbrasoft/ghnotify
```

You also need a `claude()` shell wrapper that puts every Claude session into a
tmux pane named `claude-<repo>`. Add this to `~/.bashrc` (or `~/.zshrc`):

```bash
claude() {
    if [ -n "$TMUX" ]; then
        command claude "$@"
        return
    fi
    for arg in "$@"; do
        case "$arg" in
            -p|--print|--version|--help|-h|-v) command claude "$@"; return ;;
        esac
    done
    local name root
    if root=$(git rev-parse --show-toplevel 2>/dev/null); then
        name="claude-$(basename "$root")"
    else
        name="claude-home"
    fi
    name="${name//./-}"
    if tmux has-session -t "$name" 2>/dev/null; then
        exec tmux attach -t "$name"
    fi
    exec tmux new-session -s "$name" -- claude "$@"
}
```

After that, `claude --continue` inside any repo automatically wraps in a tmux
session whose name ghnotify can address.

## Use

### Verify the setup

```bash
ghnotify doctor
ghnotify list   # shows all claude-* tmux sessions
```

### Send a one-shot prompt

```bash
ghnotify send --repo GitHub.Issues --prompt "What's the status of PR #123?"
# → tmux session "claude-GitHub-Issues" receives the prompt and Claude responds.
```

### Run the webhook receiver

```bash
ghnotify serve   # listens on 127.0.0.1:9877
```

Point a `gh webhook forward` (or any GitHub webhook destination) at
`http://127.0.0.1:9877/webhook`. ghnotify reads `repository.name` from the
payload and dispatches to `claude-<name>`.

```bash
gh webhook forward \
    --repo Olbrasoft/GitHub.Issues \
    --events '*' \
    --url http://127.0.0.1:9877/webhook
```

## Config

Optional `./ghnotify.toml` or `$XDG_CONFIG_HOME/ghnotify/config.toml`:

```toml
[server]
bind = "127.0.0.1:9877"

[github]
# If set, X-Hub-Signature-256 is verified against this secret.
# Leave unset for `gh webhook forward` (no signature in that path).
webhook_secret = "abc..."
```

## Status

MVP — basic plumbing works. Roadmap:

- [ ] `ghnotify install` subcommand: writes the `claude()` wrapper and
      registers the launcher.
- [ ] Per-event-type prompt templates (CI failure → "fix it", review done →
      "address comments").
- [ ] Pre-built binaries on Releases for Linux/macOS/Windows.

## License

MIT
