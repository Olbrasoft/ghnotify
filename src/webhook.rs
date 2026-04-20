//! HTTP webhook receiver. Accepts GitHub-style JSON webhooks at `POST /webhook`,
//! resolves the target Claude session, and dispatches a prompt via tmux.
//!
//! GitHub event payload shape (subset we care about):
//!   { "repository": { "name": "GitHub.Issues", "full_name": "Olbrasoft/GitHub.Issues" }, ... }
//!
//! Per-request pipeline, in order:
//!   1. HMAC signature verification (when `github.webhook_secret` is set).
//!   2. Classify the event into a prompt or drop reason ([`event::classify`]).
//!   3. For `check_suite` forwards only: query every check_suite on the
//!      head SHA via `gh api` and defer unless all are completed; when
//!      all are done, rebuild the prompt with the *aggregate* conclusion
//!      so a green triggering event with a red sibling still produces a
//!      `ci-failure` wake ([`refine_ci_decision`]).
//!   4. Resolve the target tmux session (see below).
//!   5. Dispatch via `tmux send-keys -l`.
//!
//! Session resolution order:
//!   1. **By author UUID from the payload** — `pull_request`,
//!      `pull_request_review`, `pull_request_review_comment` and
//!      `issue_comment` payloads carry the PR/issue body inline.
//!      Extract `<!-- claude-session: UUID -->` (embedded by `gh pr
//!      create` per the global CLAUDE.md convention) and route the
//!      wake to the session that authored the PR, regardless of the
//!      event's repo. This is the *only* correct answer when a session
//!      runs in one directory but opens PRs in another (e.g. imdb
//!      session → cr PR).
//!   2. **By author UUID via `gh api`** — `check_suite` payloads
//!      don't carry PR bodies, so we resolve them via `gh`. See
//!      [`uuid_from_check_suite`] for the three payload shapes
//!      covered (branch-named, synthetic-ref, post-merge default-
//!      branch via commit → PR lookup). Without the commit-lookup
//!      path, post-merge `ci-success branch=main pr=none` wakes fell
//!      into repo-based routing and landed on whichever session had
//!      the repo's cwd open, even when that session had nothing to
//!      do with the PR. All calls are gated on a short timeout so a
//!      stuck `gh` (auth prompt, network stall) can't jam the
//!      webhook handler.
//!   3. **By repo name** — fall back to the prefix match on
//!      `claude-<repo>` when the marker is absent, the session is
//!      dead, or any of the lookups above fail. Preserves legacy
//!      behavior for non-PR events (`ping`, `issues assigned`) and
//!      for PRs created before the marker convention existed.
//!
//! Runs in two modes:
//!   * persistent — binds an address and serves forever (`ghnotify serve`)
//!   * one-shot   — serves exactly one request then exits (`ghnotify serve --one-shot`),
//!     intended to be run per-connection under systemd socket activation so
//!     nothing is running between webhook deliveries.

use crate::{config::Config, event, session_by_uuid, session_marker, sessions, tmux};
use anyhow::{Context, Result};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use hmac::{Hmac, Mac};
use listenfd::ListenFd;
use serde_json::Value;
use sha2::Sha256;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    /// If set, the request handler fires this sender once a response is
    /// produced so the axum serve loop can shut down after one request.
    shutdown_once: Option<Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>>,
}

pub async fn serve(cfg: Config, bind_override: Option<String>, one_shot: bool) -> Result<()> {
    let listener = acquire_listener(&cfg, bind_override.as_deref()).await?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let state = AppState {
        cfg: Arc::new(cfg),
        shutdown_once: if one_shot {
            Some(Arc::new(Mutex::new(Some(shutdown_tx))))
        } else {
            None
        },
    };

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/webhook", post(handle_webhook))
        .with_state(state);

    if one_shot {
        info!("one-shot mode: will exit after one request");
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await?;
    } else {
        axum::serve(listener, app).await?;
    }
    Ok(())
}

/// Prefer a listening socket passed by the init system (systemd socket
/// activation via `LISTEN_FDS`). Fall back to binding a TCP address.
async fn acquire_listener(
    cfg: &Config,
    bind_override: Option<&str>,
) -> Result<tokio::net::TcpListener> {
    let mut lf = ListenFd::from_env();
    if let Some(std_listener) = lf
        .take_tcp_listener(0)
        .context("reading systemd LISTEN_FDS")?
    {
        std_listener.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(std_listener)?;
        let local = listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "<systemd socket>".into());
        info!(listener = %local, "ghnotify listening (socket-activated)");
        return Ok(listener);
    }
    let bind = bind_override
        .map(str::to_owned)
        .unwrap_or_else(|| cfg.server.bind.clone());
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    info!(bind = %bind, "ghnotify listening");
    Ok(listener)
}

async fn handle_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let result = process_webhook(&state, &headers, &body).await;
    if let Some(once) = &state.shutdown_once {
        let mut guard = once.lock().await;
        if let Some(tx) = guard.take() {
            let _ = tx.send(());
        }
    }
    result
}

async fn process_webhook(
    state: &AppState,
    headers: &HeaderMap,
    body: &Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    // 1. Optional HMAC signature verification.
    if let Some(secret) = state.cfg.github.webhook_secret.as_deref() {
        match verify_signature(secret, headers, body) {
            Ok(()) => {}
            Err(reason) => {
                warn!(reason, "rejecting webhook: bad signature");
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({ "error": "bad signature" })),
                );
            }
        }
    }

    // 2. Parse payload as a generic JSON value (the classifier reads many fields).
    let payload: serde_json::Value = match serde_json::from_slice(body) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "webhook body is not valid JSON");
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "invalid json" })),
            );
        }
    };

    let event_type = headers
        .get("X-GitHub-Event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");

    let repo_name = payload
        .pointer("/repository/name")
        .and_then(serde_json::Value::as_str);
    let Some(repo_name) = repo_name else {
        info!(event_type, "webhook with no repository field, ignored");
        return (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "ignored": true })),
        );
    };

    // 3. Classify: should we forward this event at all, and as what prompt?
    //    The single-event prompt produced here may be superseded by the
    //    aggregate-refined prompt below for `check_suite` events.
    let single_event_prompt = match event::classify(
        event_type,
        &payload,
        repo_name,
        &state.cfg.github.own_logins,
    ) {
        event::Decision::Forward { prompt } => prompt,
        event::Decision::Drop { reason } => {
            let action = payload
                .get("action")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            info!(
                event_type,
                repo = repo_name,
                action,
                reason,
                "event dropped by filter"
            );
            return (
                StatusCode::OK,
                Json(serde_json::json!({
                    "ok": true,
                    "filtered": true,
                    "reason": reason,
                })),
            );
        }
    };

    // 3a. For check_suite wakes, defer until every check_suite on the head
    //     SHA is complete so a single App (typically GitGuardian, which
    //     tends to finish seconds ahead of the real CI suite) can't fire
    //     a premature `ci-success`. Fails open to `single_event_prompt`
    //     on gh errors — better a possibly-early wake than a
    //     silently-dropped one.
    let prompt = match event_type {
        "check_suite" => match refine_ci_decision(&payload, repo_name).await {
            CiRefinement::ForwardAs(aggregate_prompt) => aggregate_prompt,
            CiRefinement::Defer { reason } => {
                info!(
                    event_type,
                    repo = repo_name,
                    reason,
                    "ci wake deferred (aggregate not ready)"
                );
                return (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "ok": true,
                        "deferred": true,
                        "reason": reason,
                    })),
                );
            }
            CiRefinement::FailOpen => single_event_prompt,
        },
        _ => single_event_prompt,
    };

    // 4. Resolve target session. UUID-based first (correct across repos),
    //    repo-prefix fallback (legacy behavior for events we can't
    //    attribute to a specific author session).
    let base = tmux::session_name_for_repo(repo_name);
    let (session, via) = match resolve_target_session(event_type, &payload, repo_name).await {
        Ok(Some(resolved)) => resolved,
        Ok(None) => {
            info!(
                base,
                event_type,
                "no claude session found (neither author UUID nor repo), event discarded"
            );
            return (
                StatusCode::OK,
                Json(serde_json::json!({
                    "ok": true,
                    "discarded": true,
                    "reason": "no matching claude session",
                    "session": base,
                })),
            );
        }
        Err(e) => {
            error!(error = %e, "session resolution failed");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
            );
        }
    };
    match tmux::send_prompt(&session, &prompt) {
        Ok(tmux::Delivery::Delivered) => {
            info!(session, via, event_type, "prompt delivered");
            (
                StatusCode::OK,
                Json(serde_json::json!({ "ok": true, "session": session, "via": via })),
            )
        }
        // Session vanished between list and send (rare race). Treat as soft
        // discard for the same reason the caller-less case is a soft discard.
        Ok(tmux::Delivery::NoSession) => {
            info!(
                session,
                via, event_type, "session disappeared before send, event discarded"
            );
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "ok": true,
                    "discarded": true,
                    "reason": "session disappeared before send",
                    "session": session,
                    "via": via,
                })),
            )
        }
        Err(e) => {
            error!(session, via, error = %e, "tmux send_prompt failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
            )
        }
    }
}

/// Try author-UUID routing first, then fall back to repo-name routing.
///
/// Returns `Ok(Some((session, "uuid"|"repo")))` on match, `Ok(None)` when
/// nothing matches (soft discard), `Err` only for unexpected I/O failures
/// that should surface as 503 rather than be silently swallowed.
async fn resolve_target_session(
    event_type: &str,
    payload: &Value,
    repo_name: &str,
) -> Result<Option<(String, &'static str)>> {
    // A — UUID embedded directly in the webhook payload. Covers
    // pull_request, pull_request_review, pull_request_review_comment
    // and issue_comment (on PRs).
    if let Some(uuid) = uuid_from_payload(event_type, payload) {
        if let Some(name) = try_resolve_uuid(&uuid, event_type, "payload") {
            return Ok(Some((name, "uuid")));
        }
    }

    // B — check_suite payloads don't include the PR body, only PR numbers.
    // Fetch the body via `gh api` so we can still do author-routing for
    // CI wakes, which the user expects to land on whoever pushed the
    // commit (not on "whichever session happens to have this repo open").
    if event_type == "check_suite" {
        if let Some(uuid) = uuid_from_check_suite(payload).await {
            if let Some(name) = try_resolve_uuid(&uuid, event_type, "check_suite/gh") {
                return Ok(Some((name, "uuid")));
            }
        }
    }

    // C — repo-prefix fallback. Preserves legacy behavior for events
    // without a marker (ping, issues assigned, issue_comment on non-PR
    // issues) and as a safety net when the author session is dead.
    match sessions::resolve_session_for_repo(repo_name)? {
        Some(name) => Ok(Some((name, "repo"))),
        None => Ok(None),
    }
}

/// Extract the claude-session marker from fields directly present in the
/// webhook payload. Returns `None` for event types whose payloads don't
/// carry a PR/issue body, or when no marker is present in the body.
fn uuid_from_payload(event_type: &str, payload: &Value) -> Option<String> {
    let body = match event_type {
        // `pull_request` events carry the PR body at /pull_request/body.
        // `pull_request_review` and `pull_request_review_comment` also
        // include the full `pull_request` object for context.
        "pull_request" | "pull_request_review" | "pull_request_review_comment" => {
            payload.pointer("/pull_request/body")
        }
        // issue_comment on a PR has `issue.pull_request` set and
        // `issue.body` carries the PR body. For non-PR issues, the body
        // is just the issue body (very rarely has a claude-session
        // marker), but trying is harmless.
        "issue_comment" => payload.pointer("/issue/body"),
        _ => None,
    }?
    .as_str()?;
    session_marker::extract_uuid(body)
}

/// Outcome of aggregate-refining a `check_suite` wake.
enum CiRefinement {
    /// Aggregate is final — forward this rebuilt prompt (with the
    /// aggregate conclusion, which can differ from the triggering
    /// event's conclusion when a sibling suite failed).
    ForwardAs(String),
    /// At least one sibling suite on the head SHA is still running —
    /// drop this wake so the final suite's completion fires a single
    /// canonical wake.
    Defer { reason: &'static str },
    /// The aggregate lookup itself failed (no `gh`, network stall,
    /// malformed payload, empty response). Fall through to the
    /// single-event prompt so a broken lookup can't silently swallow
    /// every CI wake.
    FailOpen,
}

/// Query the aggregate check_suite state for the head SHA of this
/// webhook's check_suite event and decide whether to forward, defer,
/// or fail open.
///
/// GitHub fires a separate `check_suite completed` webhook per App
/// installed on the repo (main CI, GitGuardian, Copilot reviewer, …).
/// Treating each as a terminal CI wake fires `ci-success` the moment
/// the *first* App's suite finishes. This helper calls
/// `gh api repos/OWNER/NAME/commits/SHA/check-suites`, merges every
/// suite's `(status, conclusion)` via [`event::compute_ci_aggregate`],
/// and rebuilds the prompt with the aggregate conclusion (via
/// [`event::build_ci_prompt`]) so a green triggering event with a red
/// sibling escalates to `ci-failure` rather than falsely reporting
/// success.
async fn refine_ci_decision(payload: &Value, repo_name: &str) -> CiRefinement {
    let info = event::extract_ci_info(payload, repo_name);
    if info.head_sha.is_empty() || info.full_name.is_empty() {
        // Malformed payload — we can't even query. Fall through so the
        // single-event prompt still reaches the session.
        return CiRefinement::FailOpen;
    }
    // Settle GitHub's eventual consistency before aggregating. On a
    // fresh push the App that finishes first (typically GitGuardian)
    // can fire its `check_suite completed` webhook before the other
    // Apps' check_suites have even been inserted into the
    // `check-suites` response — so an immediate query returns
    // `[gitguardian=success]` alone, the aggregate shows AllSuccess,
    // and `ci-success` fires while real CI hasn't started. A short
    // sleep covers the worst case observed on cr/ghnotify (creation
    // lag < 3 s) with margin, without materially delaying the wake —
    // systemd allowed up to 10 s for the webhook response already, and
    // the aggregate path was never the fast case anyway.
    tokio::time::sleep(std::time::Duration::from_secs(GH_AGGREGATE_SETTLE_SECS)).await;
    let suites = match fetch_check_suites(&info.full_name, &info.head_sha).await {
        Some(s) => s,
        None => return CiRefinement::FailOpen,
    };
    // An *empty* list from a successful gh call is genuinely ambiguous
    // (eventual consistency, commit not yet propagated, jq shape
    // change) — not the same as "every suite still in_progress". If
    // we deferred on empty, no later event would re-trigger and the
    // whole wake would vanish. Fail open to the single-event prompt
    // instead so the session still hears about the completed suite
    // that brought us here.
    if suites.is_empty() {
        warn!(
            sha = info.head_sha,
            repo = info.full_name,
            "gh check-suites returned empty list; failing open to single-event prompt"
        );
        return CiRefinement::FailOpen;
    }
    // Drop check_suites that the owning App created but has abandoned
    // while other Apps made progress. Post-merge main pushes hit this
    // when an App scoped to pull_request events (GitGuardian is the
    // concrete case) still registers a suite for the push but never
    // transitions it; aggregating those as InProgress would wedge the
    // main-branch wake indefinitely.
    //
    // The filter compares each queued-and-untouched suite against the
    // *newest-completed* suite on the same commit. Only when another
    // App has completed a whole workflow AFTER this one was last
    // touched do we conclude it's abandoned — a freshly-minted queued
    // suite on a PR still has `updated_at ≥ newest_completed` because
    // both fall inside the same push window, so it stays active and
    // keeps the aggregate honest.
    //
    // If every suite is stuck-queued, fail open rather than defer: the
    // triggering event is a real completion that deserves a wake, even
    // if we can't verify aggregate state against a sea of abandoned
    // suites.
    let active: Vec<&CheckSuiteEntry> = suites.iter().filter(|s| !s.is_stuck_queued()).collect();
    if active.is_empty() {
        warn!(
            sha = info.head_sha,
            repo = info.full_name,
            total_suites = suites.len(),
            "every check_suite on head SHA is stuck-queued; failing open"
        );
        return CiRefinement::FailOpen;
    }
    let aggregate = event::compute_ci_aggregate(
        active
            .iter()
            .map(|s| (s.status.as_str(), s.conclusion.as_deref())),
    );
    match aggregate {
        event::CiAggregate::InProgress => CiRefinement::Defer {
            reason: "sibling check_suite on head SHA not yet completed",
        },
        event::CiAggregate::AllSuccess => {
            CiRefinement::ForwardAs(event::build_ci_prompt(&info, "success"))
        }
        event::CiAggregate::HasFailure => {
            // "failure" is the canonical single-word conclusion; the
            // ci_next_action_hint branches on `!= "success"` only, so the
            // exact token doesn't matter for the hint, but we keep it
            // stable for operators grepping `status=` in the journal.
            CiRefinement::ForwardAs(event::build_ci_prompt(&info, "failure"))
        }
    }
}

/// Entry in the aggregate check_suites list for a commit.
///
/// `age_s` is computed in the gh query via jq's `now -
/// (.updated_at | fromdateiso8601)` — the wall-clock age in seconds
/// of the suite's last transition. Combined with
/// `latest_check_runs_count` it lets us tell a queued suite GitHub
/// is about to pick up from one the owning App has abandoned.
#[derive(serde::Deserialize)]
struct CheckSuiteEntry {
    status: String,
    conclusion: Option<String>,
    latest_check_runs_count: u64,
    age_s: f64,
}

/// Wait this long before querying `gh api … /check-suites`. Lets
/// GitHub finish creating the other Apps' check_suites for the
/// commit so the aggregate response is complete. See the long
/// comment at the only call site in [`refine_ci_decision`].
const GH_AGGREGATE_SETTLE_SECS: u64 = 5;

/// Seconds a queued suite with zero check_runs may sit before we
/// conclude the owning App has abandoned it. Tuned to exceed the
/// observed worker-pickup delay for active Apps (≈ 1–5 s in practice,
/// < 15 s even for slow ones) while still being short enough that
/// main-branch post-merge wakes — where `github-actions` typically
/// completes in ~30 s and GitGuardian never produces a check_run —
/// clear the threshold when the real suite's completion event fires.
/// Below this, we keep the suite in the aggregate and let the next
/// event carry the wake; above it with check_runs still 0, the App
/// is treated as having dropped the suite on the floor.
const STUCK_QUEUED_AGE_THRESHOLD_S: f64 = 25.0;

impl CheckSuiteEntry {
    /// Detect a check_suite the owning App has abandoned. Requires all of:
    /// * `status == "queued"` — anything further is making progress.
    /// * `latest_check_runs_count == 0` — the App has not created so
    ///   much as a single check_run, meaning no worker has picked it up.
    ///   Once an App starts, a check_run exists immediately.
    /// * `age_s > STUCK_QUEUED_AGE_THRESHOLD_S` — a legitimately
    ///   newly-minted queued suite on a fresh PR push is only a few
    ///   seconds old when the first peer completes; a truly abandoned
    ///   one (GitGuardian on a default-branch push) has been sitting
    ///   since the commit was pushed while another App ran its whole
    ///   workflow.
    ///
    /// All three guards are necessary. `status=queued` alone would
    /// filter freshly-minted suites that are about to start. A pure
    /// age guard would filter slow-to-pick-up active suites that
    /// already have check_runs recorded. Requiring every condition
    /// keeps the filter conservative enough that we don't re-introduce
    /// the premature-wake bug on fast PRs.
    fn is_stuck_queued(&self) -> bool {
        self.status == "queued"
            && self.latest_check_runs_count == 0
            && self.age_s > STUCK_QUEUED_AGE_THRESHOLD_S
    }
}

/// Call `gh api repos/OWNER/NAME/commits/SHA/check-suites` and return
/// the `check_suites[]` array as `CheckSuiteEntry`s. Each failure mode
/// (spawn error, timeout, non-zero exit, unparseable JSON) emits a
/// warning and returns `None` so the caller can fail open.
async fn fetch_check_suites(full_name: &str, head_sha: &str) -> Option<Vec<CheckSuiteEntry>> {
    let spawn = tokio::process::Command::new("gh")
        .args([
            "api",
            &format!("repos/{full_name}/commits/{head_sha}/check-suites"),
            "--jq",
            // `age_s` is computed here, not in Rust, so the filter
            // measures age at the exact moment gh observed the API
            // response. jq's `now` is the process's wall clock;
            // `fromdateiso8601` parses GitHub's RFC 3339 `updated_at`.
            "[.check_suites[] | {status, conclusion, latest_check_runs_count, age_s: (now - (.updated_at | fromdateiso8601))}]",
        ])
        .kill_on_drop(true)
        .output();
    let out = match tokio::time::timeout(GH_API_TIMEOUT, spawn).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            warn!(
                error = %e,
                sha = head_sha,
                repo = full_name,
                "failed to spawn `gh` for check_suite aggregate query"
            );
            return None;
        }
        Err(_) => {
            warn!(
                timeout_secs = GH_API_TIMEOUT.as_secs(),
                sha = head_sha,
                repo = full_name,
                "`gh api` timed out during check_suite aggregate query; killed"
            );
            return None;
        }
    };
    if !out.status.success() {
        warn!(
            status = %out.status,
            stderr = %String::from_utf8_lossy(&out.stderr).trim(),
            sha = head_sha,
            repo = full_name,
            "gh api commits/SHA/check-suites failed"
        );
        return None;
    }
    match serde_json::from_slice::<Vec<CheckSuiteEntry>>(&out.stdout) {
        Ok(v) => Some(v),
        Err(e) => {
            warn!(
                error = %e,
                sha = head_sha,
                repo = full_name,
                "parsing gh check-suites response failed"
            );
            None
        }
    }
}

/// Upper bound on how long we'll wait for `gh api` when fetching a PR
/// body for a `check_suite` event. Generous enough to absorb normal
/// GitHub tail latency (p99 of `gh api pulls/N` is typically < 1 s) yet
/// short enough that a stuck gh call can't jam the webhook handler:
/// GitHub considers a webhook delivery failed after 10 s and will
/// retry, which is preferable to a handler that never returns.
const GH_API_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Extract the PR number from the check_suite's `pull_requests` array.
/// Branch-named `check_suite completed` events carry this — synthetic
/// refs often do not.
fn pr_number_from_payload(payload: &Value) -> Option<u64> {
    payload
        .pointer("/check_suite/pull_requests")
        .and_then(Value::as_array)?
        .iter()
        .find_map(|pr| pr.get("number")?.as_u64())
}

/// Parse `N` out of a `head_branch` like `refs/pull/536/head` or
/// `refs/pull/536/merge`. Exactly-shaped to rule out user-authored
/// branches that happen to share the prefix (`refs/pull/foo/bar`,
/// `refs/pull/4/patch`, `refs/pulls/…`). Used as a fallback when the
/// check_suite payload's `pull_requests` array is empty, which is the
/// signature of GitHub's synthetic-ref dispatch.
fn pr_number_from_head_branch(payload: &Value) -> Option<u64> {
    let head_branch = payload
        .pointer("/check_suite/head_branch")
        .and_then(Value::as_str)?;
    let rest = head_branch.strip_prefix("refs/pull/")?;
    let (num, tail) = rest.split_once('/')?;
    if !matches!(tail, "head" | "merge") {
        return None;
    }
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    num.parse().ok()
}

/// Fetch the PR body for the PR referenced by a check_suite event
/// and extract the claude-session marker from it.
///
/// Covers three payload shapes:
///   * **Branch-named PR event** — `pull_requests[]` has the PR
///     number. Fetch body via `/pulls/N`.
///   * **Synthetic-ref event** — `head_branch=refs/pull/<N>/(head|
///     merge)` carries the number even when `pull_requests[]` is
///     empty. Fetch body via `/pulls/N`.
///   * **Post-merge main event** — `head_branch=main` (or master /
///     trunk), `pull_requests[]=[]`, no PR number in either of the
///     above. Fetch the body of the *originating* PR via
///     `/commits/SHA/pulls` (GitHub still tracks which PR a squash/
///     merge commit came from), so the post-merge wake routes back
///     to the session that opened the PR instead of falling into
///     repo-based routing and landing on whichever session happens
///     to have the repo's cwd open. Reproduced on cr after
///     merging #535/#536 — the `ci-success branch=main pr=none`
///     wakes kept arriving at `claude-cr-pts-4` even though that
///     session wasn't involved.
///
/// Requires `gh` to be on PATH and authenticated. Returns `None` on any
/// failure (spawn error, non-zero exit, empty body, timeout, missing
/// marker) — the caller falls back to repo-based routing. Each failure
/// mode emits a warning so UUID-routing regressions are diagnosable
/// from the journal; silent `None`s used to hide "gh not on PATH"
/// behind a generic repo-routing fallback.
async fn uuid_from_check_suite(payload: &Value) -> Option<String> {
    let full_name = payload
        .pointer("/repository/full_name")
        .and_then(Value::as_str)?;
    let body = if let Some(number) =
        pr_number_from_payload(payload).or_else(|| pr_number_from_head_branch(payload))
    {
        fetch_pr_body_by_number(full_name, number).await?
    } else {
        let sha = payload
            .pointer("/check_suite/head_sha")
            .and_then(Value::as_str)?;
        fetch_pr_body_by_commit(full_name, sha).await?
    };
    session_marker::extract_uuid(&body)
}

/// `gh api repos/OWNER/NAME/pulls/N --jq .body`. Returns the body on
/// success, `None` on any failure (with a journal warning for each
/// failure mode). Factored out of `uuid_from_check_suite` so both the
/// direct-number and commit-lookup paths share the same subprocess
/// handling (timeout + kill_on_drop).
async fn fetch_pr_body_by_number(full_name: &str, number: u64) -> Option<String> {
    run_gh_for_body(
        &[
            "api",
            &format!("repos/{full_name}/pulls/{number}"),
            "--jq",
            ".body",
        ],
        full_name,
        &format!("pr={number}"),
    )
    .await
}

/// `gh api repos/OWNER/NAME/commits/SHA/pulls --jq '.[0].body'`. Used
/// when the webhook payload has no PR number — post-merge default-branch
/// events, where the PR was squashed/merged and only the new merge
/// commit SHA is in the event. GitHub's API still associates the PR
/// with the resulting commit, so this recovers the original PR body
/// (and thus the claude-session marker) for author routing.
async fn fetch_pr_body_by_commit(full_name: &str, sha: &str) -> Option<String> {
    run_gh_for_body(
        &[
            "api",
            &format!("repos/{full_name}/commits/{sha}/pulls"),
            "--jq",
            ".[0].body",
        ],
        full_name,
        &format!("sha={sha}"),
    )
    .await
}

/// Spawn `gh` with the supplied args, enforce [`GH_API_TIMEOUT`] with
/// `kill_on_drop`, and return stdout trimmed. `context` is a short
/// identifier (e.g. `pr=536` or `sha=abc12345`) included in warnings
/// so a journal-tail diagnoses UUID-routing regressions without
/// having to reconstruct which call failed.
async fn run_gh_for_body(args: &[&str], repo: &str, context: &str) -> Option<String> {
    let spawn = tokio::process::Command::new("gh")
        .args(args)
        .kill_on_drop(true)
        .output();
    let out = match tokio::time::timeout(GH_API_TIMEOUT, spawn).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            warn!(
                error = %e,
                repo,
                context,
                "failed to spawn `gh` for check_suite UUID resolution"
            );
            return None;
        }
        Err(_) => {
            warn!(
                timeout_secs = GH_API_TIMEOUT.as_secs(),
                repo, context, "`gh api` timed out during check_suite UUID resolution; killed"
            );
            return None;
        }
    };
    if !out.status.success() {
        warn!(
            status = %out.status,
            stderr = %String::from_utf8_lossy(&out.stderr).trim(),
            repo,
            context,
            "`gh api` failed during UUID resolution"
        );
        return None;
    }
    let body = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if body.is_empty() {
        None
    } else {
        Some(body)
    }
}

/// Run `session_by_uuid::resolve_tmux_session`, log both the UUID-
/// found-but-session-dead case and the JSONL/tmux lookup failure case,
/// and return the tmux session name (if any) for the caller to use.
fn try_resolve_uuid(uuid: &str, event_type: &str, origin: &str) -> Option<String> {
    match session_by_uuid::resolve_tmux_session(uuid) {
        Ok(Some(name)) => Some(name),
        Ok(None) => {
            // `Ok(None)` lumps several miss reasons together: UUID has
            // no on-disk JSONL (session unknown to this host), JSONL
            // exists but has no cwd record, or the cwd's tmux session
            // is not live. All of them mean "author session not here" —
            // fall back to repo routing rather than discard.
            warn!(
                uuid,
                origin,
                event_type,
                "author session not resolvable (jsonl/cwd/tmux miss); falling back to repo routing"
            );
            None
        }
        Err(e) => {
            // I/O or tmux failure in the JSONL/pane lookup. Surfaced
            // so a broken home directory or tmux regression doesn't
            // silently revert every wake to repo routing.
            warn!(error = %e, uuid, origin, event_type, "UUID resolve failed; falling back to repo routing");
            None
        }
    }
}

fn verify_signature(secret: &str, headers: &HeaderMap, body: &[u8]) -> Result<(), &'static str> {
    let header = headers
        .get("X-Hub-Signature-256")
        .ok_or("missing X-Hub-Signature-256")?
        .to_str()
        .map_err(|_| "non-ascii signature header")?;
    let supplied = header
        .strip_prefix("sha256=")
        .ok_or("malformed signature")?;
    let supplied_bytes = hex::decode(supplied).map_err(|_| "non-hex signature")?;

    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).map_err(|_| "secret invalid for HMAC")?;
    mac.update(body);
    let expected = mac.finalize().into_bytes();

    if expected.ct_eq(&supplied_bytes).into() {
        Ok(())
    } else {
        Err("signature mismatch")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- pr_number_from_payload ----

    #[test]
    fn pr_number_from_payload_reads_first_number() {
        let payload = json!({
            "check_suite": {"pull_requests": [{"number": 536}, {"number": 999}]},
        });
        assert_eq!(pr_number_from_payload(&payload), Some(536));
    }

    #[test]
    fn pr_number_from_payload_returns_none_for_empty_array() {
        let payload = json!({"check_suite": {"pull_requests": []}});
        assert_eq!(pr_number_from_payload(&payload), None);
    }

    #[test]
    fn pr_number_from_payload_returns_none_for_missing_field() {
        let payload = json!({"check_suite": {}});
        assert_eq!(pr_number_from_payload(&payload), None);
    }

    // ---- pr_number_from_head_branch ----

    /// Regression guard for cr#536: the synthetic-ref check_suite wake
    /// for `refs/pull/536/head` must route via UUID to the author
    /// session, not via repo fallback to whichever session has the
    /// repo open. Parsing 536 out of `head_branch` is what enables
    /// `uuid_from_check_suite` to fetch the PR body.
    #[test]
    fn pr_number_from_head_branch_parses_refs_pull_head() {
        let payload = json!({"check_suite": {"head_branch": "refs/pull/536/head"}});
        assert_eq!(pr_number_from_head_branch(&payload), Some(536));
    }

    #[test]
    fn pr_number_from_head_branch_parses_refs_pull_merge() {
        let payload = json!({"check_suite": {"head_branch": "refs/pull/42/merge"}});
        assert_eq!(pr_number_from_head_branch(&payload), Some(42));
    }

    /// Must not over-match user-authored branches that happen to
    /// share the `refs/pull/` prefix. Exactly-shaped parsing only.
    #[test]
    fn pr_number_from_head_branch_rejects_lookalikes() {
        for head_branch in [
            "refs/pull/foo",           // no tail segment
            "refs/pull/foo/bar",       // non-numeric "number"
            "refs/pull/4/patch",       // wrong tail segment
            "refs/pull//head",         // empty number
            "refs/pull/4/head/extra",  // trailing path component
            "refs/pulls/4/head",       // wrong prefix spelling
            "refs/pull-custom/4/head", // prefix doesn't match exactly
            "feat/some-branch",        // normal branch name
        ] {
            let payload = json!({"check_suite": {"head_branch": head_branch}});
            assert_eq!(
                pr_number_from_head_branch(&payload),
                None,
                "head_branch {head_branch:?} must not yield a PR number",
            );
        }
    }

    #[test]
    fn pr_number_from_head_branch_handles_missing_field() {
        let payload = json!({"check_suite": {}});
        assert_eq!(pr_number_from_head_branch(&payload), None);
    }

    fn entry(
        status: &str,
        conclusion: Option<&str>,
        latest_check_runs_count: u64,
        age_s: f64,
    ) -> CheckSuiteEntry {
        CheckSuiteEntry {
            status: status.into(),
            conclusion: conclusion.map(str::to_string),
            latest_check_runs_count,
            age_s,
        }
    }

    /// The GitGuardian-on-main shape: queued, zero check_runs, sitting
    /// well past the threshold while another App ran and completed.
    #[test]
    fn stuck_queued_detects_abandoned_suite() {
        let s = entry("queued", None, 0, 120.0);
        assert!(s.is_stuck_queued());
    }

    /// Regression guard for the cr#535 bug: the other Apps' suites
    /// are fresh (low age_s, no check_runs yet) when GitGuardian
    /// completes first. Must NOT be flagged — they're about to start.
    #[test]
    fn stuck_queued_does_not_filter_newly_minted_queued() {
        let s = entry("queued", None, 0, 2.0);
        assert!(!s.is_stuck_queued(), "2s age must not count as stuck");
    }

    /// A queued suite with at least one check_run has been picked up
    /// by a worker; the App is actively on the job regardless of age.
    #[test]
    fn stuck_queued_ignores_queued_with_check_runs() {
        let s = entry("queued", None, 1, 300.0);
        assert!(
            !s.is_stuck_queued(),
            "queued with check_runs ≥ 1 must never be flagged stuck even at high age",
        );
    }

    /// in_progress / completed / any non-queued status is active
    /// regardless of age or check_runs count. The filter is strictly
    /// narrower than "is this suite interesting".
    #[test]
    fn stuck_queued_only_matches_queued_status() {
        for status in [
            "in_progress",
            "completed",
            "waiting",
            "requested",
            "pending",
        ] {
            let s = entry(status, None, 0, 120.0);
            assert!(
                !s.is_stuck_queued(),
                "status {status:?} must not count as stuck-queued",
            );
        }
    }

    /// A completed skipped suite has no check_runs AND age may be
    /// high — must not be filtered because status is completed.
    #[test]
    fn stuck_queued_does_not_filter_completed_skipped_suites() {
        let s = entry("completed", Some("skipped"), 0, 600.0);
        assert!(!s.is_stuck_queued());
    }

    /// Boundary of the age threshold: exactly-at and just-below must
    /// NOT be flagged stuck. Locks in the exact cutoff so a future
    /// retune doesn't accidentally drop the inclusive guard.
    #[test]
    fn stuck_queued_respects_age_threshold_lower_bound() {
        let at_threshold = entry("queued", None, 0, STUCK_QUEUED_AGE_THRESHOLD_S);
        assert!(
            !at_threshold.is_stuck_queued(),
            "exactly at threshold must not be stuck (strict >)",
        );
        let just_below = entry("queued", None, 0, STUCK_QUEUED_AGE_THRESHOLD_S - 0.1);
        assert!(!just_below.is_stuck_queued());
    }
}
