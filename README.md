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
ghnotify install        # writes the claude() shell wrapper to ~/.bashrc
```

`ghnotify install` is idempotent — re-running updates the managed block in
place. Use `--shell zsh` to target `~/.zshrc`, `--rc <path>` for a custom file,
or `--dry-run` to preview the change without writing.

The wrapper makes every `claude` invocation land inside a tmux session named
`claude-<repo>`, which is the address `ghnotify serve` uses to route incoming
webhook events. Open a new terminal (or `source ~/.bashrc`) for it to take
effect.

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

- [x] `ghnotify install` subcommand: writes the `claude()` wrapper into
      `~/.bashrc` / `~/.zshrc` between managed markers.
- [ ] Embed `gh webhook forward` so a single `ghnotify watch` process handles
      both the GitHub relay and the tmux dispatch (no separate daemon needed).
- [ ] Per-event-type prompt templates (CI failure → "fix it", review done →
      "address comments").
- [ ] Pre-built binaries on Releases for Linux/macOS/Windows.

## Retry semantics

ghnotify intentionally does **not** queue or retry. When a webhook arrives:

- Target tmux session exists → prompt is delivered, response is `200 {ok:true}`.
- Target session is missing → event is discarded, response is `200 {discarded:true}`.

This matches what's upstream of us anyway: `gh webhook forward` does not retry
on local non-2xx, and GitHub itself does not auto-retry repository webhooks
(failed deliveries sit in the 30-day delivery history for manual replay via
the REST API). There's nothing to gain from buffering events on our side.

## License

MIT
