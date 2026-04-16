//! Event classifier — turns a raw GitHub webhook payload into either a
//! human-readable prompt to inject into the Claude session, or a "drop this"
//! decision with a reason for the logs.
//!
//! Why this exists: forwarding every raw GitHub webhook spams the assistant
//! with non-actionable noise (per-file `workflow_run`, queued/in_progress
//! `check_suite` transitions, push events, etc.). Empirically, on a single
//! PR with multiple workflows we observed 5–6 wake events where only ONE
//! was actionable. Each wake costs ~500 tokens of context and triggers a
//! "checking …" round-trip. This module reduces that to one wake per
//! meaningful state transition.

use serde_json::Value;

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Forward { prompt: String },
    Drop { reason: &'static str },
}

/// Classify a parsed webhook payload. `event_type` is the value of the
/// `X-GitHub-Event` header; `repo` is the bare repo name (no owner);
/// `own_logins` is the list of GitHub logins whose actions should be
/// dropped (your own username + bots that act on your behalf).
///
/// Forwarding rules in plain language:
///   * `check_suite` is the *aggregate* of every workflow that ran for a
///     commit. We forward only the terminal `completed` action and call it
///     `ci-complete` — that's the one wake the assistant should react to.
///   * `workflow_run` and `check_run` are subordinate to check_suite and are
///     dropped.
///   * `pull_request_review` is forwarded only on `submitted` (the moment a
///     review actually appears) — `edited`/`dismissed` are noise.
///   * `pull_request` is forwarded on `opened`/`closed`/`reopened`/
///     `ready_for_review` — these are user-facing state changes, not the
///     dozens of `synchronize`/`labeled`/`assigned` updates.
///   * `issues` is forwarded on `opened`/`closed`/`reopened`/`assigned`.
///   * `issue_comment` is forwarded only when the body mentions `@claude-cr`
///     or starts with `/claude` (slash-command trigger) — normal issue chat
///     is noise.
///   * `push` is dropped (way too frequent on busy repos; CI completion is
///     covered by check_suite).
///   * `ping` is forwarded as a sentinel so we can confirm hook creation.
///   * Anything else is dropped — better to silently miss a new event type
///     than to leak random noise into the prompt.
///
/// Self-skip: if `payload.sender.login` is in `own_logins`, the event is
/// dropped regardless of type. This prevents waking the session for actions
/// it (or its bots) just took itself.
pub fn classify(
    event_type: &str,
    payload: &Value,
    repo: &str,
    own_logins: &[String],
) -> Decision {
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("");

    // Self-author skip applies ONLY to event types where "sender" semantically
    // means "the human (or my bot) who deliberately performed this action and
    // therefore already knows about it" — opening/closing a PR or issue.
    //
    // Critically NOT applied to check_suite / pull_request_review /
    // issue_comment: on check_suite the `sender` field is the pusher of the
    // commit that triggered CI, so suppressing those would silence the most
    // important wake event of all — the result of CI on my own push.
    if matches!(event_type, "pull_request" | "issues") {
        if let Some(sender) = payload.pointer("/sender/login").and_then(Value::as_str) {
            if own_logins.iter().any(|own| own == sender) {
                return Decision::Drop {
                    reason: "self-authored pull_request/issue (sender in own_logins)",
                };
            }
        }
    }

    match event_type {
        "ping" => Decision::Forward {
            prompt: format!("ghnotify: ping repo={repo}"),
        },

        "check_suite" if action == "completed" => {
            let conclusion = payload
                .pointer("/check_suite/conclusion")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let head_sha = payload
                .pointer("/check_suite/head_sha")
                .and_then(Value::as_str)
                .unwrap_or("");
            let head_branch = payload
                .pointer("/check_suite/head_branch")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let prs: Vec<String> = payload
                .pointer("/check_suite/pull_requests")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|pr| {
                            pr.get("number").and_then(Value::as_u64).map(|n| n.to_string())
                        })
                        .collect()
                })
                .unwrap_or_default();
            let pr_str = if prs.is_empty() { "none".into() } else { prs.join(",") };
            let head_short = head_sha.get(..8).unwrap_or(head_sha);
            Decision::Forward {
                prompt: format!(
                    "ghnotify ci-complete: repo={repo} status={conclusion} pr={pr_str} branch={head_branch} head={head_short}"
                ),
            }
        }
        "check_suite" => Decision::Drop {
            reason: "check_suite non-terminal action",
        },

        "workflow_run" => Decision::Drop {
            reason: "workflow_run is per-file noise; check_suite is the aggregate",
        },
        "check_run" => Decision::Drop {
            reason: "check_run subordinate to check_suite",
        },

        "pull_request_review" if action == "submitted" => {
            let reviewer = payload
                .pointer("/review/user/login")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let state = payload
                .pointer("/review/state")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let pr = payload
                .pointer("/pull_request/number")
                .and_then(Value::as_u64)
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into());
            Decision::Forward {
                prompt: format!(
                    "ghnotify code-review-complete: repo={repo} pr={pr} reviewer={reviewer} state={state}"
                ),
            }
        }
        "pull_request_review" => Decision::Drop {
            reason: "review action != submitted",
        },

        "pull_request"
            if matches!(action, "opened" | "closed" | "reopened" | "ready_for_review") =>
        {
            let pr = payload
                .pointer("/pull_request/number")
                .and_then(Value::as_u64)
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into());
            let merged = payload
                .pointer("/pull_request/merged")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Decision::Forward {
                prompt: format!("ghnotify pr: repo={repo} action={action} pr={pr} merged={merged}"),
            }
        }
        "pull_request" => Decision::Drop {
            reason: "pull_request action not interesting",
        },

        "issues" if matches!(action, "opened" | "closed" | "reopened" | "assigned") => {
            let n = payload
                .pointer("/issue/number")
                .and_then(Value::as_u64)
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into());
            Decision::Forward {
                prompt: format!("ghnotify issue: repo={repo} action={action} issue={n}"),
            }
        }
        "issues" => Decision::Drop {
            reason: "issue action not interesting",
        },

        "issue_comment" if action == "created" => {
            let body = payload
                .pointer("/comment/body")
                .and_then(Value::as_str)
                .unwrap_or("");
            // Wake only when explicitly addressed: @claude-cr mention or a
            // /claude slash-command on its own line. Otherwise normal issue
            // chat would wake the session every few seconds on busy repos.
            let mentioned = body.contains("@claude-cr")
                || body.contains("@claude")
                || body
                    .lines()
                    .any(|l| l.trim_start().starts_with("/claude"));
            if !mentioned {
                return Decision::Drop {
                    reason: "issue_comment without @claude / /claude trigger",
                };
            }
            let n = payload
                .pointer("/issue/number")
                .and_then(Value::as_u64)
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into());
            let author = payload
                .pointer("/comment/user/login")
                .and_then(Value::as_str)
                .unwrap_or("?");
            Decision::Forward {
                prompt: format!("ghnotify issue_comment: repo={repo} issue={n} author={author} (you were mentioned)"),
            }
        }
        "issue_comment" => Decision::Drop {
            reason: "issue_comment action != created",
        },

        "push" => Decision::Drop {
            reason: "push events are too noisy",
        },
        "status" | "deployment" | "deployment_status" => Decision::Drop {
            reason: "subordinate event type",
        },

        _ => Decision::Drop {
            reason: "unhandled event type",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn forward(d: &Decision) -> Option<&str> {
        match d {
            Decision::Forward { prompt } => Some(prompt.as_str()),
            Decision::Drop { .. } => None,
        }
    }

    #[test]
    fn ping_is_forwarded() {
        let d = classify("ping", &json!({}), "GitHub.Issues", &[]);
        assert_eq!(forward(&d), Some("ghnotify: ping repo=GitHub.Issues"));
    }

    #[test]
    fn workflow_run_is_dropped() {
        let d = classify("workflow_run", &json!({"action": "completed"}), "cr", &[]);
        assert!(matches!(d, Decision::Drop { .. }));
    }

    #[test]
    fn check_run_is_dropped() {
        let d = classify("check_run", &json!({"action": "completed"}), "cr", &[]);
        assert!(matches!(d, Decision::Drop { .. }));
    }

    #[test]
    fn check_suite_in_progress_is_dropped() {
        let d = classify("check_suite", &json!({"action": "in_progress"}), "cr", &[]);
        assert!(matches!(d, Decision::Drop { .. }));
    }

    #[test]
    fn check_suite_completed_is_forwarded_with_status_and_pr() {
        let payload = json!({
            "action": "completed",
            "check_suite": {
                "conclusion": "success",
                "head_sha": "abc12345deadbeef",
                "head_branch": "feat/foo",
                "pull_requests": [{"number": 476}, {"number": 477}],
            },
        });
        let d = classify("check_suite", &payload, "cr", &[]);
        assert_eq!(
            forward(&d),
            Some("ghnotify ci-complete: repo=cr status=success pr=476,477 branch=feat/foo head=abc12345"),
        );
    }

    #[test]
    fn check_suite_completed_no_pr_uses_none() {
        let payload = json!({
            "action": "completed",
            "check_suite": {
                "conclusion": "failure",
                "head_sha": "deadbeef",
                "head_branch": "main",
                "pull_requests": [],
            },
        });
        let d = classify("check_suite", &payload, "cr", &[]);
        assert_eq!(
            forward(&d),
            Some("ghnotify ci-complete: repo=cr status=failure pr=none branch=main head=deadbeef"),
        );
    }

    #[test]
    fn pr_review_submitted_is_forwarded() {
        let payload = json!({
            "action": "submitted",
            "review": {"user": {"login": "copilot[bot]"}, "state": "commented"},
            "pull_request": {"number": 123},
        });
        let d = classify("pull_request_review", &payload, "GitHub.Issues", &[]);
        assert_eq!(
            forward(&d),
            Some("ghnotify code-review-complete: repo=GitHub.Issues pr=123 reviewer=copilot[bot] state=commented"),
        );
    }

    #[test]
    fn pr_review_edited_is_dropped() {
        let d = classify("pull_request_review", &json!({"action": "edited"}), "x", &[]);
        assert!(matches!(d, Decision::Drop { .. }));
    }

    #[test]
    fn pr_synchronize_is_dropped_pr_opened_is_forwarded() {
        let sync = classify("pull_request", &json!({"action": "synchronize"}), "x", &[]);
        assert!(matches!(sync, Decision::Drop { .. }));

        let opened = classify(
            "pull_request",
            &json!({"action": "opened", "pull_request": {"number": 99, "merged": false}}),
            "x",
            &[],
        );
        assert_eq!(
            forward(&opened),
            Some("ghnotify pr: repo=x action=opened pr=99 merged=false"),
        );
    }

    #[test]
    fn push_is_dropped() {
        let d = classify("push", &json!({}), "x", &[]);
        assert!(matches!(d, Decision::Drop { .. }));
    }

    #[test]
    fn unknown_event_is_dropped() {
        let d = classify("totally_made_up", &json!({}), "x", &[]);
        assert!(matches!(d, Decision::Drop { .. }));
    }

    #[test]
    fn self_authored_pr_open_is_dropped() {
        let payload = json!({
            "action": "opened",
            "sender": {"login": "Olbrasoft"},
            "pull_request": {"number": 99, "merged": false},
        });
        let own = vec!["Olbrasoft".to_string()];
        let d = classify("pull_request", &payload, "x", &own);
        assert!(matches!(d, Decision::Drop { .. }));
    }

    #[test]
    fn other_authored_pr_open_passes_self_skip() {
        let payload = json!({
            "action": "opened",
            "sender": {"login": "copilot[bot]"},
            "pull_request": {"number": 99, "merged": false},
        });
        let own = vec!["Olbrasoft".to_string()];
        let d = classify("pull_request", &payload, "x", &own);
        assert!(matches!(d, Decision::Forward { .. }));
    }

    #[test]
    fn self_authored_check_suite_is_NOT_dropped() {
        // Critical: ci-complete events MUST reach the session even though the
        // pusher (= sender) is in own_logins. Otherwise the assistant never
        // hears about CI results for its own pushes.
        let payload = json!({
            "action": "completed",
            "sender": {"login": "Olbrasoft"},
            "check_suite": {
                "conclusion": "success",
                "head_sha": "abc12345",
                "head_branch": "fix/foo",
                "pull_requests": [{"number": 99}],
            },
        });
        let own = vec!["Olbrasoft".to_string()];
        let d = classify("check_suite", &payload, "cr", &own);
        assert!(matches!(d, Decision::Forward { .. }));
    }

    #[test]
    fn self_authored_pr_review_is_NOT_dropped() {
        // Same reason: even if the review event arrives with sender=self for
        // some payload reason, we want to know about reviews on my PRs.
        let payload = json!({
            "action": "submitted",
            "sender": {"login": "Olbrasoft"},
            "review": {"user": {"login": "copilot[bot]"}, "state": "commented"},
            "pull_request": {"number": 1},
        });
        let own = vec!["Olbrasoft".to_string()];
        let d = classify("pull_request_review", &payload, "x", &own);
        assert!(matches!(d, Decision::Forward { .. }));
    }

    #[test]
    fn issue_comment_without_mention_is_dropped() {
        let payload = json!({
            "action": "created",
            "comment": {"body": "Just chatting normally", "user": {"login": "alice"}},
            "issue": {"number": 5},
        });
        assert!(matches!(
            classify("issue_comment", &payload, "x", &[]),
            Decision::Drop { .. }
        ));
    }

    #[test]
    fn issue_comment_with_at_claude_is_forwarded() {
        let payload = json!({
            "action": "created",
            "comment": {"body": "Hey @claude-cr please look at this", "user": {"login": "alice"}},
            "issue": {"number": 5},
        });
        assert!(matches!(
            classify("issue_comment", &payload, "x", &[]),
            Decision::Forward { .. }
        ));
    }

    #[test]
    fn issue_comment_with_slash_claude_is_forwarded() {
        let payload = json!({
            "action": "created",
            "comment": {"body": "/claude do the thing", "user": {"login": "alice"}},
            "issue": {"number": 5},
        });
        assert!(matches!(
            classify("issue_comment", &payload, "x", &[]),
            Decision::Forward { .. }
        ));
    }
}
