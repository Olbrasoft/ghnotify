//! HTTP webhook receiver. Accepts GitHub-style JSON webhooks at `POST /webhook`,
//! resolves the affected repo, and dispatches a prompt to the matching tmux session.
//!
//! GitHub event payload shape (subset we care about):
//!   { "repository": { "name": "GitHub.Issues", "full_name": "Olbrasoft/GitHub.Issues" }, ... }
//!
//! Runs in two modes:
//!   * persistent — binds an address and serves forever (`ghnotify serve`)
//!   * one-shot   — serves exactly one request then exits (`ghnotify serve --one-shot`),
//!     intended to be run per-connection under systemd socket activation so
//!     nothing is running between webhook deliveries.

use crate::{config::Config, event, tmux};
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
    let prompt = match event::classify(
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

    // 4. Dispatch to the matching tmux session.
    let session = tmux::session_name_for_repo(repo_name);
    match tmux::send_prompt(&session, &prompt) {
        Ok(tmux::Delivery::Delivered) => {
            info!(session, event_type, "prompt delivered");
            (
                StatusCode::OK,
                Json(serde_json::json!({ "ok": true, "session": session })),
            )
        }
        // No session for this repo → soft discard. Webhook senders do not
        // retry on non-2xx anyway, but we return 200 so logs stay clean:
        // there is nothing wrong, there is just no one home to wake up.
        Ok(tmux::Delivery::NoSession) => {
            info!(
                session,
                event_type, "no claude session for repo, event discarded"
            );
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "ok": true,
                    "discarded": true,
                    "reason": "no claude session running for this repo",
                    "session": session,
                })),
            )
        }
        Err(e) => {
            error!(session, error = %e, "tmux send_prompt failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
            )
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
