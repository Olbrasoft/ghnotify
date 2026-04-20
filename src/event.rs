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
//!
//! A single commit typically has N `check_suite` entities — one per GitHub
//! App installed on the repo (main CI workflow, GitGuardian, Copilot
//! reviewer, …). GitHub fires a `check_suite completed` webhook for each
//! of them independently as they finish. Classifying every one as a
//! terminal CI wake would wake the session the moment the *first* App's
//! suite finishes (often GitGuardian, which is typically fastest), before
//! the real test/build suite is anywhere near done. The webhook handler
//! compensates by querying the aggregate check_suite state via `gh api`
//! and deferring until every suite on the head SHA has `status=completed`
//! — see [`compute_ci_aggregate`] and `webhook::refine_ci_decision`.

use serde_json::Value;

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Forward { prompt: String },
    Drop { reason: &'static str },
}

/// Fields extracted from a `check_suite completed` payload that the
/// webhook handler needs to both (a) query the aggregate check_suite
/// state via `gh api repos/OWNER/NAME/commits/SHA/check-suites` and
/// (b) rebuild the outgoing prompt with the *aggregate* conclusion
/// (which can differ from this single event's conclusion — e.g. this
/// event reports success but a sibling suite already failed).
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct CiInfo {
    pub repo: String,
    pub pr_str: String,
    pub head_branch: String,
    pub head_short: String,
    pub head_sha: String,
    pub full_name: String,
}

/// Result of merging every check_suite's conclusion on a single head SHA.
/// `HasFailure` wins over `AllSuccess`: a green main suite doesn't cancel
/// out a red GitGuardian scan — if anything on this commit is broken,
/// the aggregate wake must say so.
#[derive(Debug, PartialEq, Eq)]
pub enum CiAggregate {
    AllSuccess,
    HasFailure,
    InProgress,
}

/// Exactly-shaped test for GitHub's PR-synthetic check_suite refs:
/// `refs/pull/<digits>/head` or `refs/pull/<digits>/merge`. A plain prefix
/// match on `refs/pull/` would over-match any user-authored branch
/// happening to share that prefix; the stricter shape avoids that over-
/// match and rules out the obvious lookalikes (`refs/pulls/…`,
/// `refs/pull/4/patch`, non-numeric segment, extra trailing path). A
/// user who deliberately names a branch exactly
/// `refs/pull/<digits>/(head|merge)` would still be suppressed — that
/// collision is accepted as the cost of deduplicating the paired wake
/// for the overwhelmingly common case. The check_suite payload carries
/// no field that would distinguish a user's branch-push from GitHub's
/// synthetic-ref dispatch once the shape matches.
fn is_refs_pull_synthetic(head_branch: &str) -> bool {
    let Some(rest) = head_branch.strip_prefix("refs/pull/") else {
        return false;
    };
    let Some((num, tail)) = rest.split_once('/') else {
        return false;
    };
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    matches!(tail, "head" | "merge")
}

/// Trim, sanitize, and truncate a review/comment body for inclusion in the
/// wake prompt. Empty → empty string; otherwise returns ` body="<excerpt>"`.
///
/// **Security:** the prompt is forwarded to `tmux send-keys -l` which is
/// literal — but a session running `claude` renders received text on a
/// terminal, and ESC (`\u{001b}`) + CSI bytes embedded in a GitHub review
/// body would be interpreted as terminal control sequences / keystroke
/// injection by the TUI. The sanitizer maps every `char::is_control()`
/// character to a space so untrusted GitHub content can never smuggle
/// escape codes through. `"` is remapped to `'` because the excerpt is
/// wrapped in double-quotes in the final prompt.
///
/// Cap is 200 chars **total** (including the trailing `…` when truncated);
/// the ellipsis is budgeted in, not appended outside the limit. The body
/// is walked once, accumulating directly into the output buffer, so large
/// review bodies don't cause redundant allocation / counting passes.
fn excerpt_body(body: &str) -> String {
    const MAX: usize = 200;
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let mut excerpt = String::with_capacity(MAX * 4);
    let mut truncated = false;
    let mut has_visible = false;

    for (i, c) in trimmed.chars().enumerate() {
        if i == MAX {
            truncated = true;
            break;
        }
        let sanitized = if c.is_control() {
            ' '
        } else if c == '"' {
            '\''
        } else {
            c
        };
        if !sanitized.is_whitespace() {
            has_visible = true;
        }
        excerpt.push(sanitized);
    }

    // `str::trim` only removes ASCII/Unicode whitespace, not control chars, so
    // a body of pure control bytes (e.g. ESC + BEL) would pass the early
    // `trimmed.is_empty()` guard and then be sanitized into a run of spaces —
    // emitting ` body="   "` as pure noise. If nothing visible survived the
    // sanitizer, treat the excerpt as empty.
    if !has_visible {
        return String::new();
    }

    if truncated {
        // Make room for the ellipsis within MAX by popping the last char.
        excerpt.pop();
        excerpt.push('…');
    }

    format!(" body=\"{excerpt}\"")
}

/// Common default-branch names, used by the CI hint to distinguish a
/// post-merge aggregate wake ("idle or deploy") from a PR-fix-push wake
/// ("merge now"). Hard-coded rather than configurable because (a) these
/// three cover the overwhelming majority of repos, (b) misclassifying a
/// non-default branch only demotes the hint from the post-merge variant
/// to the safe no-action fallback, which is graceful. A repo with an
/// exotic default branch name would still get a usable (if less precise)
/// hint.
fn is_likely_default_branch(head_branch: &str) -> bool {
    matches!(head_branch, "main" | "master" | "trunk")
}

/// Concrete next-step instruction appended to the `ci-success` / `ci-failure`
/// wake. Without it sessions idle after a fix-push ("maybe Copilot will
/// re-review?") or diagnose-and-stop on failures ("it's pre-existing,
/// skipping"). Both are wrong: a wake is a call to action. The hint
/// removes all ambiguity about what to do next.
///
/// The branching matters: a CI wake on a **feature branch with a PR**
/// means merge-now (if comments addressed) or push-fix-push (if
/// CI failed). A CI wake on the **default branch with no PR** is the
/// post-merge aggregate — no action unless deploy fires.
fn ci_next_action_hint(is_failure: bool, pr_str: &str, head_branch: &str) -> &'static str {
    if is_failure {
        return " | next: CI failed — this wake is a call to action, not a report. Read failing check logs (gh run view / gh api), diagnose, fix, push. Do NOT stop at 'pre-existing, skip' — if it's truly unrelated, confirm on the default branch and open a separate issue before closing this wake.";
    }
    let has_pr = !pr_str.is_empty() && pr_str != "none";
    let on_default_branch = is_likely_default_branch(head_branch);
    match (has_pr, on_default_branch) {
        (true, _) => {
            // `--delete-branch` is only safe for a regular feature-branch
            // head; dropping it from the default invocation avoids "cannot
            // delete protected branch" errors on the rare PR whose head is
            // a protected branch. The session can append it when safe.
            " | next: CI green on open PR. If Copilot already reviewed and you addressed comments in this push → merge NOW (gh pr merge <pr> --squash; append --delete-branch only if the head branch is safe to delete). Copilot will NOT auto re-review; do not wait. If no review yet → request one (`/copilot review` comment) only if substantive, otherwise merge directly."
        }
        (false, true) => {
            " | next: post-merge CI green on the default branch. If this repo has a deploy workflow, the deploy/verify wake will follow — idle until then. Otherwise the workflow is complete; close underlying issue if applicable."
        }
        // Branch without a PR linkage (weird — direct push to a feature
        // branch, or check_suite arrived before PR was opened). Safe fallback.
        (false, false) => {
            " | next: CI green. If this commit backs an open PR not yet linked, proceed to merge once the PR appears; otherwise no further action."
        }
    }
}

/// Concrete next-step instruction appended to the `code-review-complete`
/// wake. Without it sessions loop on wishful thinking ("surely Copilot will
/// re-review my fix-push") and hang indefinitely; Copilot only reviews once
/// per explicit request and does NOT auto re-review on subsequent pushes.
/// The hint is tailored to the review state so the verb is unambiguous.
fn next_action_hint(state: &str) -> &'static str {
    match state {
        "approved" => {
            " | next: if CI green → merge now. No further review expected; do not wait."
        }
        "changes_requested" => {
            " | next: read ALL comments (gh pr view <pr> / gh api repos/OWNER/REPO/pulls/<pr>/comments), fix every item, push, then merge when CI green. Copilot reviews once per request — it will NOT auto re-review your fix-push. Only post `/copilot review` if you introduced substantial new changes beyond addressing comments."
        }
        // `commented` (the common Copilot outcome) and any unknown state
        // fall through to the same action: treat comments as asks, fix,
        // push, merge. The anti-wait clause is the critical part.
        _ => {
            " | next: read comments (gh pr view <pr> / gh api repos/OWNER/REPO/pulls/<pr>/comments), fix ALL, push, then merge when CI green. Copilot reviews once per request — it will NOT auto re-review your fix-push. Only post `/copilot review` if you introduced substantial new changes beyond addressing comments."
        }
    }
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
///     `ci-failure` (fix it). A `| next: …` action hint is appended that
///     branches on PR-vs-main: on a feature branch with a PR it says
///     merge-now (Copilot will not auto re-review the fix-push); on main
///     it acknowledges post-merge aggregate; on failure it is an explicit
///     "diagnose and fix" (NOT "diagnose and stop at pre-existing").
///     Non-terminal actions are dropped. The parallel check_suite that
///     GitHub dispatches on the synthetic `refs/pull/N/…` ref for every
///     PR is dropped — the branch-named pair is strictly more informative
///     and forwarding both means the session is woken twice per CI run.
///   * `pull_request_review submitted`/`edited` → `code-review-complete`
///     with truncated `review.body` excerpt so the session sees whether the
///     reviewer left substantive comments (vs. just an empty "commented"
///     review) without a round-trip. A `| next: …` action hint is appended
///     (verb tailored to review state) so the session cannot silently
///     wait for a second Copilot review that will never come — Copilot
///     reviews once per explicit request, never auto re-reviews a push.
///     `dismissed` is dropped (withdrawn).
///   * `pull_request_review_comment created` → `review-comment` with file
///     path, line number, and body excerpt. Critical pairing with the
///     review wake: Copilot often posts per-line nitpicks whose content
///     lives only in the individual comment payloads — without this, a
///     session seeing only `ci-success` + empty `code-review-complete`
///     would assume "ship it" and miss the fixes to make.
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
            let head_branch = payload
                .pointer("/check_suite/head_branch")
                .and_then(Value::as_str)
                .unwrap_or("?");
            // GitHub dispatches a parallel check_suite on the synthetic
            // `refs/pull/N/head` (and sometimes `refs/pull/N/merge`) ref for
            // every PR, in addition to the one for the PR's actual branch
            // name (e.g. `fix/foo`). Both fire for the same head_sha, so
            // forwarding both means the session gets two ci-success/failure
            // wakes for every PR CI run. The named-branch one is strictly
            // more informative (carries the real branch name + pr numbers
            // together), so drop the synthetic ref to keep 1 wake per run.
            // Post-merge redelivery of the synthetic ref is the specific
            // case that used to look like "a late duplicate wake arriving
            // after merge" — it's not late, it's just the second of the
            // pair completing.
            //
            // The predicate matches the exact shape
            // `refs/pull/<digits>/(head|merge)` rather than a naive
            // prefix check, which rules out the common lookalikes
            // (`refs/pulls/…`, `refs/pull/4/patch`, non-numeric middle,
            // extra trailing path). A user deliberately naming a
            // branch exactly `refs/pull/<digits>/(head|merge)` would
            // still be suppressed — see the docstring on
            // `is_refs_pull_synthetic` for why that residual collision
            // is accepted.
            if is_refs_pull_synthetic(head_branch) {
                return Decision::Drop {
                    reason: "check_suite on refs/pull/ synthetic ref (pair-dup of branch-named check_suite)",
                };
            }
            let conclusion = payload
                .pointer("/check_suite/conclusion")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let info = extract_ci_info(payload, repo);
            Decision::Forward {
                prompt: build_ci_prompt(&info, conclusion),
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

        // Forward both `submitted` (new review) and `edited` (review body
        // changed after posting). Observed in the wild: Copilot's code-review
        // bot sometimes emits `edited` without a preceding `submitted` — the
        // actionable content *is* in the edit, so dropping it hangs the
        // session. Only `dismissed` is dropped (review was withdrawn).
        "pull_request_review" if action == "submitted" || action == "edited" => {
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
            let body = payload
                .pointer("/review/body")
                .and_then(Value::as_str)
                .unwrap_or("");
            let excerpt = excerpt_body(body);
            let hint = next_action_hint(state);
            Decision::Forward {
                prompt: format!(
                    "ghnotify code-review-complete: repo={repo} pr={pr} reviewer={reviewer} state={state} action={action}{excerpt}{hint}"
                ),
            }
        }
        "pull_request_review" => Decision::Drop {
            reason: "review dismissed or unknown action",
        },

        // Per-line review comments. Without this handler they fall into the
        // default drop, so a session that sees `ci-success` + an otherwise
        // silent `pull_request_review` (state=commented, empty body) has no
        // signal that reviewer left per-file nitpicks to address. Forward
        // `created` only — `edited`/`deleted` on a comment the session
        // already consumed would be churn.
        "pull_request_review_comment" if action == "created" => {
            let author = payload
                .pointer("/comment/user/login")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let pr = payload
                .pointer("/pull_request/number")
                .and_then(Value::as_u64)
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into());
            let path = payload
                .pointer("/comment/path")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let line = payload
                .pointer("/comment/line")
                .and_then(Value::as_u64)
                .or_else(|| {
                    payload
                        .pointer("/comment/original_line")
                        .and_then(Value::as_u64)
                })
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into());
            let body = payload
                .pointer("/comment/body")
                .and_then(Value::as_str)
                .unwrap_or("");
            let excerpt = excerpt_body(body);
            Decision::Forward {
                prompt: format!(
                    "ghnotify review-comment: repo={repo} pr={pr} author={author} file={path}:{line}{excerpt}"
                ),
            }
        }
        "pull_request_review_comment" => Decision::Drop {
            reason: "review_comment action != created",
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

/// Pull the fields needed to build a CI wake prompt out of a check_suite
/// payload. Missing fields degrade gracefully to empty strings — the
/// aggregate query in the webhook handler inspects `head_sha` /
/// `full_name` and skips the `gh api` call (fail-open to the
/// single-event prompt) when either is empty, so a malformed or stubbed
/// payload can't both strip the aggregate and silently vanish the wake.
pub fn extract_ci_info(payload: &Value, repo: &str) -> CiInfo {
    let head_sha = payload
        .pointer("/check_suite/head_sha")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let full_name = payload
        .pointer("/repository/full_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let head_branch = payload
        .pointer("/check_suite/head_branch")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string();
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
    let head_short = head_sha.get(..8).unwrap_or(&head_sha).to_string();
    CiInfo {
        repo: repo.to_string(),
        pr_str,
        head_branch,
        head_short,
        head_sha,
        full_name,
    }
}

/// Build the user-facing CI wake prompt. `conclusion` is the conclusion
/// the wake should report — normally the *aggregate* across every
/// check_suite on the head SHA, not any single event's conclusion, so
/// that a sibling suite's failure is visible even if the triggering
/// event itself was a success.
pub fn build_ci_prompt(info: &CiInfo, conclusion: &str) -> String {
    let is_failure = conclusion != "success";
    let kind = if is_failure {
        "ci-failure"
    } else {
        "ci-success"
    };
    let hint = ci_next_action_hint(is_failure, &info.pr_str, &info.head_branch);
    format!(
        "ghnotify {kind}: repo={repo} status={conclusion} pr={pr_str} branch={branch} head={head}{hint}",
        repo = info.repo,
        pr_str = info.pr_str,
        branch = info.head_branch,
        head = info.head_short,
    )
}

/// Merge per-suite `(status, conclusion)` pairs into a single aggregate
/// outcome for a commit.
///
/// Rules:
///   * Any suite not yet `completed` (queued/in_progress/waiting/…) →
///     `InProgress`: the aggregate is not final yet, defer the wake.
///   * Among completed suites, `success`/`neutral`/`skipped`/`stale` are
///     treated as non-failures (their presence alone won't turn an
///     otherwise-clean aggregate red — `stale` in particular is GitHub's
///     "a newer commit superseded this result", which is neither a pass
///     nor a failure of the current run and is safe to ignore).
///   * Any other conclusion (`failure`, `cancelled`, `timed_out`,
///     `action_required`, `startup_failure`, `None`, unknown) → `HasFailure`.
///     `None` on a completed suite is unusual but we prefer to escalate
///     than silently treat it as success.
///   * Empty input → `InProgress`: the gh call returned no suites, which
///     means we can't confirm readiness; the caller should fail-open to
///     the single-event prompt rather than drop on this ambiguity.
pub fn compute_ci_aggregate<'a, I>(suites: I) -> CiAggregate
where
    I: IntoIterator<Item = (&'a str, Option<&'a str>)>,
{
    let mut any_failure = false;
    let mut any = false;
    for (status, conclusion) in suites {
        any = true;
        if status != "completed" {
            return CiAggregate::InProgress;
        }
        match conclusion {
            Some("success") | Some("neutral") | Some("skipped") | Some("stale") => {}
            _ => any_failure = true,
        }
    }
    if !any {
        return CiAggregate::InProgress;
    }
    if any_failure {
        CiAggregate::HasFailure
    } else {
        CiAggregate::AllSuccess
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
        let prompt = forward(&d).expect("forwarded");
        assert!(
            prompt.starts_with(
                "ghnotify ci-success: repo=cr status=success pr=476,477 branch=feat/foo head=abc12345"
            ),
            "prefix wrong: {prompt}"
        );
        // Feature branch + PR → merge-now hint.
        assert!(
            prompt.contains("merge NOW"),
            "merge-now hint missing: {prompt}"
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
        let prompt = forward(&d).expect("forwarded");
        assert!(
            prompt.starts_with(
                "ghnotify ci-failure: repo=cr status=failure pr=none branch=main head=deadbeef"
            ),
            "prefix wrong: {prompt}"
        );
        // Failure → call to action, anti-"diagnose and stop" clause.
        assert!(
            prompt.contains("call to action"),
            "call-to-action clause missing: {prompt}"
        );
        assert!(
            prompt.contains("pre-existing"),
            "anti-skip clause missing: {prompt}"
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
        let prompt = forward(&d).expect("forwarded");
        assert!(prompt.starts_with("ghnotify ci-failure:"));
        // Same anti-skip clause for cancelled (it's still a failure from the
        // session's perspective — something needs attention).
        assert!(prompt.contains("call to action"), "{prompt}");
    }

    #[test]
    fn ci_success_on_default_branch_has_post_merge_hint() {
        // Post-merge CI wakes (pr=none, branch=default) should NOT tell the
        // session to merge — there is nothing to merge. Idle-or-deploy hint.
        // Covers the three common default-branch names; an exotic default
        // (e.g. `develop`) falls through to the safe fallback variant —
        // that's acceptable degradation, the other test below locks it in.
        for branch in ["main", "master", "trunk"] {
            let payload = json!({
                "action": "completed",
                "check_suite": {
                    "conclusion": "success",
                    "head_sha": "11112222",
                    "head_branch": branch,
                    "pull_requests": [],
                },
            });
            let d = classify("check_suite", &payload, "cr", &[]);
            let prompt = forward(&d).expect("forwarded");
            assert!(
                prompt.contains("post-merge"),
                "{branch}: post-merge hint missing: {prompt}"
            );
            assert!(
                prompt.contains("default branch"),
                "{branch}: default-branch wording missing: {prompt}"
            );
            assert!(
                !prompt.contains("merge NOW"),
                "{branch}: must not say merge-now on default branch: {prompt}"
            );
        }
    }

    #[test]
    fn ci_success_pr_merge_hint_omits_delete_branch_by_default() {
        // `--delete-branch` was previously hard-coded in the hint, which
        // fails on PRs whose head is a protected branch. Now the default
        // hint drops it; the session can append it when safe.
        let payload = json!({
            "action": "completed",
            "check_suite": {
                "conclusion": "success",
                "head_sha": "abc12345",
                "head_branch": "feat/foo",
                "pull_requests": [{"number": 1}],
            },
        });
        let d = classify("check_suite", &payload, "cr", &[]);
        let prompt = forward(&d).expect("forwarded");
        assert!(
            prompt.contains("--squash"),
            "squash flag missing from hint: {prompt}"
        );
        assert!(
            prompt.contains("append --delete-branch only if"),
            "delete-branch must be conditional: {prompt}"
        );
        // Regression guard: no unconditional `--squash --delete-branch`.
        assert!(
            !prompt.contains("--squash --delete-branch"),
            "unconditional --delete-branch leaked back into hint: {prompt}"
        );
    }

    #[test]
    fn ci_success_on_feature_branch_without_pr_has_safe_fallback_hint() {
        // Direct push to a feature branch with no PR opened yet — don't
        // fabricate a merge instruction (there is nothing to merge).
        let payload = json!({
            "action": "completed",
            "check_suite": {
                "conclusion": "success",
                "head_sha": "33334444",
                "head_branch": "wip/experiment",
                "pull_requests": [],
            },
        });
        let d = classify("check_suite", &payload, "cr", &[]);
        let prompt = forward(&d).expect("forwarded");
        assert!(prompt.contains(" | next: "), "hint missing: {prompt}");
        assert!(
            !prompt.contains("merge NOW"),
            "must not fabricate merge: {prompt}"
        );
    }

    #[test]
    fn ci_failure_hint_forbids_diagnose_and_stop() {
        // Regression guard: user explicitly called out that "it's pre-existing,
        // skip" is unacceptable. The hint must say a failure wake is a call
        // to action, and must include the literal anti-skip clause so future
        // rewrites can't silently drop it.
        let payload = json!({
            "action": "completed",
            "check_suite": {
                "conclusion": "failure",
                "head_sha": "deadbeef",
                "head_branch": "feat/x",
                "pull_requests": [{"number": 7}],
            },
        });
        let d = classify("check_suite", &payload, "cr", &[]);
        let prompt = forward(&d).expect("forwarded");
        assert!(prompt.contains("call to action"), "{prompt}");
        assert!(
            prompt.contains("pre-existing"),
            "anti-skip clause missing: {prompt}"
        );
        // Failure hint is identical regardless of PR/branch — a failure is
        // a failure. The merge-now clause must NOT appear.
        assert!(
            !prompt.contains("merge NOW"),
            "failure must not say merge: {prompt}"
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
        let prompt = forward(&d).expect("forwarded");
        assert!(
            prompt.starts_with(
                "ghnotify code-review-complete: repo=GitHub.Issues pr=123 reviewer=copilot[bot] state=commented action=submitted"
            ),
            "prefix wrong: {prompt}"
        );
        assert!(prompt.contains(" | next: "), "hint missing: {prompt}");
        assert!(
            prompt.contains("will NOT auto re-review"),
            "anti-wait clause missing: {prompt}"
        );
    }

    #[test]
    fn pr_review_edited_is_forwarded() {
        // Copilot's reviewer bot is observed emitting `edited` when it finalizes
        // its review body. The content is still a real review, so forward.
        let payload = json!({
            "action": "edited",
            "review": {"user": {"login": "copilot-pull-request-reviewer"}, "state": "commented"},
            "pull_request": {"number": 361},
        });
        let d = classify("pull_request_review", &payload, "GitHub.Issues", &[]);
        let prompt = forward(&d).expect("forwarded");
        assert!(
            prompt.starts_with(
                "ghnotify code-review-complete: repo=GitHub.Issues pr=361 reviewer=copilot-pull-request-reviewer state=commented action=edited"
            ),
            "prefix wrong: {prompt}"
        );
        assert!(prompt.contains(" | next: "));
    }

    #[test]
    fn pr_review_dismissed_is_dropped() {
        let d = classify(
            "pull_request_review",
            &json!({"action": "dismissed"}),
            "x",
            &[],
        );
        assert!(matches!(d, Decision::Drop { .. }));
    }

    #[test]
    fn pr_review_unknown_action_is_dropped() {
        let d = classify(
            "pull_request_review",
            &json!({"action": "some_future_action"}),
            "x",
            &[],
        );
        assert!(matches!(d, Decision::Drop { .. }));
    }

    #[test]
    fn pr_review_submitted_approved_is_forwarded() {
        let payload = json!({
            "action": "submitted",
            "review": {"user": {"login": "human-reviewer"}, "state": "approved"},
            "pull_request": {"number": 42},
        });
        let d = classify("pull_request_review", &payload, "cr", &[]);
        let prompt = forward(&d).expect("forwarded");
        assert!(
            prompt.starts_with(
                "ghnotify code-review-complete: repo=cr pr=42 reviewer=human-reviewer state=approved action=submitted"
            ),
            "prefix wrong: {prompt}"
        );
        // approved → merge-now hint, NOT the read-and-fix hint.
        assert!(
            prompt.contains("merge now"),
            "approved merge hint missing: {prompt}"
        );
        assert!(
            !prompt.contains("fix ALL"),
            "approved must not tell me to fix anything: {prompt}"
        );
    }

    #[test]
    fn pr_review_submitted_changes_requested_is_forwarded() {
        let payload = json!({
            "action": "submitted",
            "review": {"user": {"login": "strict-reviewer"}, "state": "changes_requested"},
            "pull_request": {"number": 7},
        });
        let d = classify("pull_request_review", &payload, "cr", &[]);
        let prompt = forward(&d).expect("forwarded");
        assert!(
            prompt.starts_with(
                "ghnotify code-review-complete: repo=cr pr=7 reviewer=strict-reviewer state=changes_requested action=submitted"
            ),
            "prefix wrong: {prompt}"
        );
        assert!(prompt.contains("fix every item"), "hint wrong: {prompt}");
        assert!(
            prompt.contains("will NOT auto re-review"),
            "anti-wait clause missing: {prompt}"
        );
    }

    #[test]
    fn pr_review_submitted_with_missing_fields_uses_fallbacks() {
        // Robustness: malformed payload must not panic and must still forward
        // a usable prompt with "?" sentinels.
        let payload = json!({"action": "submitted"});
        let d = classify("pull_request_review", &payload, "cr", &[]);
        let prompt = forward(&d).expect("forwarded");
        assert!(
            prompt.starts_with(
                "ghnotify code-review-complete: repo=cr pr=? reviewer=? state=? action=submitted"
            ),
            "prefix wrong: {prompt}"
        );
        assert!(prompt.contains(" | next: "), "hint missing: {prompt}");
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
    fn pr_review_submitted_with_body_includes_excerpt() {
        let payload = json!({
            "action": "submitted",
            "review": {
                "user": {"login": "copilot-pull-request-reviewer"},
                "state": "commented",
                "body": "Found 3 issues: missing null check in parser.rs, wrong error handling in importer.rs, and a race in fetcher.rs.",
            },
            "pull_request": {"number": 490},
        });
        let d = classify("pull_request_review", &payload, "cr", &[]);
        let prompt = forward(&d).expect("forwarded");
        assert!(
            prompt.starts_with(
                "ghnotify code-review-complete: repo=cr pr=490 reviewer=copilot-pull-request-reviewer state=commented action=submitted body=\""
            ),
            "prompt prefix wrong: {prompt}"
        );
        assert!(prompt.contains("Found 3 issues"), "body missing: {prompt}");
    }

    #[test]
    fn pr_review_body_is_truncated_to_200_chars_with_ellipsis() {
        let long = "x".repeat(500);
        let payload = json!({
            "action": "submitted",
            "review": {"user": {"login": "r"}, "state": "commented", "body": long},
            "pull_request": {"number": 1},
        });
        let d = classify("pull_request_review", &payload, "cr", &[]);
        let prompt = forward(&d).expect("forwarded");
        // Ellipsis is budgeted inside the 200-char cap: 199 x chars + `…` =
        // 200 chars total inside the quotes, not 201. The body excerpt is
        // followed by the next-action hint.
        let marker = format!(" body=\"{}…\"", "x".repeat(199));
        assert!(prompt.contains(&marker), "excerpt shape wrong: {prompt}");
        // Locked in: extract the body excerpt between the opening quote and
        // its matching closing quote (followed by ` | next: `) and count
        // the chars.
        let body_open = prompt.find(" body=\"").unwrap() + " body=\"".len();
        let body_close = prompt[body_open..]
            .find("\" | next: ")
            .map(|i| body_open + i)
            .expect("body close before hint");
        let excerpt = &prompt[body_open..body_close];
        assert_eq!(
            excerpt.chars().count(),
            200,
            "excerpt must be exactly 200 chars including ellipsis: {excerpt:?}"
        );
    }

    #[test]
    fn pr_review_body_all_control_chars_yields_no_body_field() {
        // str::trim only strips whitespace, not control chars like ESC/BEL,
        // so a body of pure control bytes would sail past the early empty
        // guard, be sanitized into a run of spaces, and emit ` body="   "`
        // — visually empty but still there, pure noise. Nothing-visible
        // must collapse back to no body field at all.
        for body in ["\u{001b}\u{0007}", "\u{0000}\u{007f}\u{000b}"] {
            let payload = json!({
                "action": "submitted",
                "review": {"user": {"login": "r"}, "state": "commented", "body": body},
                "pull_request": {"number": 1},
            });
            let d = classify("pull_request_review", &payload, "cr", &[]);
            let prompt = forward(&d).expect("forwarded");
            assert!(!prompt.contains("body="), "empty body leaked: {prompt:?}");
        }
    }

    #[test]
    fn check_suite_on_refs_pull_synthetic_ref_is_dropped() {
        // GitHub dispatches a parallel check_suite for every PR on the
        // synthetic `refs/pull/N/head` ref, in addition to the one on the
        // PR's branch name. Forwarding both wakes the session twice for
        // the same CI run. Drop the synthetic-ref one.
        for head_branch in ["refs/pull/4/head", "refs/pull/123/merge"] {
            let payload = json!({
                "action": "completed",
                "check_suite": {
                    "conclusion": "success",
                    "head_sha": "ca6a94d2",
                    "head_branch": head_branch,
                    "pull_requests": [],
                },
            });
            assert!(
                matches!(
                    classify("check_suite", &payload, "ghnotify", &[]),
                    Decision::Drop { .. }
                ),
                "refs/pull/ head_branch {head_branch} should be dropped",
            );
        }
    }

    #[test]
    fn check_suite_refs_pull_lookalikes_are_not_dropped_by_synthetic_filter() {
        // The filter must match the exact shape `refs/pull/<digits>/(head|merge)`
        // — a plain `starts_with("refs/pull/")` would over-match any
        // user-authored branch sharing the prefix. Legitimate (if unusual)
        // branch names below must still reach the forward path.
        for head_branch in [
            "refs/pull/foo",           // no trailing /head or /merge
            "refs/pull/foo/bar",       // non-numeric "number"
            "refs/pull/4/patch",       // wrong tail segment
            "refs/pull//head",         // empty number
            "refs/pull/4/head/extra",  // trailing path component
            "refs/pulls/4/head",       // wrong prefix spelling
            "refs/pull-custom/4/head", // prefix not matching exactly
        ] {
            let payload = json!({
                "action": "completed",
                "check_suite": {
                    "conclusion": "success",
                    "head_sha": "abc12345",
                    "head_branch": head_branch,
                    "pull_requests": [],
                },
            });
            let d = classify("check_suite", &payload, "ghnotify", &[]);
            assert!(
                matches!(d, Decision::Forward { .. }),
                "lookalike {head_branch:?} should NOT be filtered as synthetic",
            );
        }
    }

    #[test]
    fn check_suite_on_named_branch_is_still_forwarded() {
        // Guard against over-filtering: a genuine feature-branch
        // check_suite must continue to wake the session. Only the
        // refs/pull/… synthetic-ref variant is suppressed.
        let payload = json!({
            "action": "completed",
            "check_suite": {
                "conclusion": "success",
                "head_sha": "ca6a94d2",
                "head_branch": "fix/sanitize-excerpt-control-chars",
                "pull_requests": [{"number": 4}],
            },
        });
        let d = classify("check_suite", &payload, "ghnotify", &[]);
        assert!(
            matches!(d, Decision::Forward { .. }),
            "named-branch check_suite must be forwarded",
        );
    }

    #[test]
    fn pr_review_body_control_chars_are_replaced_with_spaces() {
        // Security: any C0/C1 control character in a review body must not
        // survive into the prompt. A raw ESC byte followed by CSI bytes would
        // be interpreted as a terminal escape sequence by the receiving
        // Claude TUI (ANSI cursor-up, paste-bracket injection, etc.).
        // Untrusted GitHub content must be declawed to spaces.
        let payload = json!({
            "action": "submitted",
            "review": {
                "user": {"login": "attacker"},
                "state": "commented",
                // ESC [ 2 J   (clear screen), BEL, NUL, DEL
                "body": "hi\u{001b}[2Jbye\u{0007}\u{0000}end\u{007f}tail",
            },
            "pull_request": {"number": 1},
        });
        let d = classify("pull_request_review", &payload, "cr", &[]);
        let prompt = forward(&d).expect("forwarded");
        // No raw ESC / BEL / NUL / DEL in the output.
        assert!(!prompt.contains('\u{001b}'), "ESC leaked: {prompt:?}");
        assert!(!prompt.contains('\u{0007}'), "BEL leaked: {prompt:?}");
        assert!(!prompt.contains('\u{0000}'), "NUL leaked: {prompt:?}");
        assert!(!prompt.contains('\u{007f}'), "DEL leaked: {prompt:?}");
        // Each control char becomes a space — the surrounding visible text
        // is preserved so the reader still sees "hi  [2Jbye  end tail".
        assert!(prompt.contains(" body=\"hi [2Jbye  end tail\""), "{prompt}");
    }

    #[test]
    fn pr_review_body_newlines_are_collapsed_to_spaces() {
        let payload = json!({
            "action": "submitted",
            "review": {"user": {"login": "r"}, "state": "commented", "body": "line1\nline2\r\nline3"},
            "pull_request": {"number": 1},
        });
        let d = classify("pull_request_review", &payload, "cr", &[]);
        let prompt = forward(&d).expect("forwarded");
        assert!(prompt.contains(" body=\"line1 line2  line3\""), "{prompt}");
        assert!(!prompt.contains('\n'));
    }

    #[test]
    fn pr_review_empty_body_omits_body_field() {
        // An empty or whitespace-only review body (Copilot sometimes only
        // posts per-line comments with no summary) must NOT add an empty
        // body="" to the prompt — noise without signal.
        for body in ["", "   ", "\n\n"] {
            let payload = json!({
                "action": "submitted",
                "review": {"user": {"login": "r"}, "state": "commented", "body": body},
                "pull_request": {"number": 1},
            });
            let d = classify("pull_request_review", &payload, "cr", &[]);
            let prompt = forward(&d).expect("forwarded");
            assert!(!prompt.contains("body="), "body leaked: {prompt}");
        }
    }

    #[test]
    fn pr_review_hint_varies_by_state() {
        // Commented / changes_requested / unknown → fix+push+merge, no wait.
        // Approved → merge now, no further review.
        for state in ["commented", "changes_requested", "weird-future-state"] {
            let payload = json!({
                "action": "submitted",
                "review": {"user": {"login": "r"}, "state": state},
                "pull_request": {"number": 1},
            });
            let d = classify("pull_request_review", &payload, "cr", &[]);
            let prompt = forward(&d).expect("forwarded");
            assert!(
                prompt.contains("will NOT auto re-review"),
                "{state}: anti-wait clause missing: {prompt}"
            );
            assert!(
                prompt.contains("push") && prompt.contains("merge"),
                "{state}: fix/push/merge verbs missing: {prompt}"
            );
        }
        // Approved: no "fix" instruction, just merge.
        let payload = json!({
            "action": "submitted",
            "review": {"user": {"login": "r"}, "state": "approved"},
            "pull_request": {"number": 1},
        });
        let d = classify("pull_request_review", &payload, "cr", &[]);
        let prompt = forward(&d).expect("forwarded");
        assert!(
            prompt.contains("merge now"),
            "approved hint wrong: {prompt}"
        );
        assert!(
            !prompt.contains("fix ALL") && !prompt.contains("fix every item"),
            "approved must not prescribe fixes: {prompt}"
        );
        assert!(
            prompt.contains("do not wait"),
            "approved must still tell me not to wait: {prompt}"
        );
    }

    #[test]
    fn pr_review_hint_mentions_copilot_review_slash_command() {
        // The hint is the last line of defense against the "wait for Copilot
        // re-review" anti-pattern. It must tell the reader how to explicitly
        // request a re-review when (and only when) it's actually warranted.
        let payload = json!({
            "action": "submitted",
            "review": {"user": {"login": "r"}, "state": "commented"},
            "pull_request": {"number": 1},
        });
        let d = classify("pull_request_review", &payload, "cr", &[]);
        let prompt = forward(&d).expect("forwarded");
        assert!(
            prompt.contains("/copilot review"),
            "re-review escape hatch missing: {prompt}"
        );
    }

    #[test]
    fn pr_review_body_double_quotes_are_escaped_to_singles() {
        // The prompt wraps body in double-quotes so shell/tmux see one arg;
        // inner " would break the quoting — collapse to ' instead.
        let payload = json!({
            "action": "submitted",
            "review": {"user": {"login": "r"}, "state": "commented", "body": "He said \"ship it\""},
            "pull_request": {"number": 1},
        });
        let d = classify("pull_request_review", &payload, "cr", &[]);
        let prompt = forward(&d).expect("forwarded");
        assert!(prompt.contains(" body=\"He said 'ship it'\""), "{prompt}");
    }

    #[test]
    fn pr_review_comment_created_is_forwarded_with_path_line_body() {
        let payload = json!({
            "action": "created",
            "comment": {
                "body": "This should use `?` instead of `unwrap`.",
                "path": "src/parser.rs",
                "line": 42,
                "user": {"login": "copilot-pull-request-reviewer"},
            },
            "pull_request": {"number": 490},
        });
        let d = classify("pull_request_review_comment", &payload, "cr", &[]);
        assert_eq!(
            forward(&d),
            Some(
                "ghnotify review-comment: repo=cr pr=490 author=copilot-pull-request-reviewer file=src/parser.rs:42 body=\"This should use `?` instead of `unwrap`.\""
            ),
        );
    }

    #[test]
    fn pr_review_comment_edited_and_deleted_are_dropped() {
        // Only `created` wakes the session. Edits/deletes on comments
        // already consumed would be churn.
        for action in ["edited", "deleted"] {
            let payload = json!({
                "action": action,
                "comment": {"body": "x", "path": "a", "line": 1, "user": {"login": "u"}},
                "pull_request": {"number": 1},
            });
            assert!(
                matches!(
                    classify("pull_request_review_comment", &payload, "x", &[]),
                    Decision::Drop { .. }
                ),
                "action {action} should be dropped",
            );
        }
    }

    #[test]
    fn pr_review_comment_falls_back_to_original_line_if_line_missing() {
        // On outdated comments GitHub sets `line: null` but keeps
        // `original_line`. Prefer either over a "?" in the prompt.
        let payload = json!({
            "action": "created",
            "comment": {
                "body": "nit: trailing whitespace",
                "path": "x.rs",
                "line": null,
                "original_line": 7,
                "user": {"login": "u"},
            },
            "pull_request": {"number": 1},
        });
        let d = classify("pull_request_review_comment", &payload, "cr", &[]);
        assert_eq!(
            forward(&d),
            Some(
                "ghnotify review-comment: repo=cr pr=1 author=u file=x.rs:7 body=\"nit: trailing whitespace\""
            ),
        );
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

    // ---- compute_ci_aggregate ----

    /// A single completed + success suite → all-success. The smallest
    /// shape that must resolve cleanly; repos with only one App installed
    /// (no GitGuardian, no Copilot) should not see any behavior change.
    #[test]
    fn aggregate_single_completed_success() {
        let suites = [("completed", Some("success"))];
        assert_eq!(
            compute_ci_aggregate(suites.iter().map(|(s, c)| (*s, *c))),
            CiAggregate::AllSuccess,
        );
    }

    /// The exact shape of the bug: GitGuardian finishes first, main CI is
    /// still running. Aggregate must be InProgress so the wake defers.
    #[test]
    fn aggregate_one_done_others_running_is_in_progress() {
        let suites = [
            ("completed", Some("success")), // gitguardian
            ("in_progress", None),          // github-actions main CI
        ];
        assert_eq!(
            compute_ci_aggregate(suites.iter().map(|(s, c)| (*s, *c))),
            CiAggregate::InProgress,
        );
    }

    /// Any non-`completed` status — queued, in_progress, waiting,
    /// requested, pending — must defer. Exhaust the non-terminal set so
    /// a newly introduced GitHub status value can't silently pass as
    /// "done" and resurrect the premature-wake bug.
    #[test]
    fn aggregate_any_non_completed_status_is_in_progress() {
        for non_terminal in ["queued", "in_progress", "waiting", "requested", "pending"] {
            let suites = [
                ("completed", Some("success")),
                (non_terminal, None),
                ("completed", Some("success")),
            ];
            assert_eq!(
                compute_ci_aggregate(suites.iter().map(|(s, c)| (*s, *c))),
                CiAggregate::InProgress,
                "non-terminal status {non_terminal:?} must defer",
            );
        }
    }

    /// All completed, one failed → aggregate is failure. Failure wins
    /// even when the triggering event was one of the successful suites.
    #[test]
    fn aggregate_one_failure_wins_over_many_successes() {
        let suites = [
            ("completed", Some("success")),
            ("completed", Some("failure")),
            ("completed", Some("success")),
        ];
        assert_eq!(
            compute_ci_aggregate(suites.iter().map(|(s, c)| (*s, *c))),
            CiAggregate::HasFailure,
        );
    }

    /// Every non-pass conclusion surfaces as HasFailure — especially
    /// `None` on a completed suite (unusual, but treat as suspicious
    /// rather than silently-passing).
    #[test]
    fn aggregate_non_success_conclusions_are_failures() {
        for bad in [
            Some("failure"),
            Some("cancelled"),
            Some("timed_out"),
            Some("action_required"),
            Some("startup_failure"),
            None,
            Some("some_future_conclusion"),
        ] {
            let suites = [("completed", Some("success")), ("completed", bad)];
            assert_eq!(
                compute_ci_aggregate(suites.iter().map(|(s, c)| (*s, *c))),
                CiAggregate::HasFailure,
                "conclusion {bad:?} must count as failure",
            );
        }
    }

    /// `neutral` / `skipped` / `stale` are not failures. `stale` in
    /// particular is GitHub's marker for "a newer commit superseded this
    /// run" — treating it as a failure would turn every rebase into a
    /// false ci-failure wake.
    #[test]
    fn aggregate_neutral_skipped_stale_are_not_failures() {
        for ok in [Some("neutral"), Some("skipped"), Some("stale")] {
            let suites = [("completed", Some("success")), ("completed", ok)];
            assert_eq!(
                compute_ci_aggregate(suites.iter().map(|(s, c)| (*s, *c))),
                CiAggregate::AllSuccess,
                "conclusion {ok:?} must not count as failure",
            );
        }
    }

    /// Empty input (gh returned no suites, perhaps because the commit
    /// isn't known yet) must be InProgress so the webhook handler
    /// fails open to the single-event prompt rather than suppressing
    /// the wake entirely on an ambiguous response.
    #[test]
    fn aggregate_empty_input_is_in_progress() {
        let suites: [(&str, Option<&str>); 0] = [];
        assert_eq!(
            compute_ci_aggregate(suites.iter().map(|(s, c)| (*s, *c))),
            CiAggregate::InProgress,
        );
    }

    // ---- extract_ci_info / build_ci_prompt ----

    /// The full happy-path payload shape we see from GitHub. Exercises
    /// every field extraction and verifies build_ci_prompt wires them
    /// into the expected prompt format.
    #[test]
    fn extract_ci_info_full_payload_round_trips_through_build_ci_prompt() {
        let payload = json!({
            "action": "completed",
            "check_suite": {
                "conclusion": "success",
                "head_sha": "abc123456789deadbeef",
                "head_branch": "feat/foo",
                "pull_requests": [{"number": 531}, {"number": 532}],
            },
            "repository": {"full_name": "Olbrasoft/cr"},
        });
        let info = extract_ci_info(&payload, "cr");
        assert_eq!(info.repo, "cr");
        assert_eq!(info.full_name, "Olbrasoft/cr");
        assert_eq!(info.head_sha, "abc123456789deadbeef");
        assert_eq!(info.head_short, "abc12345");
        assert_eq!(info.head_branch, "feat/foo");
        assert_eq!(info.pr_str, "531,532");
        let prompt = build_ci_prompt(&info, "success");
        assert!(
            prompt.starts_with(
                "ghnotify ci-success: repo=cr status=success pr=531,532 branch=feat/foo head=abc12345"
            ),
            "prefix wrong: {prompt}"
        );
    }

    /// Aggregate-driven override: the firing event reports `success`
    /// (this check_suite passed), but a sibling suite on the same commit
    /// failed, so the aggregate is `HasFailure`. build_ci_prompt must
    /// honor the caller-supplied conclusion — not re-derive it from the
    /// payload — and emit `ci-failure` with the failure hint.
    #[test]
    fn build_ci_prompt_with_aggregate_failure_overrides_single_event_success() {
        let info = CiInfo {
            repo: "cr".into(),
            pr_str: "7".into(),
            head_branch: "feat/x".into(),
            head_short: "deadbeef".into(),
            head_sha: "deadbeef".into(),
            full_name: "Olbrasoft/cr".into(),
        };
        let prompt = build_ci_prompt(&info, "failure");
        assert!(
            prompt.starts_with("ghnotify ci-failure:"),
            "must escalate to failure: {prompt}"
        );
        assert!(
            prompt.contains("call to action"),
            "must include failure hint: {prompt}"
        );
        assert!(
            !prompt.contains("merge NOW"),
            "failure override must not suggest merge: {prompt}"
        );
    }

    /// Missing `/repository/full_name` and empty `head_sha` must not
    /// crash — extract_ci_info returns a lenient CiInfo with empty
    /// strings in those slots, and the webhook handler's aggregate
    /// query treats either-empty as "can't check" → fail-open to the
    /// single-event prompt. Locks in the behavior the existing
    /// stub-payload tests rely on.
    #[test]
    fn extract_ci_info_missing_optional_fields_degrades_gracefully() {
        let payload = json!({
            "action": "completed",
            "check_suite": {
                "conclusion": "success",
                "head_branch": "feat/x",
                "pull_requests": [],
            },
        });
        let info = extract_ci_info(&payload, "cr");
        assert_eq!(info.head_sha, "");
        assert_eq!(info.full_name, "");
        assert_eq!(info.pr_str, "none");
        // Prompt still builds — malformed payload doesn't crash the pipeline.
        let prompt = build_ci_prompt(&info, "success");
        assert!(prompt.starts_with("ghnotify ci-success:"), "{prompt}");
    }
}
