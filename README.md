# ghnotify

Cross-platform single-binary GitHub webhook → agent-session forwarder.
Supports both **Claude Code** and **Codex** sessions running side-by-side.

When a GitHub webhook arrives (CI complete, code review done, deploy result, etc.),
ghnotify delivers a prompt directly into the agent session that owns the PR —
Claude or Codex — via `tmux send-keys`. No MCP channels, no kernel hacks, no
per-session flags.

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

## Codex setup (optional)

ghnotify also routes wakes into Codex sessions. The conventions mirror
Claude's: tmux session names start with `codex-`, PR bodies carry a
`<!-- codex-session: $UUID -->` marker, and a pid-index file under
`~/.codex/sessions/pids/<pid>.json` enables tier-1 (pid-walk) routing
alongside tier-2 (cwd-basename) fallback.

`ghnotify install` only writes the Claude wrapper; Codex setup is currently
manual. Four pieces — the first three are required for routing to work at
all, the fourth (pid index) is an optional precision upgrade:

**1. Bash wrapper** for tmux sessioning, in `~/.bashrc`:

```bash
codex() {
    if [ -n "$TMUX" ]; then command codex "$@"; return; fi
    case "${1:-}" in exec) command codex "$@"; return ;; esac
    local root base tty_dev tty_id name
    if root=$(git rev-parse --show-toplevel 2>/dev/null); then
        base="$(basename "$root")"
    else
        base="$(basename "$PWD")"
    fi
    tty_dev=$(tty 2>/dev/null)
    tty_id="${tty_dev#/dev/}"; tty_id="${tty_id//\//-}"
    name="codex-${base}-${tty_id}"; name="${name//./-}"
    if tmux has-session -t "$name" 2>/dev/null; then tmux attach -t "$name"; return; fi
    tmux new-session -s "$name" -- codex "$@"
}
```

**2. PR marker convention** — instruct Codex (in `~/.codex/AGENTS.md`) that
every `gh pr create` MUST start the PR body with:

```
<!-- codex-session: $SESSION_ID -->
```

where `$SESSION_ID` is the current Codex session id (the `payload.id` from the
session's `rollout-*.jsonl`). Without the marker, ghnotify falls back to repo-
prefix routing, which works fine for Codex-only repos but can misroute when a
Codex session in repo A opens a PR on repo B.

**3. Helper script** at `~/.codex/get-session-id.sh` that emits the current
session id on stdout. The recommended approach: walk up the parent process
tree to find the `codex` pid, then pick the most-recently-modified file
matching `~/.codex/sessions/*/*/*/rollout-*-<uuid>.jsonl` for that process's
cwd-basename, extracting the UUID from the filename. Mirror of
`~/.claude/hooks/get-session-id.sh`.

**4. Pid index (tier-1 routing)** — optional but recommended. ghnotify reads
`~/.codex/sessions/pids/<pid>.json` (same shape as
`~/.claude/sessions/<pid>.json`: `{"pid":N,"sessionId":"...","cwd":"..."}`) to
disambiguate two Codex sessions sharing a cwd. The simplest way to keep it
fresh is from your Codex `notify` callback (`~/.codex/codex-notify.sh`) on
agent-turn-complete; the JSON arg carries `thread-id` (sessionId) and `cwd`,
and `$PPID` is the Codex CLI process. Without the pid index, ghnotify falls
back to tier-2 (cwd-basename of the rollout JSONL's `session_meta.payload.cwd`)
— ambiguous only when two Codex sessions share a cwd.

## Use

### Verify the setup

```bash
ghnotify doctor
ghnotify list   # shows all claude-* and codex-* tmux sessions
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
# E2E verify after watchdog confirmed @ 21:20:53Z

<!-- test marker: round-2 positive/negative classifier verification (do not delete in PR; removed on merge) -->
