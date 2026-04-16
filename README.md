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

### Run the all-in-one daemon

```bash
ghnotify watch                                     # auto-discover repos
ghnotify watch --repo owner/foo --repo owner/bar   # explicit list
```

`watch` is the recommended way to run ghnotify. In a single process it:

1. Cleans up any zombie forwarder hooks on each target repo.
2. Spawns one `gh webhook forward` subprocess per repo (so GitHub can reach you).
3. Runs the local HTTP receiver and dispatches incoming events to the matching
   `claude-<repo>` tmux session via `tmux send-keys`.

Auto-discovery walks `/proc` for running `claude` processes and reads each
one's git remote — Linux only. On macOS/Windows pass `--repo` explicitly.
`gh` must be on `PATH` and authenticated (`gh auth status`).

Defaults to forwarding `pull_request_review,check_suite,workflow_run`. Override
with `--events '*'` for everything, or any comma-separated subset.

### Lower-level: receiver only

```bash
ghnotify serve   # listens on 127.0.0.1:9877, no gh subprocess
```

Use this when you already manage `gh webhook forward` (or another webhook
source) yourself. Point it at `http://127.0.0.1:9877/webhook`.

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
- [x] `ghnotify watch`: spawns `gh webhook forward` per repo *and* runs the
      receiver in one process. Auto-discovers repos from active `claude`
      processes (Linux); pass `--repo` on other platforms.
- [ ] Native WebSocket relay client to drop `gh` as a runtime dep (today
      `watch` shells out to `gh webhook forward`).
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
# trailing newline test
# watchdog E2E test trigger at 21:15:37Z
