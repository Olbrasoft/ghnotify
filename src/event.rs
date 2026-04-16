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
/// `own_logins` is the list of GitHub logins representing "me" (your own
/// username + bots that act on your behalf).
///
/// **Design principle: wake only when an action is required.** Mere state
/// changes (PR opened, PR merged, issue filed) do not trigger a wake — they
/// become actionable only when CI or a reviewer concludes. Every forwarded
/// event must answer "what do I do in response?" with a concrete verb.
///
/// Forwarding rules:
///   * `check_suite completed` is the one reliable CI aggregate. Success →
///     `ci-success` (decide whether to merge); failure/cancelled/timed_out →
///     `ci-failure` (fix it). Non-terminal actions are dropped.
///   * `pull_request_review submitted` → `code-review-complete` (read
///     comments, address or merge). `edited`/`dismissed` are noise.
///   * `issue_comment created` is forwarded only when the body mentions
///     `@claude`/`@claude-cr` or starts with `/claude` — the explicit
///     "please act" signal.
///   * `issues assigned` is forwarded only when the assignee is in
///     `own_logins`. Someone else's assignment is not my work.
///   * `pull_request` is fully dropped — the wakes that matter are the
///     downstream `check_suite` and `pull_request_review` events, not the
///     state change itself. I know when *I* opened/merged a PR.
///   * `issues opened/closed/reopened` is dropped — triage is not a wake-up
///     task.
///   * `workflow_run`/`check_run`/`push`/`status`/`deployment*` are dropped
///     as subordinate or too-frequent.
///   * `ping` is forwarded so hook creation is visible.
///   * Anything else is dropped — silently miss a new event type rather than
///     leak noise.
pub fn classify(event_type: &str, payload: &Value, repo: &str, own_logins: &[String]) -> Decision {
    let action = payload.get("action").and_then(Value::as_str).unwrap_or("");

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
                            pr.get("number")
                                .and_then(Value::as_u64)
                                .map(|n| n.to_string())
                        })
                        .collect()
                })
                .unwrap_or_default();
            let pr_str = if prs.is_empty() {
                "none".into()
            } else {
                prs.join(",")
            };
            let head_short = head_sha.get(..8).unwrap_or(head_sha);
            let kind = if conclusion == "success" {
                "ci-success"
            } else {
                "ci-failure"
            };
            Decision::Forward {
                prompt: format!(
                    "ghnotify {kind}: repo={repo} status={conclusion} pr={pr_str} branch={head_branch} head={head_short}"
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

        // pull_request is informational only. All wake-ups related to a PR
        // come from downstream events (check_suite, pull_request_review) or
        // from explicit @claude mentions in comments. Knowing that a PR was
        // opened/merged/synchronized is never on its own a call to action.
        "pull_request" => Decision::Drop {
            reason: "pull_request is informational; wakes come from check_suite/review",
        },

        // issues: only "someone just handed me this issue" is an immediate
        // action trigger. Opening/closing/reopening an issue does not require
        // the session to wake — that's what @claude mentions are for.
        "issues" if action == "assigned" => {
            let assignee = payload
                .pointer("/assignee/login")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !own_logins.iter().any(|own| own == assignee) {
                return Decision::Drop {
                    reason: "issue assigned to someone else",
                };
            }
            let n = payload
                .pointer("/issue/number")
                .and_then(Value::as_u64)
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into());
            Decision::Forward {
                prompt: format!(
                    "ghnotify issue-assigned-to-me: repo={repo} issue={n} assignee={assignee}"
                ),
            }
        }
        "issues" => Decision::Drop {
            reason: "issues action not actionable on its own",
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
                || body.lines().any(|l| l.trim_start().starts_with("/claude"));
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
    fn check_suite_completed_success_is_forwarded_as_ci_success() {
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
            Some("ghnotify ci-success: repo=cr status=success pr=476,477 branch=feat/foo head=abc12345"),
        );
    }

    #[test]
    fn check_suite_completed_failure_is_forwarded_as_ci_failure() {
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
            Some("ghnotify ci-failure: repo=cr status=failure pr=none branch=main head=deadbeef"),
        );
    }

    #[test]
    fn check_suite_completed_cancelled_is_ci_failure() {
        let payload = json!({
            "action": "completed",
            "check_suite": {
                "conclusion": "cancelled",
                "head_sha": "c0ffee0000",
                "head_branch": "feat/x",
                "pull_requests": [{"number": 1}],
            },
        });
        let d = classify("check_suite", &payload, "cr", &[]);
        // cancelled / timed_out / action_required all classify as ci-failure
        // (something actionable went wrong).
        assert!(forward(&d).unwrap().starts_with("ghnotify ci-failure:"));
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
        let d = classify(
            "pull_request_review",
            &json!({"action": "edited"}),
            "x",
            &[],
        );
        assert!(matches!(d, Decision::Drop { .. }));
    }

    #[test]
    fn all_pull_request_actions_are_dropped() {
        // Every pull_request action — opened, closed, reopened, synchronize,
        // ready_for_review, labeled, edited — is non-actionable on its own.
        // Wakes come from the downstream check_suite and pull_request_review.
        for action in [
            "opened",
            "closed",
            "reopened",
            "synchronize",
            "ready_for_review",
            "labeled",
            "edited",
            "assigned",
        ] {
            let payload = json!({
                "action": action,
                "pull_request": {"number": 99, "merged": false},
                "sender": {"login": "someone-else"},
            });
            assert!(
                matches!(
                    classify("pull_request", &payload, "x", &[]),
                    Decision::Drop { .. }
                ),
                "pull_request action '{action}' should be dropped",
            );
        }
    }

    #[test]
    fn pr_merged_by_self_is_dropped() {
        // I just merged the PR myself → I know; the post-merge check_suite on
        // main is the next wake that matters.
        let payload = json!({
            "action": "closed",
            "pull_request": {"number": 99, "merged": true},
            "sender": {"login": "Olbrasoft"},
        });
        let own = vec!["Olbrasoft".to_string()];
        assert!(matches!(
            classify("pull_request", &payload, "x", &own),
            Decision::Drop { .. },
        ));
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
    fn self_pushed_check_suite_is_forwarded() {
        // Critical: CI results for *my own* pushes must reach the session,
        // even though sender == own_logins. The classifier never drops
        // check_suite on sender; this test locks that behavior in.
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
    fn pr_review_is_forwarded_regardless_of_sender() {
        // Reviews on my PRs are always actionable — read the comments or merge.
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
    fn issue_opened_is_dropped_even_from_others() {
        // Triage is not a wake-up task; @claude mentions handle "please act".
        let payload = json!({
            "action": "opened",
            "issue": {"number": 5},
            "sender": {"login": "alice"},
        });
        assert!(matches!(
            classify("issues", &payload, "x", &[]),
            Decision::Drop { .. },
        ));
    }

    #[test]
    fn issue_closed_reopened_labeled_are_dropped() {
        for action in ["closed", "reopened", "labeled", "edited", "unassigned"] {
            let payload = json!({
                "action": action,
                "issue": {"number": 5},
                "sender": {"login": "alice"},
            });
            assert!(
                matches!(
                    classify("issues", &payload, "x", &[]),
                    Decision::Drop { .. }
                ),
                "issues action '{action}' should be dropped",
            );
        }
    }

    #[test]
    fn issue_assigned_to_me_is_forwarded() {
        let payload = json!({
            "action": "assigned",
            "issue": {"number": 42},
            "assignee": {"login": "Olbrasoft"},
            "sender": {"login": "alice"},
        });
        let own = vec!["Olbrasoft".to_string()];
        let d = classify("issues", &payload, "cr", &own);
        assert_eq!(
            forward(&d),
            Some("ghnotify issue-assigned-to-me: repo=cr issue=42 assignee=Olbrasoft"),
        );
    }

    #[test]
    fn issue_assigned_to_someone_else_is_dropped() {
        let payload = json!({
            "action": "assigned",
            "issue": {"number": 42},
            "assignee": {"login": "bob"},
            "sender": {"login": "alice"},
        });
        let own = vec!["Olbrasoft".to_string()];
        assert!(matches!(
            classify("issues", &payload, "cr", &own),
            Decision::Drop { .. },
        ));
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
