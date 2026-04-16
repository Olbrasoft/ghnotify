//! HTTP webhook receiver. Accepts GitHub-style JSON webhooks at `POST /webhook`,
//! resolves the affected repo, and dispatches a prompt to the matching tmux session.
//!
//! GitHub event payload shape (subset we care about):
//!   { "repository": { "name": "GitHub.Issues", "full_name": "Olbrasoft/GitHub.Issues" }, ... }

use crate::{config::Config, event, tmux};
use anyhow::Result;
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tracing::{error, info, warn};

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
}

pub async fn serve(cfg: Config, bind_override: Option<String>) -> Result<()> {
    let bind = bind_override.unwrap_or_else(|| cfg.server.bind.clone());
    let state = AppState { cfg: Arc::new(cfg) };

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/webhook", post(handle_webhook))
        .with_state(state);

    info!(bind = %bind, "ghnotify listening");

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn handle_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    // 1. Optional HMAC signature verification.
    if let Some(secret) = state.cfg.github.webhook_secret.as_deref() {
        match verify_signature(secret, &headers, &body) {
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
    let payload: serde_json::Value = match serde_json::from_slice(&body) {
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
        return (StatusCode::OK, Json(serde_json::json!({ "ok": true, "ignored": true })));
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
            info!(event_type, repo = repo_name, reason, "event dropped by filter");
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
            (StatusCode::OK, Json(serde_json::json!({ "ok": true, "session": session })))
        }
        // No session for this repo → soft discard. Webhook senders (gh webhook
        // forward, GitHub itself) do not retry on non-2xx anyway, but we return
        // 200 so logs stay clean: there is nothing wrong, there is just no one
        // home to wake up.
        Ok(tmux::Delivery::NoSession) => {
            info!(session, event_type, "no claude session for repo, event discarded");
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
    let supplied = header.strip_prefix("sha256=").ok_or("malformed signature")?;
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
