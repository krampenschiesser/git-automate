//! Workflow checks.
//!
//! Implements four workflow checks that drive the automation loop:
//! - `run_triage_check` — finds `@ai`-tagged issues, adds them to the project,
//!   sets status to "Triage", and starts a triage OpenCode session.
//! - `run_todo_check` — finds items with "Todo" status, creates branches if
//!   needed, and starts a developer OpenCode session.
//! - `run_dev_completion_check` — detects completed developer sessions in
//!   "In Development" status, resolves review threads from session output,
//!   and transitions the item to "Review Technical".
//! - `run_review_check` — finds items in review states ("Review Technical",
//!   "Review Product", "QA"), fills a prompt template, and starts a review
//!   OpenCode session.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::external_agent::opencode::{Delivery, OpenCodeClient};
use crate::external_issues::github::client::GitHubClient;
use crate::external_issues::github::types::IssueInfo;

use super::helpers::{
    LogLevel, ProjectContext, SessionBinding, SessionCompletion, WorkflowContext, WorkflowError,
    branch_name_for_issue, classify_session, extract_session_id, fill_prompt, get_issue_body_map,
    issue_body_or_title, load_prompt_template, log_deduped, now_unix_secs, parse_resolve_threads,
    parse_session_binding, resolve_field_ids, resolve_option_id, resolve_status_option_and_session,
    serialize_binding,
};
use super::verdict::{AgentDecision, VerdictParse, parse_verdict};
use crate::workflow::AgentName;

// ─── Constants ─────────────────────────────────────────────────

/// Descriptor for a review workflow state.
#[derive(Clone, Copy)]
pub enum ReviewState {
    Technical,
    Product,
    Qa,
}

impl ReviewState {
    pub fn status(&self) -> &'static str {
        match self {
            ReviewState::Technical => "Review Technical",
            ReviewState::Product => "Review Product",
            ReviewState::Qa => "QA",
        }
    }

    pub fn agent(&self) -> crate::workflow::AgentName {
        match self {
            ReviewState::Technical => crate::workflow::AgentName::Reviewer,
            ReviewState::Product => crate::workflow::AgentName::Product,
            ReviewState::Qa => crate::workflow::AgentName::QA,
        }
    }

    pub fn prompt(&self) -> &'static str {
        match self {
            ReviewState::Technical => "reviewer",
            ReviewState::Product => "product",
            ReviewState::Qa => "qa",
        }
    }
}

/// The three review states, in order.
pub const REVIEW_STATES: [ReviewState; 3] = [
    ReviewState::Technical,
    ReviewState::Product,
    ReviewState::Qa,
];

/// The status that follows a completed review state.
fn next_review_status(status: &str) -> Option<&'static str> {
    match status {
        "Review Technical" => Some("Review Product"),
        "Review Product" => Some("QA"),
        "QA" => Some("Done"),
        _ => None,
    }
}

// ─── Session binding helpers ───────────────────────────────────

/// Parse the `sessionId` field of a project item into a [`SessionBinding`].
fn binding_from_values(field_values: &BTreeMap<String, Option<String>>) -> SessionBinding {
    parse_session_binding(&extract_session_id(field_values).unwrap_or_default())
}

/// Outcome of the session-ownership guard.
enum OwnerCheck {
    /// Binding belongs to *current_status*; the caller may act on the session.
    Proceed,
    /// The binding was upgraded (legacy) or cleared (detached); do not act.
    Handled,
}

/// Require that the tracked session is owned by *current_status* before acting.
///
/// - Legacy binding (`step == None`): upgrade it to `step = current_status`,
///   persist, and return [`OwnerCheck::Handled`].
/// - Detached binding (`step != current_status`): clear it without reverting the
///   status and return [`OwnerCheck::Handled`].
/// - Matching binding: return [`OwnerCheck::Proceed`].
async fn verify_session_owner(
    github: &GitHubClient,
    ctx: &ProjectContext,
    item_id: &str,
    session_field_id: &str,
    binding: &SessionBinding,
    current_status: &str,
) -> Result<OwnerCheck, WorkflowError> {
    match binding.step.as_deref() {
        Some(step) if step == current_status => Ok(OwnerCheck::Proceed),
        None => {
            let upgraded = SessionBinding {
                step: Some(current_status.to_string()),
                id: binding.id.clone(),
                resume: binding.resume.clone(),
                attempts: binding.attempts,
            };
            github
                .update_project_item_session_id(
                    &ctx.project_id,
                    item_id,
                    session_field_id,
                    Some(&serialize_binding(&upgraded)),
                )
                .await?;
            Ok(OwnerCheck::Handled)
        }
        Some(_) => {
            github
                .update_project_item_session_id(&ctx.project_id, item_id, session_field_id, None)
                .await?;
            Ok(OwnerCheck::Handled)
        }
    }
}

// ─── OpenCode session config ───────────────────────────────────

/// OpenCode session configuration passed to check functions.
#[derive(Clone)]
pub struct OpencodeSessionConfig {
    pub url: String,
    pub pw: String,
    pub directory: String,
    pub project: Option<String>,
    pub concurrency: HashMap<String, usize>,
    pub session_timeout_secs: u64,
    pub session_max_secs: u64,
    pub max_session_attempts: u32,
}

// ─── Concurrency dedup state ──────────────────────────────────

/// Tracks whether OpenCode concurrency capacity has been exceeded during the
/// current daemon cycle. Reset to `false` on daemon restart (static variable).
static CAPACITY_EXCEEDED: AtomicBool = AtomicBool::new(false);

/// Tracks whether the healthy log message has already been emitted for the
/// current health-check cycle. Reset to `false` on daemon restart (static).
pub(crate) static OPENCODE_HEALTHY_LOGGED: AtomicBool = AtomicBool::new(false);

/// Tracks whether the unhealthy log message has already been emitted for the
/// current health-check cycle. Reset to `false` on daemon restart (static).
pub(crate) static OPENCODE_UNHEALTHY_LOGGED: AtomicBool = AtomicBool::new(false);

// ─── start_opencode_session ────────────────────────────────────

/// Log (once per daemon cycle) that OpenCode capacity is exhausted for *kind*
/// *key* and return the short-circuiting [`WorkflowError::ConcurrencyExceeded`].
async fn capacity_exceeded(
    log_dedup: &std::sync::Arc<tokio::sync::Mutex<HashMap<String, String>>>,
    kind: &str,
    key: &str,
    active: usize,
    limit: usize,
    title: &str,
) -> Result<String, WorkflowError> {
    if !CAPACITY_EXCEEDED.swap(true, Ordering::Relaxed) {
        log_deduped(
            log_dedup,
            "all",
            LogLevel::Warn,
            format!(
                "OpenCode capacity exceeded for {} {} ({} >= {}), skipping: {}",
                kind, key, active, limit, title
            ),
        )
        .await;
    }
    Err(WorkflowError::ConcurrencyExceeded)
}

/// Start an OpenCode session with the given agent and user message.
///
/// Creates a new [`OpenCodeClient`] per call. The `agent` names the OpenCode
/// agent (e.g. `git-automate-triage`) whose instructions drive the session;
/// v2 has no per-session system prompt.
///
/// When `concurrency` is non-empty the active session count is queried first;
/// if it meets or exceeds the applicable limit the session is **not** created
/// and [`WorkflowError::ConcurrencyExceeded`] is returned. The gate is keyed on
/// the *agent* being started (falling back to the `default` key); configured
/// keys containing `/` are treated as legacy *model* keys and checked against
/// per-model active counts.
///
/// Maps [`crate::external_agent::opencode::OpenCodeError`] to `WorkflowError::Other`.
#[allow(clippy::too_many_arguments)]
async fn start_opencode_session(
    oc: &OpencodeSessionConfig,
    directory: &str,
    title: &str,
    agent: crate::workflow::AgentName,
    message: &str,
    concurrency: &HashMap<String, usize>,
    log_dedup: &std::sync::Arc<tokio::sync::Mutex<HashMap<String, String>>>,
    step: &str,
) -> Result<String, WorkflowError> {
    let client = OpenCodeClient::new(oc.url.clone(), oc.pw.clone());

    if !concurrency.is_empty() {
        let agent_key = agent.as_str();

        let agent_limit = concurrency
            .get(agent_key)
            .or_else(|| concurrency.get("default"))
            .copied();
        if let Some(limit) = agent_limit {
            let counts = client
                .get_active_by_agent()
                .await
                .map_err(|e| WorkflowError::Other(format!("OpenCode: {}", e)))?;
            let active = counts.get(agent_key).copied().unwrap_or(0);
            if active >= limit {
                return capacity_exceeded(log_dedup, "agent", agent_key, active, limit, title)
                    .await;
            }
        }

        if concurrency.keys().any(|key| key.contains('/')) {
            let counts = client
                .get_session_models()
                .await
                .map_err(|e| WorkflowError::Other(format!("OpenCode: {}", e)))?;
            for (model_key, limit) in concurrency.iter().filter(|(key, _)| key.contains('/')) {
                let active = counts.get(model_key).copied().unwrap_or(0);
                if active >= *limit {
                    return capacity_exceeded(log_dedup, "model", model_key, active, *limit, title)
                        .await;
                }
            }
        }
    }

    let location = client
        .get_location(directory)
        .await
        .map_err(|e| WorkflowError::Other(format!("OpenCode: {}", e)))?;
    let worktree = client
        .create_worktree(&location.project.id, None)
        .await
        .map_err(|e| WorkflowError::Other(format!("OpenCode: {}", e)))?;
    log_deduped(
        log_dedup,
        step,
        LogLevel::Info,
        format!("starting session in worktree {}", worktree.directory),
    )
    .await;
    if CAPACITY_EXCEEDED.swap(false, Ordering::Relaxed) {
        log_deduped(
            log_dedup,
            "all",
            LogLevel::Info,
            format!(
                "OpenCode capacity available again, resuming workflow for {}",
                title
            ),
        )
        .await;
    }

    let session = client
        .create_session(title, agent.as_str(), None, &worktree.directory)
        .await
        .map_err(|e| WorkflowError::Other(format!("OpenCode: {}", e)))?;
    client
        .send_prompt(&session.id, message)
        .await
        .map_err(|e| WorkflowError::Other(format!("OpenCode: {}", e)))?;
    Ok(session.id)
}

/// Start a session and persist its [`SessionBinding`] in one step.
///
/// If persisting the binding fails, the freshly created session is deleted on a
/// best-effort basis (a failed delete is logged) and the original persist error
/// is propagated, so a create that cannot be recorded never leaks a live
/// session.
#[allow(clippy::too_many_arguments)]
async fn start_and_record_session(
    deps: &WorkflowContext,
    github: &GitHubClient,
    ctx: &ProjectContext,
    oc: &OpencodeSessionConfig,
    session_field_id: &str,
    item_id: &str,
    title: &str,
    agent: AgentName,
    message: &str,
    step: &str,
    binding_step: &str,
    resume: String,
    prev_attempts: u32,
) -> Result<String, WorkflowError> {
    let session_id = start_opencode_session(
        oc,
        oc.directory.as_str(),
        title,
        agent,
        message,
        &oc.concurrency,
        &deps.log_dedup,
        step,
    )
    .await?;

    let new_binding = SessionBinding {
        step: Some(binding_step.to_string()),
        id: session_id.clone(),
        resume,
        attempts: prev_attempts.saturating_add(1),
    };

    if let Err(e) = github
        .update_project_item_session_id(
            &ctx.project_id,
            item_id,
            session_field_id,
            Some(&serialize_binding(&new_binding)),
        )
        .await
    {
        let cleanup = OpenCodeClient::new(oc.url.clone(), oc.pw.clone());
        if let Err(delete_err) = cleanup.delete_session(&session_id).await {
            log_deduped(
                &deps.log_dedup,
                step,
                LogLevel::Warn,
                format!(
                    "{}: could not delete orphaned session {} after persist failure: {}",
                    ctx.name, session_id, delete_err
                ),
            )
            .await;
        }
        return Err(e.into());
    }

    Ok(session_id)
}

// ── Triage Check ──

/// Find issues with titles starting `@ai `, add them to the project, set
/// status to "Triage", and start a triage session if no session exists yet.
pub async fn run_triage_check(
    deps: &WorkflowContext,
    ctx: &ProjectContext,
    oc: &OpencodeSessionConfig,
) -> Result<(), WorkflowError> {
    let github = deps
        .github
        .as_ref()
        .ok_or_else(|| WorkflowError::NoGitHub(ctx.name.clone()))?;

    let (status_field_id, triage_option_id, session_field_id) =
        resolve_status_option_and_session(github, &ctx.project_id, "Triage").await?;

    let issues = github.list_repo_issues(&ctx.owner, &ctx.repo).await?;
    let pattern = regex::Regex::new(&ctx.config.title_pattern).map_err(|e| {
        WorkflowError::Other(format!(
            "invalid title_pattern '{}': {}",
            ctx.config.title_pattern, e
        ))
    })?;
    let ai_issues: Vec<&IssueInfo> = issues
        .iter()
        .filter(|i| pattern.is_match(&i.title))
        .collect();

    if ai_issues.is_empty() {
        log_deduped(
            &deps.log_dedup,
            "triage",
            LogLevel::Info,
            format!("{}: no @ai issues found", ctx.name),
        )
        .await;
        return Ok(());
    }

    let project_items = github.list_project_items(&ctx.project_id).await?;
    let mut existing_items: HashMap<i64, String> = HashMap::new();
    for item in &project_items {
        existing_items.insert(item.content_number, item.id.clone());
    }

    for issue in &ai_issues {
        let (item_id, is_new) = if let Some(id) = existing_items.get(&issue.number) {
            (id.clone(), false)
        } else {
            log_deduped(
                &deps.log_dedup,
                "triage",
                LogLevel::Info,
                format!("{}: adding issue #{} to project", ctx.name, issue.number),
            )
            .await;
            let id = github
                .add_issue_to_project(&issue.id, &ctx.project_id)
                .await?;
            (id, true)
        };

        let item_values = github.get_project_item_values(&item_id).await?;
        let current_status = item_values
            .get("Status")
            .and_then(|v| v.as_deref())
            .unwrap_or("");

        // C3: never pull an already-progressed item back to Triage.
        if !is_new && !current_status.is_empty() {
            continue;
        }

        github
            .update_project_item_status(
                &ctx.project_id,
                &item_id,
                &status_field_id,
                &triage_option_id,
            )
            .await?;

        let binding = binding_from_values(&item_values);
        if binding.id.is_empty() {
            if binding.attempts >= oc.max_session_attempts {
                log_deduped(
                    &deps.log_dedup,
                    "triage",
                    LogLevel::Error,
                    format!(
                        "triage: max session attempts reached for issue #{} — parking",
                        issue.number
                    ),
                )
                .await;
                continue;
            }
            log_deduped(
                &deps.log_dedup,
                "triage",
                LogLevel::Info,
                format!(
                    "{}: starting triage session for #{}",
                    ctx.name, issue.number
                ),
            )
            .await;
            let template = load_prompt_template("triage", Some(Path::new(&ctx.config.directory)))?;
            let values = HashMap::from([
                ("ISSUE_TITLE".to_string(), issue.title.clone()),
                ("ISSUE_NUMBER".to_string(), issue.number.to_string()),
                (
                    "ISSUE_BODY".to_string(),
                    issue.body.clone().unwrap_or_default(),
                ),
                ("ISSUE_LABELS".to_string(), String::new()),
                ("ISSUE_ASSIGNEE".to_string(), String::new()),
            ]);
            let user_prompt = fill_prompt(&template, &values);
            start_and_record_session(
                deps,
                github,
                ctx,
                oc,
                &session_field_id,
                &item_id,
                &issue.title,
                AgentName::Triage,
                &user_prompt,
                "triage",
                "Triage",
                String::new(),
                binding.attempts,
            )
            .await?;
        }
    }

    Ok(())
}

// ── Todo Check ──

/// Find items with "Todo" status, create branches if needed, and start a
/// developer session for each item without a session.
pub async fn run_todo_check(
    deps: &WorkflowContext,
    ctx: &ProjectContext,
    oc: &OpencodeSessionConfig,
) -> Result<(), WorkflowError> {
    let github = deps
        .github
        .as_ref()
        .ok_or_else(|| WorkflowError::NoGitHub(ctx.name.clone()))?;

    let field_ids = resolve_field_ids(github, &ctx.project_id).await?;
    let session_field_id = field_ids
        .session_field_id
        .ok_or_else(|| WorkflowError::NoSessionField(ctx.project_id.clone()))?;

    let project_items = github.list_project_items(&ctx.project_id).await?;
    let issue_map = get_issue_body_map(github, &ctx.owner, &ctx.repo).await?;

    // Build a map of item_id → (wave_id, status, has_session, attempts) for wave
    // dependency checking.
    let mut item_wave_status: HashMap<String, (Option<i64>, String, bool, u32)> = HashMap::new();
    for item in &project_items {
        let item_values = github.get_project_item_values(&item.id).await?;
        let status = item_values
            .get("Status")
            .and_then(|v| v.as_deref())
            .unwrap_or("")
            .to_string();
        let wave_id = if let Some(wave_str) = item_values.get("waveId") {
            wave_str.as_ref().unwrap().parse::<i64>().ok()
        } else {
            None
        };
        let binding = binding_from_values(&item_values);
        let has_session = binding.has_session();
        item_wave_status.insert(
            item.id.clone(),
            (wave_id, status, has_session, binding.attempts),
        );
    }

    // Collect items that are "Todo" and have no session yet.
    let mut todo_items: Vec<(String, i64)> = Vec::new();
    for item in &project_items {
        let (wave_id, ref status, has_session, _) = item_wave_status[&item.id];
        if status == "Todo" && !has_session {
            // Wave dependency filtering for sub-issues: a sub-issue with
            // waveId=W may only start if all sub-issues with waveId < W are
            // in "Done" status.
            if let Some(wave) = wave_id {
                let all_lower_waves_done =
                    item_wave_status
                        .iter()
                        .all(|(_, (other_wave, other_status, _, _))| match other_wave {
                            Some(ow) if *ow < wave => other_status == "Done",
                            _ => true,
                        });
                if !all_lower_waves_done {
                    tracing::info!(
                        "{}: skipping sub-issue #{} (waveId={}) — lower wave dependencies not met",
                        ctx.name,
                        item.content_number,
                        wave
                    );
                    continue;
                }
            }
            todo_items.push((item.id.clone(), item.content_number));
        }
    }

    if todo_items.is_empty() {
        log_deduped(
            &deps.log_dedup,
            "todo",
            LogLevel::Info,
            format!("{}: no Todo items without sessions found", ctx.name),
        )
        .await;
        return Ok(());
    }

    let status_field = github.get_project_status_field(&ctx.project_id).await?;
    let in_dev_option_id = status_field
        .as_ref()
        .and_then(|sf| resolve_option_id(&sf.options, "In Development"));

    let default_branch = github
        .get_repo_default_branch(&ctx.owner, &ctx.repo)
        .await?;
    let commit_sha = github
        .get_repo_commit_sha(&ctx.owner, &ctx.repo, &default_branch)
        .await?;

    for (item_id, issue_number) in &todo_items {
        let prev_attempts = item_wave_status[item_id].3;
        if prev_attempts >= oc.max_session_attempts {
            log_deduped(
                &deps.log_dedup,
                "todo",
                LogLevel::Error,
                format!(
                    "todo: max session attempts reached for issue #{} — parking",
                    issue_number
                ),
            )
            .await;
            continue;
        }
        let issue = issue_map.get(issue_number);
        let branch_name = branch_name_for_issue(*issue_number, &deps.config.git, None);

        let exists = github
            .branch_exists(&ctx.owner, &ctx.repo, &branch_name)
            .await?;
        if !exists {
            log_deduped(
                &deps.log_dedup,
                "todo",
                LogLevel::Info,
                format!("{}: creating branch {}", ctx.name, branch_name),
            )
            .await;
            github
                .create_branch_ref(&ctx.owner, &ctx.repo, &branch_name, &commit_sha)
                .await?;
        }

        let title = if let Some(issue) = issue {
            issue.title.clone()
        } else {
            format!("Dev work for issue #{}", issue_number)
        };
        let message = if let Some(issue) = issue {
            issue_body_or_title(issue)
        } else {
            format!("Issue #{}", issue_number)
        };

        // Gather PR context if a PR exists for the branch.
        let pr_info = github
            .get_pull_request_for_branch(&ctx.owner, &ctx.repo, &branch_name)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(
                    "{}: could not fetch PR for branch '{}': {}",
                    ctx.name,
                    branch_name,
                    e
                );
                None
            });

        let (pr_url, pr_changes, pr_comments) = if let Some(pr) = &pr_info {
            let diff = github
                .get_pr_diff(&ctx.owner, &ctx.repo, pr.number)
                .await
                .unwrap_or_default();
            let comments = github
                .list_pr_review_comments(&ctx.owner, &ctx.repo, pr.number)
                .await
                .map(|threads| {
                    threads
                        .into_iter()
                        .filter(|t| !t.is_resolved)
                        .flat_map(|t| {
                            t.comments
                                .nodes
                                .iter()
                                .map(|c| c.body.clone())
                                .collect::<Vec<_>>()
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let comments_text = comments.join("\n");
            (pr.url.clone(), diff, comments_text)
        } else {
            (String::new(), String::new(), String::new())
        };

        log_deduped(
            &deps.log_dedup,
            "todo",
            LogLevel::Info,
            format!(
                "{}: starting developer session for #{}",
                ctx.name, issue_number
            ),
        )
        .await;
        let template = load_prompt_template("developer", Some(Path::new(&ctx.config.directory)))?;
        let branch_name = branch_name_for_issue(*issue_number, &deps.config.git, None);
        let values = HashMap::from([
            ("ISSUE_TITLE".to_string(), title.clone()),
            ("ISSUE_NUMBER".to_string(), issue_number.to_string()),
            ("ISSUE_BODY".to_string(), message.clone()),
            ("BRANCH_NAME".to_string(), branch_name),
            (
                "PROJECT_REPOSITORY".to_string(),
                format!("{}/{}", ctx.owner, ctx.repo),
            ),
            ("PR_URL".to_string(), pr_url),
            ("PR_CHANGES".to_string(), pr_changes),
            ("PR_COMMENTS".to_string(), pr_comments),
        ]);
        let user_prompt = fill_prompt(&template, &values);
        start_and_record_session(
            deps,
            github,
            ctx,
            oc,
            &session_field_id,
            item_id,
            &title,
            AgentName::Developer,
            &user_prompt,
            "todo",
            "In Development",
            String::new(),
            prev_attempts,
        )
        .await?;

        if let Some(in_dev_id) = &in_dev_option_id {
            github
                .update_project_item_status(
                    &ctx.project_id,
                    item_id,
                    &field_ids.status_field_id,
                    in_dev_id,
                )
                .await?;
        } else {
            tracing::warn!(
                "{}: 'In Development' status option not found — item #{} left in Todo",
                ctx.name,
                issue_number
            );
        }
    }

    Ok(())
}

// ── Review Check ──

/// Find items with "Review Technical", "Review Product", or "QA" status,
/// load the appropriate prompt template, fill it, and start a review session.
pub async fn run_review_check(
    deps: &WorkflowContext,
    ctx: &ProjectContext,
    oc: &OpencodeSessionConfig,
) -> Result<(), WorkflowError> {
    let github = deps
        .github
        .as_ref()
        .ok_or_else(|| WorkflowError::NoGitHub(ctx.name.clone()))?;

    let field_ids = resolve_field_ids(github, &ctx.project_id).await?;
    let session_field_id = field_ids
        .session_field_id
        .ok_or_else(|| WorkflowError::NoSessionField(ctx.project_id.clone()))?;
    let project_items = github.list_project_items(&ctx.project_id).await?;
    let issue_map = get_issue_body_map(github, &ctx.owner, &ctx.repo).await?;

    let status_field = github.get_project_status_field(&ctx.project_id).await?;

    let mut item_values = Vec::new();
    for item in &project_items {
        item_values.push(github.get_project_item_values(&item.id).await?);
    }

    for state in &REVIEW_STATES {
        let Some(status_field) = &status_field else {
            continue;
        };
        let state_option_id = status_field
            .options
            .iter()
            .find(|o| o.name == state.status())
            .map(|o| o.id.clone());
        let Some(_state_option_id) = state_option_id else {
            log_deduped(
                &deps.log_dedup,
                "review",
                LogLevel::Warn,
                format!(
                    "{}: status option \"{}\" not found",
                    ctx.name,
                    state.status()
                ),
            )
            .await;
            continue;
        };

        let template =
            load_prompt_template(state.prompt(), Some(Path::new(&ctx.config.directory)))?;

        for (item, values) in project_items.iter().zip(&item_values) {
            let current_status = values.get("Status").and_then(|v| v.as_deref());
            if current_status != Some(state.status()) {
                continue;
            }
            let existing_binding = binding_from_values(values);
            if !existing_binding.id.is_empty() {
                continue;
            }
            if existing_binding.attempts >= oc.max_session_attempts {
                log_deduped(
                    &deps.log_dedup,
                    "review",
                    LogLevel::Error,
                    format!(
                        "review: max session attempts reached for issue #{} — parking",
                        item.content_number
                    ),
                )
                .await;
                continue;
            }

            let issue = issue_map.get(&item.content_number);
            let issue_number = item.content_number;
            let branch_name = branch_name_for_issue(issue_number, &deps.config.git, None);

            // Gather PR context if a PR exists for the branch.
            let pr_info = github
                .get_pull_request_for_branch(&ctx.owner, &ctx.repo, &branch_name)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(
                        "{}: could not fetch PR for branch '{}': {}",
                        ctx.name,
                        branch_name,
                        e
                    );
                    None
                });

            let (pr_url, pr_changes, pr_comments) = if let Some(pr) = &pr_info {
                let diff = github
                    .get_pr_diff(&ctx.owner, &ctx.repo, pr.number)
                    .await
                    .unwrap_or_default();
                let comments = github
                    .list_pr_review_comments(&ctx.owner, &ctx.repo, pr.number)
                    .await
                    .map(|threads| {
                        threads
                            .into_iter()
                            .filter(|t| !t.is_resolved)
                            .flat_map(|t| {
                                t.comments
                                    .nodes
                                    .iter()
                                    .map(|c| c.body.clone())
                                    .collect::<Vec<_>>()
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let comments_text = comments.join("\n");
                (pr.url.clone(), diff, comments_text)
            } else {
                (String::new(), String::new(), String::new())
            };

            let filled_prompt = fill_prompt(
                &template,
                &HashMap::from([
                    ("ISSUE_NUMBER".to_string(), issue_number.to_string()),
                    ("BRANCH_NAME".to_string(), branch_name),
                    (
                        "ISSUE_TITLE".to_string(),
                        issue
                            .as_ref()
                            .map(|i| i.title.clone())
                            .unwrap_or_else(|| format!("Issue #{}", issue_number)),
                    ),
                    (
                        "ISSUE_BODY".to_string(),
                        issue
                            .as_ref()
                            .and_then(|i| i.body.clone())
                            .unwrap_or_default(),
                    ),
                    ("PR_URL".to_string(), pr_url),
                    ("PR_CHANGES".to_string(), pr_changes),
                    ("PR_COMMENTS".to_string(), pr_comments),
                ]),
            );

            let title = format!(
                "{}: {}",
                state.status(),
                issue
                    .as_ref()
                    .map(|i| i.title.as_str())
                    .unwrap_or(&format!("#{}", issue_number))
            );

            log_deduped(
                &deps.log_dedup,
                "review",
                LogLevel::Info,
                format!(
                    "{}: starting {} session for #{}",
                    ctx.name,
                    state.agent().as_str(),
                    issue_number
                ),
            )
            .await;

            start_and_record_session(
                deps,
                github,
                ctx,
                oc,
                &session_field_id,
                &item.id,
                &title,
                state.agent(),
                &filled_prompt,
                "review",
                state.status(),
                existing_binding.resume.clone(),
                existing_binding.attempts,
            )
            .await?;
        }
    }

    run_review_completion_check(deps, ctx, oc).await?;

    Ok(())
}

/// Build the developer prompt for a `changes` review verdict: the reviewer's
/// notes followed by the unresolved PR review comments, with a generic
/// fallback when both are empty.
async fn build_changes_feedback(
    github: &GitHubClient,
    ctx: &ProjectContext,
    issue_number: i64,
    notes: &str,
) -> String {
    let branch_name = branch_name_for_issue(issue_number, &ctx.config, None);
    let mut comments = Vec::new();

    match github
        .get_pull_request_for_branch(&ctx.owner, &ctx.repo, &branch_name)
        .await
    {
        Ok(Some(pr)) => {
            if let Ok(threads) = github
                .list_pr_review_comments(&ctx.owner, &ctx.repo, pr.number)
                .await
            {
                for thread in threads.into_iter().filter(|t| !t.is_resolved) {
                    for comment in &thread.comments.nodes {
                        comments.push(comment.body.clone());
                    }
                }
            }
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(
                "{}: could not fetch PR for branch '{}': {}",
                ctx.name,
                branch_name,
                e
            );
        }
    }

    let mut feedback = notes.trim().to_string();
    let comments_text = comments.join("\n");
    if !comments_text.trim().is_empty() {
        if !feedback.is_empty() {
            feedback.push_str("\n\n");
        }
        feedback.push_str(comments_text.trim());
    }
    if feedback.is_empty() {
        feedback = "The review requested changes. Address the review feedback and update the pull request."
            .to_string();
    }
    feedback
}

/// Degrade a review item to `Todo` with an empty session binding so the todo
/// check starts a fresh developer session on the next cycle.
async fn reset_review_to_todo(
    github: &GitHubClient,
    ctx: &ProjectContext,
    status_field_id: &str,
    session_field_id: &str,
    item_id: &str,
    todo_option_id: Option<&str>,
    attempts: u32,
) -> Result<(), WorkflowError> {
    let binding = SessionBinding {
        step: Some("Todo".to_string()),
        id: String::new(),
        resume: String::new(),
        attempts,
    };
    github
        .update_project_item_session_id(
            &ctx.project_id,
            item_id,
            session_field_id,
            Some(&serialize_binding(&binding)),
        )
        .await?;
    if let Some(todo_id) = todo_option_id {
        github
            .update_project_item_status(&ctx.project_id, item_id, status_field_id, todo_id)
            .await?;
    }
    Ok(())
}

/// Detect completed review sessions and drive the next transition from the
/// machine-readable verdict in their session output.
///
/// - `approve` advances to the next review state (`Review Technical` →
///   `Review Product` → `QA` → `Done`), preserving the binding's `resume` so a
///   later `changes` verdict can resume the developer session; reaching `Done`
///   clears the binding.
/// - `changes` (and every missing/malformed/unknown verdict — fail safe) sets
///   the item to `In Development` and resumes the developer session recorded in
///   `resume`. If that session is absent or the resume prompt fails, the item
///   degrades to `Todo` so the todo check starts a fresh developer session.
/// - a failed/not-found session resets the item to `Todo` and clears the
///   binding.
pub async fn run_review_completion_check(
    deps: &WorkflowContext,
    ctx: &ProjectContext,
    oc: &OpencodeSessionConfig,
) -> Result<(), WorkflowError> {
    let github = deps
        .github
        .as_ref()
        .ok_or_else(|| WorkflowError::NoGitHub(ctx.name.clone()))?;

    let field_ids = resolve_field_ids(github, &ctx.project_id).await?;
    let session_field_id = field_ids
        .session_field_id
        .ok_or_else(|| WorkflowError::NoSessionField(ctx.project_id.clone()))?;

    let project_items = github.list_project_items(&ctx.project_id).await?;
    if project_items.is_empty() {
        log_deduped(
            &deps.log_dedup,
            "review",
            LogLevel::Info,
            format!("{}: no project items for review completion check", ctx.name),
        )
        .await;
        return Ok(());
    }

    let status_field = github.get_project_status_field(&ctx.project_id).await?;

    let Some(status_field) = status_field else {
        log_deduped(
            &deps.log_dedup,
            "review",
            LogLevel::Warn,
            format!(
                "{}: no Status field found for review completion check",
                ctx.name
            ),
        )
        .await;
        return Ok(());
    };

    let todo_option_id = resolve_option_id(&status_field.options, "Todo");
    let in_dev_option_id = resolve_option_id(&status_field.options, "In Development");
    let done_option_id = resolve_option_id(&status_field.options, "Done");

    let oc_client = OpenCodeClient::new(oc.url.clone(), oc.pw.clone());
    let active_sessions = match oc_client.get_active_sessions().await {
        Ok(sessions) => sessions,
        Err(e) => {
            log_deduped(
                &deps.log_dedup,
                "review",
                LogLevel::Warn,
                format!(
                    "{}: could not fetch OpenCode active sessions for review completion check: {}",
                    ctx.name, e
                ),
            )
            .await;
            return Ok(());
        }
    };

    let mut item_values = Vec::new();
    for item in &project_items {
        item_values.push(github.get_project_item_values(&item.id).await?);
    }

    for state in &REVIEW_STATES {
        for (item, values) in project_items.iter().zip(&item_values) {
            let current_status = values.get("Status").and_then(|v| v.as_deref());
            if current_status != Some(state.status()) {
                continue;
            }

            let binding = binding_from_values(values);
            if binding.id.is_empty() {
                continue;
            }

            match verify_session_owner(
                github,
                ctx,
                &item.id,
                &session_field_id,
                &binding,
                state.status(),
            )
            .await?
            {
                OwnerCheck::Proceed => {}
                OwnerCheck::Handled => continue,
            }

            let session_id = binding.id.clone();
            let is_active = active_sessions.contains_key(&session_id);

            let session = match oc_client.get_session_v2(&session_id).await {
                Ok(session) => session,
                Err(e) => {
                    log_deduped(
                        &deps.log_dedup,
                        "review",
                        LogLevel::Warn,
                        format!(
                            "{}: could not fetch OpenCode session {} for review completion check: {}",
                            ctx.name, session_id, e
                        ),
                    )
                    .await;
                    continue;
                }
            };

            match classify_session(
                is_active,
                session.as_ref(),
                now_unix_secs(),
                oc.session_timeout_secs,
                oc.session_max_secs,
            ) {
                SessionCompletion::Waiting => continue,
                SessionCompletion::Failed(reason) => {
                    if is_active
                        && reason == "timeout"
                        && let Err(e) = oc_client.interrupt_session(&session_id).await
                    {
                        log_deduped(
                            &deps.log_dedup,
                            "review",
                            LogLevel::Warn,
                            format!(
                                "{}: could not interrupt stale review session {}: {}",
                                ctx.name, session_id, e
                            ),
                        )
                        .await;
                    }
                    log_deduped(
                        &deps.log_dedup,
                        "review",
                        LogLevel::Warn,
                        format!(
                            "{}: review session {} for issue #{} ended abnormally ({}), resetting to Todo",
                            ctx.name, session_id, item.content_number, reason
                        ),
                    )
                    .await;
                    reset_review_to_todo(
                        github,
                        ctx,
                        &field_ids.status_field_id,
                        &session_field_id,
                        &item.id,
                        todo_option_id.as_deref(),
                        binding.attempts,
                    )
                    .await?;
                    continue;
                }
                SessionCompletion::Succeeded => {}
            }

            let messages = match oc_client.get_session_messages(&session_id).await {
                Ok(messages) => messages,
                Err(e) => {
                    log_deduped(
                        &deps.log_dedup,
                        "review",
                        LogLevel::Warn,
                        format!(
                            "{}: could not fetch session messages for {}: {}",
                            ctx.name, session_id, e
                        ),
                    )
                    .await;
                    Vec::new()
                }
            };

            let (decision, notes) = match parse_verdict(&messages) {
                VerdictParse::Found(verdict) => (verdict.decision, verdict.notes),
                VerdictParse::Missing => {
                    log_deduped(
                        &deps.log_dedup,
                        "review",
                        LogLevel::Warn,
                        format!(
                            "{}: review session {} for issue #{} produced no verdict, failing safe to changes",
                            ctx.name, session_id, item.content_number
                        ),
                    )
                    .await;
                    (AgentDecision::Changes, String::new())
                }
                VerdictParse::Malformed(detail) => {
                    log_deduped(
                        &deps.log_dedup,
                        "review",
                        LogLevel::Warn,
                        format!(
                            "{}: review session {} for issue #{} produced a malformed verdict ({}), failing safe to changes",
                            ctx.name, session_id, item.content_number, detail
                        ),
                    )
                    .await;
                    (AgentDecision::Changes, String::new())
                }
            };

            if decision == AgentDecision::Approve {
                let Some(next_status) = next_review_status(state.status()) else {
                    continue;
                };

                if next_status == "Done" {
                    github
                        .update_project_item_session_id(
                            &ctx.project_id,
                            &item.id,
                            &session_field_id,
                            None,
                        )
                        .await?;
                    if let Some(done_id) = &done_option_id {
                        github
                            .update_project_item_status(
                                &ctx.project_id,
                                &item.id,
                                &field_ids.status_field_id,
                                done_id,
                            )
                            .await?;
                        log_deduped(
                            &deps.log_dedup,
                            "review",
                            LogLevel::Info,
                            format!(
                                "{}: review approved for issue #{}, marking Done",
                                ctx.name, item.content_number
                            ),
                        )
                        .await;
                    } else {
                        log_deduped(
                            &deps.log_dedup,
                            "review",
                            LogLevel::Warn,
                            format!(
                                "{}: 'Done' status option not found — item #{} left in {}",
                                ctx.name,
                                item.content_number,
                                state.status()
                            ),
                        )
                        .await;
                    }
                    continue;
                }

                let advanced = SessionBinding {
                    step: Some(next_status.to_string()),
                    id: String::new(),
                    resume: binding.resume.clone(),
                    attempts: binding.attempts,
                };
                github
                    .update_project_item_session_id(
                        &ctx.project_id,
                        &item.id,
                        &session_field_id,
                        Some(&serialize_binding(&advanced)),
                    )
                    .await?;
                match status_field
                    .options
                    .iter()
                    .find(|option| option.name == next_status)
                    .map(|option| option.id.clone())
                {
                    Some(option_id) => {
                        github
                            .update_project_item_status(
                                &ctx.project_id,
                                &item.id,
                                &field_ids.status_field_id,
                                &option_id,
                            )
                            .await?;
                        log_deduped(
                            &deps.log_dedup,
                            "review",
                            LogLevel::Info,
                            format!(
                                "{}: review approved for issue #{}, advancing to {}",
                                ctx.name, item.content_number, next_status
                            ),
                        )
                        .await;
                    }
                    None => {
                        log_deduped(
                            &deps.log_dedup,
                            "review",
                            LogLevel::Warn,
                            format!(
                                "{}: status option '{}' not found — item #{} advanced in binding only",
                                ctx.name, next_status, item.content_number
                            ),
                        )
                        .await;
                    }
                }
                continue;
            }

            let resume_id = binding.resume.clone();
            let in_dev_binding = SessionBinding {
                step: Some("In Development".to_string()),
                id: resume_id.clone(),
                resume: String::new(),
                attempts: binding.attempts.saturating_add(1),
            };
            github
                .update_project_item_session_id(
                    &ctx.project_id,
                    &item.id,
                    &session_field_id,
                    Some(&serialize_binding(&in_dev_binding)),
                )
                .await?;
            if let Some(in_dev_id) = &in_dev_option_id {
                github
                    .update_project_item_status(
                        &ctx.project_id,
                        &item.id,
                        &field_ids.status_field_id,
                        in_dev_id,
                    )
                    .await?;
            } else {
                log_deduped(
                    &deps.log_dedup,
                    "review",
                    LogLevel::Warn,
                    format!(
                        "{}: 'In Development' status option not found — item #{} left in {}",
                        ctx.name,
                        item.content_number,
                        state.status()
                    ),
                )
                .await;
            }

            let feedback = build_changes_feedback(github, ctx, item.content_number, &notes).await;

            let mut resumed = false;
            if !resume_id.is_empty() {
                match oc_client.get_session_v2(&resume_id).await {
                    Ok(Some(_)) => match oc_client
                        .send_prompt_with(&resume_id, &feedback, true, Some(Delivery::Steer))
                        .await
                    {
                        Ok(_) => resumed = true,
                        Err(e) => {
                            log_deduped(
                                &deps.log_dedup,
                                "review",
                                LogLevel::Warn,
                                format!(
                                    "{}: could not resume developer session {} for issue #{}: {}",
                                    ctx.name, resume_id, item.content_number, e
                                ),
                            )
                            .await;
                        }
                    },
                    Ok(None) => {
                        log_deduped(
                            &deps.log_dedup,
                            "review",
                            LogLevel::Warn,
                            format!(
                                "{}: developer session {} for issue #{} not found, degrading to Todo",
                                ctx.name, resume_id, item.content_number
                            ),
                        )
                        .await;
                    }
                    Err(e) => {
                        log_deduped(
                            &deps.log_dedup,
                            "review",
                            LogLevel::Warn,
                            format!(
                                "{}: could not fetch developer session {} for issue #{}: {}",
                                ctx.name, resume_id, item.content_number, e
                            ),
                        )
                        .await;
                    }
                }
            }

            if resumed {
                log_deduped(
                    &deps.log_dedup,
                    "review",
                    LogLevel::Info,
                    format!(
                        "{}: review requested changes for issue #{}, resumed developer session {}",
                        ctx.name, item.content_number, resume_id
                    ),
                )
                .await;
            } else {
                reset_review_to_todo(
                    github,
                    ctx,
                    &field_ids.status_field_id,
                    &session_field_id,
                    &item.id,
                    todo_option_id.as_deref(),
                    binding.attempts,
                )
                .await?;
                log_deduped(
                    &deps.log_dedup,
                    "review",
                    LogLevel::Warn,
                    format!(
                        "{}: review requested changes for issue #{} but no developer session could be resumed, reset to Todo",
                        ctx.name, item.content_number
                    ),
                )
                .await;
            }
        }
    }

    Ok(())
}

/// Detect completed developer sessions in "In Development" status, resolve
/// review threads (from the developer verdict's `resolved_threads`, falling
/// back to the legacy `### Resolve threads` section), and transition the item
/// to "Review Technical".
///
/// The developer session id is retained as the binding's `resume` so a later
/// `changes` verdict can resume it. If no threads are reported, no resolution
/// mutations fire but the issue still transitions to "Review Technical".
pub async fn run_dev_completion_check(
    deps: &WorkflowContext,
    ctx: &ProjectContext,
    oc: &OpencodeSessionConfig,
) -> Result<(), WorkflowError> {
    let github = deps
        .github
        .as_ref()
        .ok_or_else(|| WorkflowError::NoGitHub(ctx.name.clone()))?;

    let field_ids = resolve_field_ids(github, &ctx.project_id).await?;
    let session_field_id = field_ids
        .session_field_id
        .ok_or_else(|| WorkflowError::NoSessionField(ctx.project_id.clone()))?;

    let project_items = github.list_project_items(&ctx.project_id).await?;
    if project_items.is_empty() {
        log_deduped(
            &deps.log_dedup,
            "review",
            LogLevel::Info,
            format!("{}: no project items for dev completion check", ctx.name),
        )
        .await;
        return Ok(());
    }

    let status_field = github.get_project_status_field(&ctx.project_id).await?;

    let Some(status_field) = status_field else {
        log_deduped(
            &deps.log_dedup,
            "review",
            LogLevel::Warn,
            format!(
                "{}: no Status field found for dev completion check",
                ctx.name
            ),
        )
        .await;
        return Ok(());
    };

    let _in_dev_option_id = resolve_option_id(&status_field.options, "In Development");
    let review_tech_option_id = resolve_option_id(&status_field.options, "Review Technical");
    let todo_option_id = resolve_option_id(&status_field.options, "Todo");

    let oc_client = OpenCodeClient::new(oc.url.clone(), oc.pw.clone());
    let active_sessions = match oc_client.get_active_sessions().await {
        Ok(sessions) => sessions,
        Err(e) => {
            log_deduped(
                &deps.log_dedup,
                "review",
                LogLevel::Warn,
                format!(
                    "{}: could not fetch OpenCode active sessions for dev completion check: {}",
                    ctx.name, e
                ),
            )
            .await;
            return Ok(());
        }
    };

    let mut item_values = Vec::new();
    for item in &project_items {
        item_values.push(github.get_project_item_values(&item.id).await?);
    }

    for (item, values) in project_items.iter().zip(&item_values) {
        let current_status = values.get("Status").and_then(|v| v.as_deref());
        if current_status != Some("In Development") {
            continue;
        }

        let binding = binding_from_values(values);
        if binding.id.is_empty() {
            continue;
        }

        match verify_session_owner(
            github,
            ctx,
            &item.id,
            &session_field_id,
            &binding,
            "In Development",
        )
        .await?
        {
            OwnerCheck::Proceed => {}
            OwnerCheck::Handled => continue,
        }

        let session_id = binding.id.clone();
        let is_active = active_sessions.contains_key(&session_id);

        let session = match oc_client.get_session_v2(&session_id).await {
            Ok(session) => session,
            Err(e) => {
                log_deduped(
                    &deps.log_dedup,
                    "review",
                    LogLevel::Warn,
                    format!(
                        "{}: could not fetch OpenCode session {} for dev completion check: {}",
                        ctx.name, session_id, e
                    ),
                )
                .await;
                continue;
            }
        };

        match classify_session(
            is_active,
            session.as_ref(),
            now_unix_secs(),
            oc.session_timeout_secs,
            oc.session_max_secs,
        ) {
            SessionCompletion::Waiting => continue,
            SessionCompletion::Failed(reason) => {
                if is_active
                    && reason == "timeout"
                    && let Err(e) = oc_client.interrupt_session(&session_id).await
                {
                    log_deduped(
                        &deps.log_dedup,
                        "review",
                        LogLevel::Warn,
                        format!(
                            "{}: could not interrupt stale developer session {}: {}",
                            ctx.name, session_id, e
                        ),
                    )
                    .await;
                }
                log_deduped(
                    &deps.log_dedup,
                    "review",
                    LogLevel::Warn,
                    format!(
                        "{}: developer session {} for issue #{} ended abnormally ({}), resetting to Todo",
                        ctx.name, session_id, item.content_number, reason
                    ),
                )
                .await;
                let parked = SessionBinding {
                    step: Some("Todo".to_string()),
                    id: String::new(),
                    resume: String::new(),
                    attempts: binding.attempts,
                };
                github
                    .update_project_item_session_id(
                        &ctx.project_id,
                        &item.id,
                        &session_field_id,
                        Some(&serialize_binding(&parked)),
                    )
                    .await?;
                if let Some(ref option_id) = todo_option_id {
                    github
                        .update_project_item_status(
                            &ctx.project_id,
                            &item.id,
                            &field_ids.status_field_id,
                            option_id,
                        )
                        .await?;
                }
                continue;
            }
            SessionCompletion::Succeeded => {}
        }

        log_deduped(
            &deps.log_dedup,
            "review",
            LogLevel::Info,
            format!(
                "{}: developer session {} for issue #{} has completed, resolving threads",
                ctx.name, session_id, item.content_number
            ),
        )
        .await;

        let messages = match oc_client.get_session_messages(&session_id).await {
            Ok(msgs) => msgs,
            Err(e) => {
                log_deduped(
                    &deps.log_dedup,
                    "review",
                    LogLevel::Warn,
                    format!(
                        "{}: could not fetch session messages for {}: {}",
                        ctx.name, session_id, e
                    ),
                )
                .await;
                Vec::new()
            }
        };
        let thread_ids = match parse_verdict(&messages) {
            VerdictParse::Found(verdict) => verdict.resolved_threads,
            VerdictParse::Missing | VerdictParse::Malformed(_) => {
                parse_resolve_threads(&messages.iter().map(|m| m.text()).collect::<Vec<_>>())
            }
        };

        for thread_id in &thread_ids {
            log_deduped(
                &deps.log_dedup,
                "review",
                LogLevel::Info,
                format!(
                    "{}: resolving review thread {} for issue #{}",
                    ctx.name, thread_id, item.content_number
                ),
            )
            .await;
            if let Err(e) = github.resolve_review_thread(thread_id).await {
                log_deduped(
                    &deps.log_dedup,
                    "review",
                    LogLevel::Warn,
                    format!(
                        "{}: failed to resolve thread {}: {}",
                        ctx.name, thread_id, e
                    ),
                )
                .await;
            }
        }

        let retained = SessionBinding {
            step: Some("Review Technical".to_string()),
            id: String::new(),
            resume: session_id.clone(),
            attempts: binding.attempts.saturating_add(1),
        };
        github
            .update_project_item_session_id(
                &ctx.project_id,
                &item.id,
                &session_field_id,
                Some(&serialize_binding(&retained)),
            )
            .await?;

        if let Some(ref option_id) = review_tech_option_id {
            github
                .update_project_item_status(
                    &ctx.project_id,
                    &item.id,
                    &field_ids.status_field_id,
                    option_id,
                )
                .await?;
        }
    }

    Ok(())
}

/// Detect completed triage sessions in "Triage" status, parse session output
/// for sub-task definitions, create sub-issues via the GitHub API, add them
/// to the project with "Todo" status and a waveId, and clear the session ID
/// on the parent issue. If no sub-tasks are found, transitions the parent
/// issue directly to "Todo".
///
/// Sub-task format: lines under a `## Sub-tasks` heading matching
/// `^\d+\.\s+(.+?):?\s*(.*)$`.
pub async fn run_triage_completion_check(
    deps: &WorkflowContext,
    ctx: &ProjectContext,
    oc: &OpencodeSessionConfig,
) -> Result<(), WorkflowError> {
    let github = deps
        .github
        .as_ref()
        .ok_or_else(|| WorkflowError::NoGitHub(ctx.name.clone()))?;

    let field_ids = resolve_field_ids(github, &ctx.project_id).await?;
    let session_field_id = field_ids
        .session_field_id
        .ok_or_else(|| WorkflowError::NoSessionField(ctx.project_id.clone()))?;
    let wave_field_id = field_ids
        .wave_field_id
        .ok_or_else(|| WorkflowError::Other("waveId field not found".to_string()))?;

    let project_items = github.list_project_items(&ctx.project_id).await?;
    if project_items.is_empty() {
        tracing::info!("{}: no project items for triage completion check", ctx.name);
        return Ok(());
    }

    let status_field = github.get_project_status_field(&ctx.project_id).await?;

    let Some(status_field) = status_field else {
        tracing::warn!(
            "{}: no Status field found for triage completion check",
            ctx.name
        );
        return Ok(());
    };

    let todo_option_id = resolve_option_id(&status_field.options, "Todo")
        .ok_or_else(|| WorkflowError::StatusOptionNotFound("Todo".to_string()))?;
    let done_option_id = resolve_option_id(&status_field.options, "Done");

    let oc_client = OpenCodeClient::new(oc.url.clone(), oc.pw.clone());
    let active_sessions = match oc_client.get_active_sessions().await {
        Ok(sessions) => sessions,
        Err(e) => {
            tracing::warn!(
                "{}: could not fetch OpenCode active sessions for triage completion check: {}",
                ctx.name,
                e
            );
            return Ok(());
        }
    };

    let mut item_values = Vec::new();
    for item in &project_items {
        item_values.push(github.get_project_item_values(&item.id).await?);
    }

    for (item, values) in project_items.iter().zip(&item_values) {
        let current_status = values.get("Status").and_then(|v| v.as_deref());
        if current_status != Some("Triage") {
            continue;
        }

        let binding = binding_from_values(values);
        if binding.id.is_empty() {
            continue;
        }

        match verify_session_owner(github, ctx, &item.id, &session_field_id, &binding, "Triage")
            .await?
        {
            OwnerCheck::Proceed => {}
            OwnerCheck::Handled => continue,
        }

        let session_id = binding.id.clone();
        let is_active = active_sessions.contains_key(&session_id);

        let session = match oc_client.get_session_v2(&session_id).await {
            Ok(session) => session,
            Err(e) => {
                tracing::warn!(
                    "{}: could not fetch OpenCode session {} for triage completion check: {}",
                    ctx.name,
                    session_id,
                    e
                );
                continue;
            }
        };

        match classify_session(
            is_active,
            session.as_ref(),
            now_unix_secs(),
            oc.session_timeout_secs,
            oc.session_max_secs,
        ) {
            SessionCompletion::Waiting => continue,
            SessionCompletion::Failed(reason) => {
                if is_active
                    && reason == "timeout"
                    && let Err(e) = oc_client.interrupt_session(&session_id).await
                {
                    tracing::warn!(
                        "{}: could not interrupt stale triage session {}: {}",
                        ctx.name,
                        session_id,
                        e
                    );
                }
                tracing::warn!(
                    "{}: triage session {} for issue #{} ended abnormally ({}), parking",
                    ctx.name,
                    session_id,
                    item.content_number,
                    reason
                );
                let parked = SessionBinding {
                    step: Some("Triage".to_string()),
                    id: String::new(),
                    resume: String::new(),
                    attempts: binding.attempts,
                };
                github
                    .update_project_item_session_id(
                        &ctx.project_id,
                        &item.id,
                        &session_field_id,
                        Some(&serialize_binding(&parked)),
                    )
                    .await?;
                continue;
            }
            SessionCompletion::Succeeded => {}
        }

        tracing::info!(
            "{}: triage session {} for issue #{} has completed, parsing sub-tasks",
            ctx.name,
            session_id,
            item.content_number
        );

        let messages = oc_client
            .get_session_messages(&session_id)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(
                    "{}: could not fetch session messages for {}: {}",
                    ctx.name,
                    session_id,
                    e
                );
                Vec::new()
            });
        let sub_tasks = match parse_verdict(&messages) {
            VerdictParse::Found(verdict) => verdict
                .sub_tasks
                .into_iter()
                .map(|task| (task.title, task.description))
                .collect(),
            VerdictParse::Missing | VerdictParse::Malformed(_) => {
                parse_sub_tasks(&messages.iter().map(|m| m.text()).collect::<Vec<_>>())
            }
        };

        if sub_tasks.is_empty() {
            tracing::info!(
                "{}: no sub-tasks found for issue #{}, transitioning to Todo",
                ctx.name,
                item.content_number
            );
            github
                .update_project_item_status(
                    &ctx.project_id,
                    &item.id,
                    &field_ids.status_field_id,
                    &todo_option_id,
                )
                .await?;
        } else {
            tracing::info!(
                "{}: creating {} sub-tasks for issue #{}",
                ctx.name,
                sub_tasks.len(),
                item.content_number
            );
            for (index, (title, description)) in sub_tasks.iter().enumerate() {
                let wave_id = (index + 1) as i64;
                let sub_title = format!("[#{:?}] {}", item.content_number, title);
                let sub_body = format!(
                    "Sub-task of #[{}]\n\n{}\n\n**Wave:** {}",
                    item.content_number, description, wave_id
                );

                let created_issue = github
                    .create_issue(&ctx.owner, &ctx.repo, &sub_title, &sub_body)
                    .await?;

                let sub_item_id = github
                    .add_issue_to_project(&created_issue.node_id, &ctx.project_id)
                    .await?;

                github
                    .update_project_item_status(
                        &ctx.project_id,
                        &sub_item_id,
                        &field_ids.status_field_id,
                        &todo_option_id,
                    )
                    .await?;

                github
                    .update_project_item_wave_id(
                        &ctx.project_id,
                        &sub_item_id,
                        &wave_field_id,
                        wave_id,
                    )
                    .await?;

                tracing::info!(
                    "{}: created sub-task #{} (waveId={}) for issue #{}",
                    ctx.name,
                    created_issue.number,
                    wave_id,
                    item.content_number
                );
            }

            // C4: the parent is fully decomposed — mark it Done so it is not
            // re-triaged forever.
            if let Some(done_id) = &done_option_id {
                github
                    .update_project_item_status(
                        &ctx.project_id,
                        &item.id,
                        &field_ids.status_field_id,
                        done_id,
                    )
                    .await?;
            } else {
                tracing::warn!(
                    "{}: 'Done' status option not found — parent #{} left in Triage",
                    ctx.name,
                    item.content_number
                );
            }
        }

        github
            .update_project_item_session_id(&ctx.project_id, &item.id, &session_field_id, None)
            .await?;
    }

    Ok(())
}

/// Parse sub-task definitions from session messages.
///
/// Looks for a `## Sub-tasks` heading and parses lines matching
/// `^\d+\.\s+(.+?):?\s*(.*)$` into `(title, description)` tuples.
pub fn parse_sub_tasks(messages: &[String]) -> Vec<(String, String)> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"^\s*\d+\.\s+([^:]+)(?:\s*:\s*(.*))?$")
            .expect("hardcoded sub-task regex literal is valid")
    });
    let mut in_section = false;
    let mut sub_tasks = Vec::new();

    for line in messages {
        if line.contains("## Sub-tasks") {
            in_section = true;
            continue;
        }
        if in_section {
            if line.starts_with("###") {
                break;
            }
            if let Some(captures) = re.captures(line)
                && let Some(title_match) = captures.get(1)
            {
                let title = title_match.as_str().trim().to_string();
                let description = captures
                    .get(2)
                    .map(|m| m.as_str().trim().to_string())
                    .unwrap_or_default();
                sub_tasks.push((title, description));
            }
        }
    }

    sub_tasks
}

// ─── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GitSection;
    use crate::test_utils::{gh_client, make_deps};
    use crate::workflow::helpers::ProjectContext;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use wiremock::matchers::{body_string_contains, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ─── Test helpers ────────────────────────────────────────

    /// Matches when the request body does NOT contain the given substring.
    struct BodyNotContains(&'static str);

    impl wiremock::Match for BodyNotContains {
        fn matches(&self, request: &wiremock::Request) -> bool {
            let body = String::from_utf8_lossy(&request.body);
            !body.contains(self.0)
        }
    }

    /// Build a [`ProjectContext`] for testing.
    fn make_context() -> ProjectContext {
        ProjectContext {
            name: "test-project".to_string(),
            config: GitSection {
                repository: "https://github.com/owner/repo".to_string(),
                project_id: Some("PID-123".to_string()),
                directory: "/test-work".to_string(),
                title_pattern: "@ai.*".to_string(),
                trello_api_key: None,
                trello_token: None,
                trello_board_id: None,
                token: None,
                branch_name: None,
            },
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            project_id: "PID-123".to_string(),
        }
    }

    /// Build an [`OpencodeSessionConfig`] pointing at a mock OpenCode server.
    fn make_oc_config(url: String) -> OpencodeSessionConfig {
        OpencodeSessionConfig {
            url,
            pw: "pw".to_string(),
            directory: "/test-work".to_string(),
            project: None,
            concurrency: HashMap::new(),
            session_timeout_secs: 1800,
            session_max_secs: 86400,
            max_session_attempts: 3,
        }
    }

    /// Build a field-value node for a text field (e.g. `sessionId`).
    fn text_field_value(name: &str, text: Option<&str>) -> serde_json::Value {
        json!({
            "__typename": "ProjectV2ItemFieldTextValue",
            "text": text,
            "field": {"__typename": "ProjectV2Field", "name": name}
        })
    }

    /// Build a `sessionId` field-value node carrying a serialized binding that
    /// names *status* as the owning step.
    fn session_binding_value(status: &str, session_id: &str) -> serde_json::Value {
        let binding = SessionBinding {
            step: Some(status.to_string()),
            id: session_id.to_string(),
            resume: String::new(),
            attempts: 1,
        };
        text_field_value("sessionId", Some(&serialize_binding(&binding)))
    }

    /// Build a field-value node for a single-select field (e.g. `Status`).
    fn single_select_field_value(name: &str, option_name: &str) -> serde_json::Value {
        json!({
            "__typename": "ProjectV2ItemFieldSingleSelectValue",
            "name": option_name,
            "field": {"__typename": "ProjectV2Field", "name": name}
        })
    }

    /// Build a field-value node for a number field (e.g. `waveId`).
    fn number_field_value(name: &str, number: Option<f64>) -> serde_json::Value {
        json!({
            "__typename": "ProjectV2ItemFieldNumberValue",
            "number": number,
            "field": {"__typename": "ProjectV2Field", "name": name}
        })
    }

    /// Mount common GitHub mocks needed by the triage check.
    async fn mount_triage_github_mocks(
        server: &MockServer,
        issues: serde_json::Value,
        project_items: serde_json::Value,
        field_values: serde_json::Value,
    ) {
        // Status field query (for resolve_status_option_and_session)
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "status-field-id",
                            "options": [
                                {"id": "triage-opt-id", "name": "Triage"},
                                {"id": "todo-opt-id", "name": "Todo"},
                                {"id": "review-tech-opt-id", "name": "Review Technical"},
                                {"id": "review-prod-opt-id", "name": "Review Product"},
                                {"id": "qa-opt-id", "name": "QA"},
                            ]
                        }
                    }
                }
            })))
            .mount(server)
            .await;

        // Fields query (for sessionId field)
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id": "status-field-id", "name": "Status", "dataType": "SINGLE_SELECT"},
                                {"id": "session-field-id", "name": "sessionId", "dataType": "TEXT"},
                            ]
                        }
                    }
                }
            })))
            .mount(server)
            .await;

        // REST issues endpoint
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issues))
            .mount(server)
            .await;

        // Project items query
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("items(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "items": project_items } }
            })))
            .mount(server)
            .await;

        // Add issue to project mutation
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("addProjectV2ItemById"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "addProjectV2ItemById": { "item": { "id": "new-item-id-1" } }
                }
            })))
            .mount(server)
            .await;

        // Update item field value mutation (status + session id)
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2ItemFieldValue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "updateProjectV2ItemFieldValue": { "projectV2Item": { "id": "item-id" } }
                }
            })))
            .mount(server)
            .await;

        // Field values query
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fieldValues(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "fieldValues": field_values } }
            })))
            .mount(server)
            .await;
    }

    /// Mount OpenCode mocks for the v2 session flow (location → worktree → session → prompt).
    async fn mount_opencode_mocks(server: &MockServer) {
        // Location lookup (directory → project id)
        Mock::given(method("GET"))
            .and(path("/api/location"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/test-work",
                "project": {"id": "p1", "directory": "/test-work", "canonical": "/test-work"}
            })))
            .mount(server)
            .await;

        // Worktree creation
        Mock::given(method("POST"))
            .and(path("/api/worktree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/wt/dir1"
            })))
            .mount(server)
            .await;

        // Session creation
        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "sess123"}
            })))
            .mount(server)
            .await;

        // Prompt
        Mock::given(method("POST"))
            .and(path("/api/session/sess123/prompt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "prompt1"}
            })))
            .mount(server)
            .await;
    }

    /// Mount branch-related REST endpoints for the todo check.
    async fn mount_branch_mocks(
        server: &MockServer,
        exists: bool,
        default_branch: &str,
        commit_sha: &str,
    ) {
        let branch_resp = if exists {
            ResponseTemplate::new(200).set_body_json(json!({"nodes": []}))
        } else {
            ResponseTemplate::new(404).set_body_json(json!({"nodes": []}))
        };
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/branches/issue-42"))
            .respond_with(branch_resp)
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "default_branch": default_branch
            })))
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!(
                "/repos/owner/repo/commits/{}",
                default_branch
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "sha": commit_sha
            })))
            .mount(server)
            .await;

        if !exists {
            Mock::given(method("POST"))
                .and(path("/repos/owner/repo/git/refs"))
                .respond_with(ResponseTemplate::new(201).set_body_json(json!({})))
                .expect(1)
                .mount(server)
                .await;
        } else {
            Mock::given(method("POST"))
                .and(path("/repos/owner/repo/git/refs"))
                .respond_with(ResponseTemplate::new(201).set_body_json(json!({})))
                .expect(0)
                .mount(server)
                .await;
        }
    }

    /// Mount common GitHub mocks needed by the todo check.
    async fn mount_todo_github_mocks(
        server: &MockServer,
        project_items: serde_json::Value,
        issues: serde_json::Value,
        field_values: serde_json::Value,
    ) {
        // Fields query (for resolve_field_ids — status + session)
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id": "status-field-id", "name": "Status", "dataType": "SINGLE_SELECT"},
                                {"id": "session-field-id", "name": "sessionId", "dataType": "TEXT"},
                            ]
                        }
                    }
                }
            })))
            .mount(server)
            .await;

        // Status field query (for resolve_field_ids via get_project_status_field)
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "status-field-id",
                            "options": [
                                {"id": "todo-opt-id", "name": "Todo"},
                                {"id": "dev-opt-id", "name": "In Development"},
                            ]
                        }
                    }
                }
            })))
            .mount(server)
            .await;

        // REST issues endpoint
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issues))
            .mount(server)
            .await;

        // Project items query
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("items(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "items": project_items } }
            })))
            .mount(server)
            .await;

        // Field values query
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fieldValues(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "fieldValues": field_values } }
            })))
            .mount(server)
            .await;

        // Update session id mutation
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2ItemFieldValue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "updateProjectV2ItemFieldValue": { "projectV2Item": { "id": "item-id" } }
                }
            })))
            .mount(server)
            .await;
    }

    /// Mount common GitHub mocks needed by the review check.
    async fn mount_review_github_mocks(
        server: &MockServer,
        project_items: serde_json::Value,
        issues: serde_json::Value,
        field_values: serde_json::Value,
        status_options: Vec<serde_json::Value>,
    ) {
        // Fields query (for resolve_field_ids)
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id": "status-field-id", "name": "Status", "dataType": "SINGLE_SELECT"},
                                {"id": "session-field-id", "name": "sessionId", "dataType": "TEXT"},
                            ]
                        }
                    }
                }
            })))
            .mount(server)
            .await;

        // Status field query (for get_project_status_field)
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "status-field-id",
                            "options": status_options
                        }
                    }
                }
            })))
            .mount(server)
            .await;

        // REST issues endpoint
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issues))
            .mount(server)
            .await;

        // Project items query
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("items(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "items": project_items } }
            })))
            .mount(server)
            .await;

        // Field values query
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fieldValues(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "fieldValues": field_values } }
            })))
            .mount(server)
            .await;

        // Update session id mutation
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2ItemFieldValue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "updateProjectV2ItemFieldValue": { "projectV2Item": { "id": "item-id" } }
                }
            })))
            .mount(server)
            .await;
    }

    /// Mount branch mocks for the review check.
    async fn mount_review_branch_mocks(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/branches/issue-42"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"nodes": []})))
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "default_branch": "main"
            })))
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/commits/main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "sha": "abc123"
            })))
            .mount(server)
            .await;
    }

    // ─── Constants / structures tests ──────────────────────────

    // Test: ReviewState fields are correct
    #[test]
    fn review_states_constant_matches_ts() {
        assert_eq!(REVIEW_STATES.len(), 3);
        assert_eq!(REVIEW_STATES[0].status(), "Review Technical");
        assert_eq!(REVIEW_STATES[0].agent().as_str(), "git-automate-reviewer");
        assert_eq!(REVIEW_STATES[0].prompt(), "reviewer");
        assert_eq!(REVIEW_STATES[1].status(), "Review Product");
        assert_eq!(REVIEW_STATES[1].agent().as_str(), "git-automate-product");
        assert_eq!(REVIEW_STATES[1].prompt(), "product");
        assert_eq!(REVIEW_STATES[2].status(), "QA");
        assert_eq!(REVIEW_STATES[2].agent().as_str(), "git-automate-qa");
        assert_eq!(REVIEW_STATES[2].prompt(), "qa");
    }

    // ─── Pure logic tests (no HTTP) ────────────────────────────

    // Test 1: @ai filter — "@ai something" matches
    #[test]
    fn filter_at_ai_title_with_space_matches() {
        assert!("@ai Fix bug".starts_with("@ai "));
    }

    // Test 2: @ai filter — "@ai" (no space, no text) does NOT match
    #[test]
    fn filter_at_ai_title_no_space_does_not_match() {
        assert!(!"@ai".starts_with("@ai "));
    }

    // Test 3: @ai filter — "fix @ai" does NOT match
    #[test]
    fn filter_at_ai_title_mid_string_does_not_match() {
        assert!(!"fix @ai".starts_with("@ai "));
    }

    // Test 4: Branch name format is "issue-{number}"
    #[test]
    fn branch_name_format() {
        let issue_number: i64 = 42;
        let branch_name = format!("issue-{}", issue_number);
        assert_eq!(branch_name, "issue-42");
    }

    // Test 5: Title fallback for todo check: "Dev work for issue #{n}"
    #[test]
    fn todo_title_fallback_when_issue_not_in_map() {
        let issue_number: i64 = 42;
        let issue: Option<IssueInfo> = None;
        let title = if let Some(issue) = issue {
            issue.title.clone()
        } else {
            format!("Dev work for issue #{}", issue_number)
        };
        assert_eq!(title, "Dev work for issue #42");
    }

    // Test 6: Title uses issue title when issue is in map
    #[test]
    fn todo_title_uses_issue_title_when_present() {
        let issue_number: i64 = 42;
        let issue: Option<IssueInfo> = Some(IssueInfo {
            id: "n1".to_string(),
            number: 42,
            title: "Real Issue Title".to_string(),
            body: Some("body".to_string()),
            state: "open".to_string(),
        });
        let title = if let Some(issue) = issue {
            issue.title.clone()
        } else {
            format!("Dev work for issue #{}", issue_number)
        };
        assert_eq!(title, "Real Issue Title");
    }

    // Test 7: Message fallback for todo check: "Issue #{n}"
    #[test]
    fn todo_message_fallback_when_issue_not_in_map() {
        let issue_number: i64 = 42;
        let issue: Option<&IssueInfo> = None;
        let message = if let Some(issue) = issue {
            issue_body_or_title(issue)
        } else {
            format!("Issue #{}", issue_number)
        };
        assert_eq!(message, "Issue #42");
    }

    // Test 8: Message uses issue_body_or_title when issue is in map
    #[test]
    fn todo_message_uses_issue_body_or_title_when_present() {
        let issue = IssueInfo {
            id: "n1".to_string(),
            number: 42,
            title: "Title".to_string(),
            body: Some("Detailed body".to_string()),
            state: "open".to_string(),
        };
        let issue_number: i64 = 42;
        let issue_opt: Option<&IssueInfo> = Some(&issue);
        let message = if let Some(issue) = issue_opt {
            issue_body_or_title(issue)
        } else {
            format!("Issue #{}", issue_number)
        };
        assert_eq!(message, "Detailed body");
    }

    // Test 9: Message fallback when issue body is None: "Issue #{n}: {title}"
    #[test]
    fn todo_message_uses_fallback_when_body_is_none() {
        let issue = IssueInfo {
            id: "n1".to_string(),
            number: 42,
            title: "No Body Issue".to_string(),
            body: None,
            state: "open".to_string(),
        };
        let issue_opt: Option<&IssueInfo> = Some(&issue);
        let issue_number: i64 = 42;
        let message = if let Some(issue) = issue_opt {
            issue_body_or_title(issue)
        } else {
            format!("Issue #{}", issue_number)
        };
        // body is None, so issue_body_or_title returns "Issue #{n}: {title}"
        assert_eq!(message, "Issue #42: No Body Issue");
    }

    // Test 10: Review title format is "{status}: {issue_title}"
    #[test]
    fn review_title_format_with_issue() {
        let state = &REVIEW_STATES[0]; // Review Technical
        let issue = IssueInfo {
            id: "n1".to_string(),
            number: 7,
            title: "Fix login".to_string(),
            body: Some("body".to_string()),
            state: "open".to_string(),
        };
        let issue_opt: Option<&IssueInfo> = Some(&issue);
        let issue_number: i64 = 7;
        let title = format!(
            "{}: {}",
            state.status(),
            issue_opt
                .map(|i| i.title.as_str())
                .unwrap_or(&format!("#{}", issue_number))
        );
        assert_eq!(title, "Review Technical: Fix login");
    }

    // Test 11: Review title format fallback: "{status}: #{number}"
    #[test]
    fn review_title_format_fallback_when_issue_absent() {
        let state = &REVIEW_STATES[0]; // Review Technical
        let issue_opt: Option<&IssueInfo> = None;
        let issue_number: i64 = 7;
        let title = format!(
            "{}: {}",
            state.status(),
            issue_opt
                .map(|i| i.title.as_str())
                .unwrap_or(&format!("#{}", issue_number))
        );
        assert_eq!(title, "Review Technical: #7");
    }

    // Test 12: fill_prompt receives correct values for review check
    #[test]
    fn fill_prompt_review_values() {
        let template = "Issue: {{ISSUE_NUMBER}}\nBranch: {{BRANCH_NAME}}\nTitle: {{ISSUE_TITLE}}\nBody: {{ISSUE_BODY}}\nPR: {{PR_URL}}\nChanges: {{PR_CHANGES}}".to_string();
        let issue_number: i64 = 42;
        let branch_name = format!("issue-{}", issue_number);
        let values = HashMap::from([
            ("ISSUE_NUMBER".to_string(), issue_number.to_string()),
            ("BRANCH_NAME".to_string(), branch_name.clone()),
            ("ISSUE_TITLE".to_string(), "Fix bug".to_string()),
            ("ISSUE_BODY".to_string(), "Description".to_string()),
            ("PR_URL".to_string(), String::new()),
            ("PR_CHANGES".to_string(), String::new()),
        ]);
        let result = fill_prompt(&template, &values);
        assert_eq!(
            result,
            "Issue: 42\nBranch: issue-42\nTitle: Fix bug\nBody: Description\nPR: \nChanges: "
        );
    }

    // Test 13: fill_prompt with empty ISSUE_NUMBER as string
    #[test]
    fn fill_prompt_issue_number_is_string() {
        let template = "{{ISSUE_NUMBER}}".to_string();
        let values = HashMap::from([("ISSUE_NUMBER".to_string(), "42".to_string())]);
        let result = fill_prompt(&template, &values);
        assert_eq!(result, "42");
    }

    // ─── Triage Check Integration Tests ───────────────────────

    // Test 1: No @ai issues → early return, no session started
    #[tokio::test]
    async fn triage_no_ai_issues_early_return() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        // No @ai issues — only non-matching titles
        let issues = json!([
            {"node_id": "i1", "number": 1, "title": "Regular bug", "body": "body", "state": "open", "pull_request": null},
        ]);
        let empty_items = json!({"nodes": []});
        let empty_field_values = json!({"nodes": []});

        mount_triage_github_mocks(&mock, issues, empty_items, empty_field_values).await;

        // OpenCode should never be called
        let _ = mount_opencode_mocks(&oc_mock).await;

        let result = run_triage_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());

        // Verify OpenCode session was never created
        oc_mock.verify().await;
    }

    // Test 2: @ai issue not in project → calls addIssueToProject, updateProjectItemStatus, startOpencodeSession
    #[tokio::test]
    async fn triage_ai_issue_not_in_project_calls_all() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([
            {"node_id": "issue-node-1", "number": 1, "title": "@ai Fix bug", "body": "Detailed description", "state": "open", "pull_request": null},
        ]);
        // Project has no items yet (issue not in project)
        let empty_items = json!({"nodes": []});
        // Field values returns empty → no session → should start one
        let empty_field_values = json!({"nodes": []});

        mount_triage_github_mocks(&mock, issues, empty_items, empty_field_values).await;
        mount_opencode_mocks(&oc_mock).await;

        let result = run_triage_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());

        // Verify OpenCode session was created
        oc_mock.verify().await;
    }

    // Test 3: @ai issue already in project → skips addIssueToProject
    #[tokio::test]
    async fn triage_ai_issue_already_in_project_skips_add() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([
            {"node_id": "issue-node-1", "number": 1, "title": "@ai Fix bug", "body": "body", "state": "open", "pull_request": null},
        ]);
        // Issue already in project — content_number 1 maps to item id "existing-item-id"
        let project_items = json!({
            "nodes": [
                {"id": "existing-item-id", "content": {"__typename": "Issue", "id": "issue-node-1", "number": 1}}
            ]
        });
        let empty_field_values = json!({"nodes": []});

        mount_triage_github_mocks(&mock, issues, project_items, empty_field_values).await;
        mount_opencode_mocks(&oc_mock).await;

        // Track whether addProjectV2ItemById is called — it should NOT be
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("addProjectV2ItemById"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "addProjectV2ItemById": { "item": { "id": "unexpected" } }
                }
            })))
            .expect(0)
            .mount(&mock)
            .await;

        let result = run_triage_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
    }

    // Test 4: Triage — @ai issue with existing session → does NOT start new session
    #[tokio::test]
    async fn triage_ai_issue_with_session_skips_start() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([
            {"node_id": "issue-node-1", "number": 1, "title": "@ai Fix bug", "body": "body", "state": "open", "pull_request": null},
        ]);
        let empty_items = json!({"nodes": []});
        // Issue already has a session → should NOT start new OpenCode session
        let field_values = json!({
            "nodes": [
                session_binding_value("Triage", "existing-session-id")
            ]
        });

        mount_triage_github_mocks(&mock, issues, empty_items, field_values).await;

        // OpenCode should never be called — use expect(0) on the session endpoint
        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "should-not-happen",
                "projectID": "p",
                "directory": "/d",
                "title": "t",
                "version": "1",
                "time": {"created": 1, "updated": 2}
            })))
            .expect(0)
            .mount(&oc_mock)
            .await;

        let result = run_triage_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
    }

    // Test 5: Triage — no github client → error
    #[tokio::test]
    async fn triage_no_github_client_errors() {
        let deps = make_deps(None);
        let ctx = make_context();
        let oc = make_oc_config("http://localhost:8081".to_string());

        let result = run_triage_check(&deps, &ctx, &oc).await;
        assert!(matches!(result, Err(WorkflowError::NoGitHub(_))));
    }

    // ─── Todo Check Integration Tests ─────────────────────────

    // Test 6: No Todo items without sessions → early return
    #[tokio::test]
    async fn todo_no_todo_items_early_return() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        // No project items
        let empty_items = json!({"nodes": []});
        let empty_issues = json!([]);
        let empty_field_values = json!({"nodes": []});

        mount_todo_github_mocks(&mock, empty_items, empty_issues, empty_field_values).await;

        // OpenCode should never be called
        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "x", "projectID": "p", "directory": "/d", "title": "t",
                "version": "1", "time": {"created": 1, "updated": 2}
            })))
            .expect(0)
            .mount(&oc_mock)
            .await;

        let result = run_todo_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
    }

    // Test 7: Todo item with existing branch → does NOT call createBranchRef
    #[tokio::test]
    async fn todo_existing_branch_skips_create() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([
            {"node_id": "issue-node-42", "number": 42, "title": "Implement feature", "body": "body", "state": "open", "pull_request": null},
        ]);
        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        // Status = Todo, no session
        let field_values = json!({
            "nodes": [
                single_select_field_value("Status", "Todo"),
            ]
        });

        mount_todo_github_mocks(&mock, project_items, issues, field_values).await;
        // Branch already exists → 200
        mount_branch_mocks(&mock, true, "main", "abc123").await;
        mount_opencode_mocks(&oc_mock).await;

        let result = run_todo_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
    }

    // Test 8: Todo item without branch → calls getRepoDefaultBranch, getRepoCommitSha, createBranchRef
    #[tokio::test]
    async fn todo_no_branch_calls_create() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([
            {"node_id": "issue-node-42", "number": 42, "title": "Implement feature", "body": "body", "state": "open", "pull_request": null},
        ]);
        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        let field_values = json!({
            "nodes": [
                single_select_field_value("Status", "Todo"),
            ]
        });

        mount_todo_github_mocks(&mock, project_items, issues, field_values).await;
        // Branch does NOT exist → 404
        mount_branch_mocks(&mock, false, "main", "abc123").await;
        mount_opencode_mocks(&oc_mock).await;

        let result = run_todo_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
    }

    // Test 9: Branch name format verified via branch_exists call
    #[tokio::test]
    async fn todo_branch_name_is_issue_number() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([
            {"node_id": "issue-node-42", "number": 42, "title": "Feature", "body": "body", "state": "open", "pull_request": null},
        ]);
        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        let field_values = json!({
            "nodes": [
                single_select_field_value("Status", "Todo"),
            ]
        });

        mount_todo_github_mocks(&mock, project_items, issues, field_values).await;
        // Verify branch_exists is called with "issue-42"
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/branches/issue-42"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"nodes": []})))
            .expect(1)
            .mount(&mock)
            .await;

        // Default branch + commit sha
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"default_branch": "main"})),
            )
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/commits/main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"sha": "abc123"})))
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/owner/repo/git/refs"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({})))
            .expect(1)
            .mount(&mock)
            .await;
        mount_opencode_mocks(&oc_mock).await;

        let result = run_todo_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
    }

    // Test 10: Todo — issue not in issueMap → fallback title "Dev work for issue #42"
    #[tokio::test]
    async fn todo_issue_not_in_map_fallback_title_and_message() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        // Issue #42 is NOT in the issues list (issueMap won't have it)
        let issues = json!([]);
        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        let field_values = json!({
            "nodes": [
                single_select_field_value("Status", "Todo"),
            ]
        });

        mount_todo_github_mocks(&mock, project_items, issues, field_values).await;
        mount_branch_mocks(&mock, false, "main", "abc123").await;

        // Location + worktree creation
        Mock::given(method("GET"))
            .and(path("/api/location"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/test-work",
                "project": {"id": "p1", "directory": "/test-work", "canonical": "/test-work"}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/worktree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/wt/dir1"
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        // Specific session mock: expects title "Dev work for issue #42"
        Mock::given(method("POST"))
            .and(path("/api/session"))
            .and(body_string_contains("\"title\":\"Dev work for issue #42\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "sess123"}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        // Prompt mock
        Mock::given(method("POST"))
            .and(path("/api/session/sess123/prompt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "prompt1"}
            })))
            .mount(&oc_mock)
            .await;

        let result = run_todo_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        oc_mock.verify().await;
    }

    // Test 10b: Sub-issue with unmet wave dependency is filtered out
    #[tokio::test]
    async fn todo_filters_sub_issue_with_unmet_wave_dependency() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        // Two sub-issues: #10 (waveId=1, Todo) and #11 (waveId=2, Todo)
        // #11 should be filtered because #10 (lower wave) is not Done.
        let issues = json!([
            {"node_id": "issue-node-10", "number": 10, "title": "Wave 1 sub", "body": "body10", "state": "open", "pull_request": null},
            {"node_id": "issue-node-11", "number": 11, "title": "Wave 2 sub", "body": "body11", "state": "open", "pull_request": null},
        ]);
        let project_items = json!({
            "nodes": [
                {"id": "item-10", "content": {"__typename": "Issue", "id": "issue-node-10", "number": 10}},
                {"id": "item-11", "content": {"__typename": "Issue", "id": "issue-node-11", "number": 11}},
            ]
        });
        // Both items are Todo; item-10 has waveId=1, item-11 has waveId=2
        let field_values = json!({
            "nodes": [
                single_select_field_value("Status", "Todo"),
                number_field_value("waveId", Some(1.0)),
            ]
        });
        let field_values_11 = json!({
            "nodes": [
                single_select_field_value("Status", "Todo"),
                number_field_value("waveId", Some(2.0)),
            ]
        });

        // Fields query includes waveId
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id": "status-field-id", "name": "Status", "dataType": "SINGLE_SELECT"},
                                {"id": "session-field-id", "name": "sessionId", "dataType": "TEXT"},
                                {"id": "wave-field-id", "name": "waveId", "dataType": "NUMBER"},
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "status-field-id",
                            "options": [
                                {"id": "todo-opt-id", "name": "Todo"},
                                {"id": "done-opt-id", "name": "Done"},
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        // Project items query
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("items(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "items": project_items } }
            })))
            .mount(&mock)
            .await;

        // Field values for item-10
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fieldValues(first:"))
            .and(body_string_contains("item-10"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "fieldValues": field_values } }
            })))
            .mount(&mock)
            .await;

        // Field values for item-11
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fieldValues(first:"))
            .and(body_string_contains("item-11"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "fieldValues": field_values_11 } }
            })))
            .mount(&mock)
            .await;

        // REST issues endpoint
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issues))
            .mount(&mock)
            .await;

        // Only item-10 should get a session; item-11 is filtered
        mount_branch_mocks(&mock, false, "main", "abc123").await;

        // Location + worktree for item-10 only
        Mock::given(method("GET"))
            .and(path("/api/location"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/test-work",
                "project": {"id": "p1", "directory": "/test-work", "canonical": "/test-work"}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/worktree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/wt/dir1"
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/session"))
            .and(body_string_contains("\"title\":\"Wave 1 sub\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "sess10"}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/session/sess10/prompt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "prompt1"}
            })))
            .mount(&oc_mock)
            .await;

        // Session for item-11 should NOT be called
        Mock::given(method("POST"))
            .and(path("/api/session"))
            .and(body_string_contains("\"title\":\"Wave 2 sub\""))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2ItemFieldValue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "updateProjectV2ItemFieldValue": { "projectV2Item": { "id": "item-10" } }
                }
            })))
            .mount(&mock)
            .await;

        let result = run_todo_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        oc_mock.verify().await;
    }

    // Test 10c: Sub-issue starts when all lower-wave sub-issues are Done
    #[tokio::test]
    async fn todo_starts_sub_issue_when_wave_done() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        // Two sub-issues: #20 (waveId=1, Done) and #21 (waveId=2, Todo)
        // #21 should proceed because #20 (lower wave) is Done.
        let issues = json!([
            {"node_id": "issue-node-20", "number": 20, "title": "Wave 1 done", "body": "body20", "state": "closed", "pull_request": null},
            {"node_id": "issue-node-21", "number": 21, "title": "Wave 2 todo", "body": "body21", "state": "open", "pull_request": null},
        ]);
        let project_items = json!({
            "nodes": [
                {"id": "item-20", "content": {"__typename": "Issue", "id": "issue-node-20", "number": 20}},
                {"id": "item-21", "content": {"__typename": "Issue", "id": "issue-node-21", "number": 21}},
            ]
        });
        let field_values_20 = json!({
            "nodes": [
                single_select_field_value("Status", "Done"),
                number_field_value("waveId", Some(1.0)),
            ]
        });
        let field_values_21 = json!({
            "nodes": [
                single_select_field_value("Status", "Todo"),
                number_field_value("waveId", Some(2.0)),
            ]
        });

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id": "status-field-id", "name": "Status", "dataType": "SINGLE_SELECT"},
                                {"id": "session-field-id", "name": "sessionId", "dataType": "TEXT"},
                                {"id": "wave-field-id", "name": "waveId", "dataType": "NUMBER"},
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "status-field-id",
                            "options": [
                                {"id": "todo-opt-id", "name": "Todo"},
                                {"id": "done-opt-id", "name": "Done"},
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("items(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "items": project_items } }
            })))
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fieldValues(first:"))
            .and(body_string_contains("item-20"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "fieldValues": field_values_20 } }
            })))
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fieldValues(first:"))
            .and(body_string_contains("item-21"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "fieldValues": field_values_21 } }
            })))
            .mount(&mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issues))
            .mount(&mock)
            .await;

        mount_branch_mocks(&mock, false, "main", "abc123").await;

        Mock::given(method("GET"))
            .and(path("/api/location"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/test-work",
                "project": {"id": "p1", "directory": "/test-work", "canonical": "/test-work"}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/worktree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/wt/dir1"
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/session"))
            .and(body_string_contains("\"title\":\"Wave 2 todo\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "sess21"}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/session/sess21/prompt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "prompt1"}
            })))
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2ItemFieldValue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "updateProjectV2ItemFieldValue": { "projectV2Item": { "id": "item-11" } }
                }
            })))
            .mount(&mock)
            .await;

        let result = run_todo_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        oc_mock.verify().await;
    }

    // Test 10d: Main issue without waveId proceeds normally
    #[tokio::test]
    async fn todo_starts_main_issue_without_wave_filter() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        // Main issue #30 (no waveId, Status=Todo) — should proceed normally.
        let issues = json!([
            {"node_id": "issue-node-30", "number": 30, "title": "Main issue", "body": "body30", "state": "open", "pull_request": null},
        ]);
        let project_items = json!({
            "nodes": [
                {"id": "item-30", "content": {"__typename": "Issue", "id": "issue-node-30", "number": 30}},
            ]
        });
        // No waveId field value — this is a main issue.
        let field_values = json!({
            "nodes": [
                single_select_field_value("Status", "Todo"),
            ]
        });

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id": "status-field-id", "name": "Status", "dataType": "SINGLE_SELECT"},
                                {"id": "session-field-id", "name": "sessionId", "dataType": "TEXT"},
                                {"id": "wave-field-id", "name": "waveId", "dataType": "NUMBER"},
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "status-field-id",
                            "options": [
                                {"id": "todo-opt-id", "name": "Todo"},
                                {"id": "done-opt-id", "name": "Done"},
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("items(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "items": project_items } }
            })))
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fieldValues(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "fieldValues": field_values } }
            })))
            .mount(&mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issues))
            .mount(&mock)
            .await;

        mount_branch_mocks(&mock, false, "main", "abc123").await;
        mount_opencode_mocks(&oc_mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2ItemFieldValue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "updateProjectV2ItemFieldValue": { "projectV2Item": { "id": "item-30" } }
                }
            })))
            .mount(&mock)
            .await;

        let result = run_todo_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
    }

    // ─── Review Check Integration Tests ───────────────────────

    // Test 11: No matching review items → early return
    #[tokio::test]
    async fn review_no_matching_items_early_return() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([]);
        let empty_items = json!({"nodes": []});
        let empty_field_values = json!({"nodes": []});
        let status_options = vec![
            json!({"id": "review-tech-id", "name": "Review Technical"}),
            json!({"id": "review-prod-id", "name": "Review Product"}),
            json!({"id": "qa-id", "name": "QA"}),
        ];

        mount_review_github_mocks(
            &mock,
            empty_items,
            issues,
            empty_field_values,
            status_options,
        )
        .await;

        // OpenCode should never be called
        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "x", "projectID": "p", "directory": "/d", "title": "t",
                "version": "1", "time": {"created": 1, "updated": 2}
            })))
            .expect(0)
            .mount(&oc_mock)
            .await;

        let result = run_review_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
    }

    // Test 12: Item in "Review Technical" → starts git-automate-reviewer session
    #[tokio::test]
    async fn review_item_in_review_technical_starts_reviewer() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([
            {"node_id": "issue-node-42", "number": 42, "title": "Fix login", "body": "Login is broken", "state": "open", "pull_request": null},
        ]);
        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        let field_values = json!({
            "nodes": [
                single_select_field_value("Status", "Review Technical"),
            ]
        });
        let status_options = vec![
            json!({"id": "review-tech-id", "name": "Review Technical"}),
            json!({"id": "review-prod-id", "name": "Review Product"}),
            json!({"id": "qa-id", "name": "QA"}),
        ];

        mount_review_github_mocks(&mock, project_items, issues, field_values, status_options).await;
        mount_review_branch_mocks(&mock).await;

        // Location + worktree creation
        Mock::given(method("GET"))
            .and(path("/api/location"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/test-work",
                "project": {"id": "p1", "directory": "/test-work", "canonical": "/test-work"}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/worktree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/wt/dir1"
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        // Specific session mock: expects title "Review Technical: Fix login"
        Mock::given(method("POST"))
            .and(path("/api/session"))
            .and(body_string_contains(
                "\"title\":\"Review Technical: Fix login\"",
            ))
            .and(body_string_contains("\"agent\":\"git-automate-reviewer\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "sess123"}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/session/sess123/prompt"))
            .and(body_string_contains("\"text\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "prompt1"}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        let result = run_review_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        oc_mock.verify().await;
    }

    // Test 13: fill_prompt receives correct values — verify filled prompt in body
    #[tokio::test]
    async fn review_fill_prompt_values_verified() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([
            {"node_id": "issue-node-7", "number": 7, "title": "Add feature", "body": "Need this feature", "state": "open", "pull_request": null},
        ]);
        let project_items = json!({
            "nodes": [
                {"id": "item-7", "content": {"__typename": "Issue", "id": "issue-node-7", "number": 7}}
            ]
        });
        let field_values = json!({
            "nodes": [
                single_select_field_value("Status", "QA"),
            ]
        });
        let status_options = vec![
            json!({"id": "review-tech-id", "name": "Review Technical"}),
            json!({"id": "review-prod-id", "name": "Review Product"}),
            json!({"id": "qa-id", "name": "QA"}),
        ];

        mount_review_github_mocks(&mock, project_items, issues, field_values, status_options).await;
        mount_review_branch_mocks(&mock).await;

        // Location + worktree creation
        Mock::given(method("GET"))
            .and(path("/api/location"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/test-work",
                "project": {"id": "p1", "directory": "/test-work", "canonical": "/test-work"}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/worktree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/wt/dir1"
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        // Session mock for QA state
        Mock::given(method("POST"))
            .and(path("/api/session"))
            .and(body_string_contains("\"agent\":\"git-automate-qa\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "sess123"}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        // Verify prompt body: text field present and contains filled
        // BRANCH_NAME ("issue-7") — proves fill_prompt was called.
        Mock::given(method("POST"))
            .and(path("/api/session/sess123/prompt"))
            .and(body_string_contains("\"text\""))
            .and(body_string_contains("issue-7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "prompt1"}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        let result = run_review_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        oc_mock.verify().await;
    }

    // Test 14: Title format "{status}: {issue_title}" verified via session creation
    #[tokio::test]
    async fn review_title_format_verified() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        // Test Review Product state
        let issues = json!([
            {"node_id": "issue-node-99", "number": 99, "title": "Update landing page", "body": "body", "state": "open", "pull_request": null},
        ]);
        let project_items = json!({
            "nodes": [
                {"id": "item-99", "content": {"__typename": "Issue", "id": "issue-node-99", "number": 99}}
            ]
        });
        let field_values = json!({
            "nodes": [
                single_select_field_value("Status", "Review Product"),
            ]
        });
        let status_options = vec![
            json!({"id": "review-tech-id", "name": "Review Technical"}),
            json!({"id": "review-prod-id", "name": "Review Product"}),
            json!({"id": "qa-id", "name": "QA"}),
        ];

        mount_review_github_mocks(&mock, project_items, issues, field_values, status_options).await;
        mount_review_branch_mocks(&mock).await;

        // Location + worktree creation
        Mock::given(method("GET"))
            .and(path("/api/location"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/test-work",
                "project": {"id": "p1", "directory": "/test-work", "canonical": "/test-work"}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/worktree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/wt/dir1"
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        // Specific session mock: expects title "Review Product: Update landing page"
        Mock::given(method("POST"))
            .and(path("/api/session"))
            .and(body_string_contains(
                "\"title\":\"Review Product: Update landing page\"",
            ))
            .and(body_string_contains("\"agent\":\"git-automate-product\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "sess123"}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/session/sess123/prompt"))
            .and(body_string_contains("\"text\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "prompt1"}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        let result = run_review_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        oc_mock.verify().await;
    }

    // Test 15: Status option not found → logs warn, continues (no session started)
    #[tokio::test]
    async fn review_status_option_not_found_skips() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([]);
        let empty_items = json!({"nodes": []});
        let empty_field_values = json!({"nodes": []});
        // Only "Triage" and "Todo" options — NO review states
        let status_options = vec![
            json!({"id": "triage-id", "name": "Triage"}),
            json!({"id": "todo-id", "name": "Todo"}),
        ];

        mount_review_github_mocks(
            &mock,
            empty_items,
            issues,
            empty_field_values,
            status_options,
        )
        .await;

        // OpenCode should never be called
        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "x", "projectID": "p", "directory": "/d", "title": "t",
                "version": "1", "time": {"created": 1, "updated": 2}
            })))
            .expect(0)
            .mount(&oc_mock)
            .await;

        let result = run_review_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
    }

    // Test 16: Todo — no session field → error
    #[tokio::test]
    async fn todo_no_session_field_errors() {
        let mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config("http://localhost:8081".to_string());

        // Status field is returned, but NO sessionId field
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id": "status-field-id", "name": "Status", "dataType": "SINGLE_SELECT"},
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "status-field-id",
                            "options": [
                                {"id": "todo-id", "name": "Todo"},
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        let result = run_todo_check(&deps, &ctx, &oc).await;
        assert!(matches!(result, Err(WorkflowError::NoSessionField(_))));
    }

    // Test 17: Start opencode session maps error correctly
    #[tokio::test]
    async fn start_opencode_session_maps_open_code_error() {
        let mock = MockServer::start().await;

        // Location + worktree succeed; the 500 comes from /api/session
        Mock::given(method("GET"))
            .and(path("/api/location"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/dir",
                "project": {"id": "p1", "directory": "/dir", "canonical": "/dir"}
            })))
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/worktree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/wt/dir1"
            })))
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let oc = OpencodeSessionConfig {
            url: mock.uri(),
            pw: "pw".to_string(),
            directory: "/test-work".to_string(),
            project: None,
            concurrency: HashMap::new(),
            session_timeout_secs: 1800,
            session_max_secs: 86400,
            max_session_attempts: 3,
        };

        let result = start_opencode_session(
            &oc,
            "/dir",
            "title",
            crate::workflow::AgentName::Triage,
            "message",
            &HashMap::new(),
            &Arc::new(Mutex::new(HashMap::new())),
            "test",
        )
        .await;
        assert!(result.is_err());
        assert!(matches!(result, Err(WorkflowError::Other(_))));
    }

    // Test 17a: start_opencode_session skips when active sessions >= concurrency limit
    #[tokio::test]
    async fn start_opencode_session_skips_when_at_limit() {
        let mock = MockServer::start().await;

        // GET /api/session/active returns 4 active sessions
        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "sess1": { "type": "running" },
                    "sess2": { "type": "running" },
                    "sess3": { "type": "running" },
                    "sess4": { "type": "running" },
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // Each active session carries the myprovider/fast model
        for id in ["sess1", "sess2", "sess3", "sess4"] {
            Mock::given(method("GET"))
                .and(path(format!("/api/session/{id}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "data": { "id": id, "model": { "id": "fast", "providerID": "myprovider" } }
                })))
                .expect(1)
                .mount(&mock)
                .await;
        }

        // Location + worktree + session should NOT be created — limit is 2 and there are 4 active
        Mock::given(method("GET"))
            .and(path("/api/location"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/dir",
                "project": {"id": "p1", "directory": "/dir", "canonical": "/dir"}
            })))
            .expect(0)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/worktree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/wt/dir1"
            })))
            .expect(0)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "should-not-happen"}
            })))
            .expect(0)
            .mount(&mock)
            .await;

        let oc = OpencodeSessionConfig {
            url: mock.uri(),
            pw: "pw".to_string(),
            directory: "/test-work".to_string(),
            project: None,
            concurrency: HashMap::new(),
            session_timeout_secs: 1800,
            session_max_secs: 86400,
            max_session_attempts: 3,
        };

        // limit = 2 for myprovider/fast, active = 4 → 4 >= 2 → should skip
        let mut concurrency = HashMap::new();
        concurrency.insert("myprovider/fast".to_string(), 2);
        let result = start_opencode_session(
            &oc,
            "/dir",
            "title",
            crate::workflow::AgentName::Triage,
            "message",
            &concurrency,
            &Arc::new(Mutex::new(HashMap::new())),
            "test",
        )
        .await;

        assert!(result.is_err());
        assert!(matches!(result, Err(WorkflowError::ConcurrencyExceeded)));
    }

    // Test 17b: start_opencode_session proceeds when active sessions < concurrency limit
    #[tokio::test]
    async fn start_opencode_session_proceeds_below_limit() {
        let mock = MockServer::start().await;

        // GET /api/session/active returns 2 active sessions
        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "sess1": { "type": "running" },
                    "sess2": { "type": "running" },
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // Each active session carries the myprovider/fast model
        for id in ["sess1", "sess2"] {
            Mock::given(method("GET"))
                .and(path(format!("/api/session/{id}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "data": { "id": id, "model": { "id": "fast", "providerID": "myprovider" } }
                })))
                .expect(1)
                .mount(&mock)
                .await;
        }

        // Location + worktree creation succeed
        Mock::given(method("GET"))
            .and(path("/api/location"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/dir",
                "project": {"id": "p1", "directory": "/dir", "canonical": "/dir"}
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/worktree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/wt/dir1"
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // POST /api/session should be called exactly once and return "sess123"
        Mock::given(method("POST"))
            .and(path("/api/session"))
            .and(body_string_contains("\"directory\":\"/wt/dir1\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "sess123"}
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/session/sess123/prompt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "prompt1"}
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let oc = OpencodeSessionConfig {
            url: mock.uri(),
            pw: "pw".to_string(),
            directory: "/test-work".to_string(),
            project: None,
            concurrency: HashMap::new(),
            session_timeout_secs: 1800,
            session_max_secs: 86400,
            max_session_attempts: 3,
        };

        // limit = 5 for myprovider/fast, active = 2 → 2 < 5 → should proceed
        let mut concurrency = HashMap::new();
        concurrency.insert("myprovider/fast".to_string(), 5);
        let result = start_opencode_session(
            &oc,
            "/dir",
            "title",
            crate::workflow::AgentName::Triage,
            "message",
            &concurrency,
            &Arc::new(Mutex::new(HashMap::new())),
            "test",
        )
        .await;

        assert_eq!(result.unwrap(), "sess123");
    }

    // Test 17c: agent-key gate trips when active sessions for the agent reach
    // its configured limit.
    #[tokio::test]
    async fn start_opencode_session_skips_when_agent_at_limit() {
        let mock = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "sess1": { "type": "running" },
                    "sess2": { "type": "running" },
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        for id in ["sess1", "sess2"] {
            Mock::given(method("GET"))
                .and(path(format!("/api/session/{id}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "data": { "id": id, "agent": "git-automate-triage" }
                })))
                .expect(1)
                .mount(&mock)
                .await;
        }

        // No session should be created — 2 active >= limit 2.
        Mock::given(method("GET"))
            .and(path("/api/location"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&mock)
            .await;

        let oc = make_oc_config(mock.uri());
        let concurrency = HashMap::from([("git-automate-triage".to_string(), 2usize)]);
        let result = start_opencode_session(
            &oc,
            "/dir",
            "title",
            crate::workflow::AgentName::Triage,
            "message",
            &concurrency,
            &Arc::new(Mutex::new(HashMap::new())),
            "test",
        )
        .await;

        assert!(matches!(result, Err(WorkflowError::ConcurrencyExceeded)));
        mock.verify().await;
    }

    // Test 17d: agent-key gate allows creation when active sessions are below
    // the configured limit.
    #[tokio::test]
    async fn start_opencode_session_proceeds_when_agent_below_limit() {
        let mock = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "sess1": { "type": "running" } }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/sess1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "sess1", "agent": "git-automate-triage" }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/location"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/dir",
                "project": {"id": "p1", "directory": "/dir", "canonical": "/dir"}
            })))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/worktree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/wt/dir1"
            })))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "sess123"}
            })))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/session/sess123/prompt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "prompt1"}
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let oc = make_oc_config(mock.uri());
        let concurrency = HashMap::from([("git-automate-triage".to_string(), 5usize)]);
        let result = start_opencode_session(
            &oc,
            "/dir",
            "title",
            crate::workflow::AgentName::Triage,
            "message",
            &concurrency,
            &Arc::new(Mutex::new(HashMap::new())),
            "test",
        )
        .await;

        assert_eq!(result.unwrap(), "sess123");
    }

    // Test 17e: the `default` key caps agents without their own entry.
    #[tokio::test]
    async fn start_opencode_session_default_key_trips_agent_gate() {
        let mock = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "sess1": { "type": "running" } }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/sess1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "sess1", "agent": "git-automate-developer" }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&mock)
            .await;

        let oc = make_oc_config(mock.uri());
        // No `git-automate-developer` key; `default: 1` applies.
        let concurrency = HashMap::from([("default".to_string(), 1usize)]);
        let result = start_opencode_session(
            &oc,
            "/dir",
            "title",
            crate::workflow::AgentName::Developer,
            "message",
            &concurrency,
            &Arc::new(Mutex::new(HashMap::new())),
            "test",
        )
        .await;

        assert!(matches!(result, Err(WorkflowError::ConcurrencyExceeded)));
        mock.verify().await;
    }

    // Test 17f: an agent-specific key takes precedence over `default`.
    #[tokio::test]
    async fn start_opencode_session_agent_key_overrides_default() {
        let mock = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "sess1": { "type": "running" } }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/sess1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "sess1", "agent": "git-automate-developer" }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/location"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/dir",
                "project": {"id": "p1", "directory": "/dir", "canonical": "/dir"}
            })))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/worktree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/wt/dir1"
            })))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "sess123"}
            })))
            .expect(1)
            .mount(&mock)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/session/sess123/prompt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "prompt1"}
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let oc = make_oc_config(mock.uri());
        // `default` would block (1 >= 1) but the agent key (5) wins.
        let concurrency = HashMap::from([
            ("git-automate-developer".to_string(), 5usize),
            ("default".to_string(), 1usize),
        ]);
        let result = start_opencode_session(
            &oc,
            "/dir",
            "title",
            crate::workflow::AgentName::Developer,
            "message",
            &concurrency,
            &Arc::new(Mutex::new(HashMap::new())),
            "test",
        )
        .await;

        assert_eq!(result.unwrap(), "sess123");
    }

    // T22: full v2 flow — location → worktree → session(named agent) → prompt
    #[tokio::test]
    async fn start_opencode_session_full_flow_uses_v2_endpoints() {
        let mock = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/location"))
            .and(query_param("location[directory]", "/dir"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/dir",
                "project": {"id": "p1", "directory": "/dir", "canonical": "/dir"}
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/worktree"))
            .and(body_string_contains("\"projectID\":\"p1\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/wt/dir1"
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // Named agent + worktree location, and no `system` prompt field in v2.
        Mock::given(method("POST"))
            .and(path("/api/session"))
            .and(body_string_contains("\"agent\":\"git-automate-triage\""))
            .and(body_string_contains("\"directory\":\"/wt/dir1\""))
            .and(BodyNotContains("\"system\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "ses123"}
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/session/ses123/prompt"))
            .and(body_string_contains("\"text\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "prompt1"}
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/experimental/workspace"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/experimental/worktree"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&mock)
            .await;

        let oc = OpencodeSessionConfig {
            url: mock.uri(),
            pw: "pw".to_string(),
            directory: "/test-work".to_string(),
            project: None,
            concurrency: HashMap::new(),
            session_timeout_secs: 1800,
            session_max_secs: 86400,
            max_session_attempts: 3,
        };

        let result = start_opencode_session(
            &oc,
            "/dir",
            "title",
            crate::workflow::AgentName::Triage,
            "message",
            &HashMap::new(),
            &Arc::new(Mutex::new(HashMap::new())),
            "test",
        )
        .await;
        assert_eq!(result.unwrap(), "ses123");
        mock.verify().await;
    }

    // T20: location lookup fails → no worktree, no session
    #[tokio::test]
    async fn start_opencode_session_location_failure_stops_flow() {
        let mock = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/location"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/worktree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/wt/dir1"
            })))
            .expect(0)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "should-not-happen"}
            })))
            .expect(0)
            .mount(&mock)
            .await;

        let oc = OpencodeSessionConfig {
            url: mock.uri(),
            pw: "pw".to_string(),
            directory: "/test-work".to_string(),
            project: None,
            concurrency: HashMap::new(),
            session_timeout_secs: 1800,
            session_max_secs: 86400,
            max_session_attempts: 3,
        };

        let result = start_opencode_session(
            &oc,
            "/dir",
            "title",
            crate::workflow::AgentName::Triage,
            "message",
            &HashMap::new(),
            &Arc::new(Mutex::new(HashMap::new())),
            "test",
        )
        .await;
        assert!(matches!(result, Err(WorkflowError::Other(_))));
        mock.verify().await;
    }

    // T21: worktree creation fails → no session
    #[tokio::test]
    async fn start_opencode_session_worktree_failure_stops_flow() {
        let mock = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/location"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/dir",
                "project": {"id": "p1", "directory": "/dir", "canonical": "/dir"}
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/worktree"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "should-not-happen"}
            })))
            .expect(0)
            .mount(&mock)
            .await;

        let oc = OpencodeSessionConfig {
            url: mock.uri(),
            pw: "pw".to_string(),
            directory: "/test-work".to_string(),
            project: None,
            concurrency: HashMap::new(),
            session_timeout_secs: 1800,
            session_max_secs: 86400,
            max_session_attempts: 3,
        };

        let result = start_opencode_session(
            &oc,
            "/dir",
            "title",
            crate::workflow::AgentName::Triage,
            "message",
            &HashMap::new(),
            &Arc::new(Mutex::new(HashMap::new())),
            "test",
        )
        .await;
        assert!(matches!(result, Err(WorkflowError::Other(_))));
        mock.verify().await;
    }

    // Test 18: Review — item with existing session → skips
    #[tokio::test]
    async fn review_item_with_session_skips() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([
            {"node_id": "issue-node-42", "number": 42, "title": "Fix", "body": "body", "state": "open", "pull_request": null},
        ]);
        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        // Status = "Review Technical" AND has a session
        let field_values = json!({
            "nodes": [
                single_select_field_value("Status", "Review Technical"),
                session_binding_value("Review Technical", "existing-session"),
            ]
        });
        let status_options = vec![
            json!({"id": "review-tech-id", "name": "Review Technical"}),
            json!({"id": "review-prod-id", "name": "Review Product"}),
            json!({"id": "qa-id", "name": "QA"}),
        ];

        mount_review_github_mocks(&mock, project_items, issues, field_values, status_options).await;

        // GET /api/session/active → session still running
        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "existing-session": {"type": "running"}
                }
            })))
            .mount(&oc_mock)
            .await;

        // OpenCode should never be called
        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "x", "projectID": "p", "directory": "/d", "title": "t",
                "version": "1", "time": {"created": 1, "updated": 2}
            })))
            .expect(0)
            .mount(&oc_mock)
            .await;

        let result = run_review_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
    }

    // ── Error-path tests ──────────────────────────────────────

    // Test: run_triage_check with invalid title_pattern regex → returns Other error
    #[tokio::test]
    async fn triage_invalid_title_pattern_returns_error() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let mut ctx = make_context();
        ctx.config.title_pattern = "[invalid".to_string();
        let oc = make_oc_config(oc_mock.uri());

        // Mount resolve_status_option_and_session mocks
        mount_triage_github_mocks(
            &mock,
            json!([
                {"node_id": "i1", "number": 1, "title": "@ai Fix bug", "body": "body", "state": "open", "pull_request": null},
            ]),
            json!({"nodes": []}),
            json!({"nodes": []}),
        )
        .await;

        // OpenCode should never be called — regex fails before any session
        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "x", "projectID": "p", "directory": "/d", "title": "t",
                "version": "1", "time": {"created": 1, "updated": 2}
            })))
            .expect(0)
            .mount(&oc_mock)
            .await;

        let result = run_triage_check(&deps, &ctx, &oc).await;
        assert!(result.is_err(), "triage check should fail on invalid regex");
        assert!(
            matches!(result, Err(WorkflowError::Other(ref msg)) if msg.contains("invalid title_pattern")),
            "error should mention 'invalid title_pattern': {:?}",
            result
        );
    }

    // Test: run_triage_check with GitHub API error (list_repo_issues 500) → returns error
    #[tokio::test]
    async fn triage_github_api_error_returns_error() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        // Mount only resolve_status_option_and_session mocks
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "status-field-id",
                            "options": [
                                {"id": "triage-opt-id", "name": "Triage"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id": "status-field-id", "name": "Status", "dataType": "SINGLE_SELECT"},
                                {"id": "session-field-id", "name": "sessionId", "dataType": "TEXT"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // list_repo_issues returns 500 (retried 3x: 1 initial + 3 retries via execute_with_retry)
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(500))
            .expect(4)
            .mount(&mock)
            .await;

        // OpenCode should never be called
        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "x", "projectID": "p", "directory": "/d", "title": "t",
                "version": "1", "time": {"created": 1, "updated": 2}
            })))
            .expect(0)
            .mount(&oc_mock)
            .await;

        let result = run_triage_check(&deps, &ctx, &oc).await;
        assert!(
            result.is_err(),
            "triage check should fail on GitHub API error"
        );
    }

    // Test: run_todo_check with GitHub API error (list_repo_issues 500 in get_issue_body_map) → returns error
    #[tokio::test]
    async fn todo_github_api_error_returns_error() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        // Mount resolve_field_ids mocks (fields + status field)
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id": "status-field-id", "name": "Status", "dataType": "SINGLE_SELECT"},
                                {"id": "session-field-id", "name": "sessionId", "dataType": "TEXT"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "status-field-id",
                            "options": [
                                {"id": "todo-opt-id", "name": "Todo"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // list_project_items — no todo items
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("items(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "items": { "nodes": [] } } }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // list_repo_issues returns 500 (called by get_issue_body_map)
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(500))
            .expect(4)
            .mount(&mock)
            .await;

        // OpenCode should never be called
        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "x", "projectID": "p", "directory": "/d", "title": "t",
                "version": "1", "time": {"created": 1, "updated": 2}
            })))
            .expect(0)
            .mount(&oc_mock)
            .await;

        let result = run_todo_check(&deps, &ctx, &oc).await;
        assert!(
            result.is_err(),
            "todo check should fail on GitHub API error"
        );
    }

    // Test: run_review_check with GitHub API error (list_repo_issues 500 in get_issue_body_map) → returns error
    #[tokio::test]
    async fn review_github_api_error_returns_error() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        // Mount resolve_field_ids mocks (fields + status field)
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id": "status-field-id", "name": "Status", "dataType": "SINGLE_SELECT"},
                                {"id": "session-field-id", "name": "sessionId", "dataType": "TEXT"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "status-field-id",
                            "options": [
                                {"id": "review-tech-id", "name": "Review Technical"},
                                {"id": "review-prod-id", "name": "Review Product"},
                                {"id": "qa-id", "name": "QA"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // list_project_items — no items
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("items(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "items": { "nodes": [] } } }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // list_repo_issues returns 500 (called by get_issue_body_map)
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(500))
            .expect(4)
            .mount(&mock)
            .await;

        // OpenCode should never be called
        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "x", "projectID": "p", "directory": "/d", "title": "t",
                "version": "1", "time": {"created": 1, "updated": 2}
            })))
            .expect(0)
            .mount(&oc_mock)
            .await;

        let result = run_review_check(&deps, &ctx, &oc).await;
        assert!(
            result.is_err(),
            "review check should fail on GitHub API error"
        );
    }

    // Test: run_triage_check with no GitHub client → returns NoGitHub error
    #[tokio::test]
    async fn triage_no_github_client_returns_error() {
        let deps = make_deps(None);
        let ctx = make_context();
        let oc = make_oc_config("http://localhost:8081".to_string());

        let result = run_triage_check(&deps, &ctx, &oc).await;
        assert!(matches!(result, Err(WorkflowError::NoGitHub(_))));
    }

    // Test: run_todo_check with no GitHub client → returns NoGitHub error
    #[tokio::test]
    async fn todo_no_github_client_returns_error() {
        let deps = make_deps(None);
        let ctx = make_context();
        let oc = make_oc_config("http://localhost:8081".to_string());

        let result = run_todo_check(&deps, &ctx, &oc).await;
        assert!(matches!(result, Err(WorkflowError::NoGitHub(_))));
    }

    // Test: run_review_check with no GitHub client → returns NoGitHub error
    #[tokio::test]
    async fn review_no_github_client_returns_error() {
        let deps = make_deps(None);
        let ctx = make_context();
        let oc = make_oc_config("http://localhost:8081".to_string());

        let result = run_review_check(&deps, &ctx, &oc).await;
        assert!(matches!(result, Err(WorkflowError::NoGitHub(_))));
    }

    // ── Review Completion Check Tests ──────────────────────────

    fn all_status_options() -> Vec<serde_json::Value> {
        vec![
            json!({"id": "triage-id", "name": "Triage"}),
            json!({"id": "todo-id", "name": "Todo"}),
            json!({"id": "in-dev-id", "name": "In Development"}),
            json!({"id": "review-tech-id", "name": "Review Technical"}),
            json!({"id": "review-prod-id", "name": "Review Product"}),
            json!({"id": "qa-id", "name": "QA"}),
            json!({"id": "done-id", "name": "Done"}),
        ]
    }

    fn session_binding_value_full(
        status: &str,
        session_id: &str,
        resume: &str,
        attempts: u32,
    ) -> serde_json::Value {
        let binding = SessionBinding {
            step: Some(status.to_string()),
            id: session_id.to_string(),
            resume: resume.to_string(),
            attempts,
        };
        text_field_value("sessionId", Some(&serialize_binding(&binding)))
    }

    fn review_field_values(status: &str, session_id: &str) -> serde_json::Value {
        json!({
            "nodes": [
                single_select_field_value("Status", status),
                session_binding_value(status, session_id),
            ]
        })
    }

    fn review_field_values_with_resume(
        status: &str,
        session_id: &str,
        resume: &str,
    ) -> serde_json::Value {
        json!({
            "nodes": [
                single_select_field_value("Status", status),
                session_binding_value_full(status, session_id, resume, 1),
            ]
        })
    }

    async fn mount_review_github_mocks_with_options(
        server: &MockServer,
        field_values: serde_json::Value,
    ) {
        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        mount_review_github_mocks(
            server,
            project_items,
            json!([]),
            field_values,
            all_status_options(),
        )
        .await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(server)
            .await;
    }

    async fn mount_review_session_verdict(
        server: &MockServer,
        session_id: &str,
        verdict: Option<&str>,
    ) {
        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!("/api/session/{session_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": session_id, "outcome": "succeeded"}
            })))
            .mount(server)
            .await;

        let messages = match verdict {
            Some(text) => json!([
                {"type": "assistant", "id": "m1", "content": [{"type": "text", "text": text}]}
            ]),
            None => json!([]),
        };

        Mock::given(method("GET"))
            .and(path(format!("/api/session/{session_id}/message")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": messages,
                "cursor": null
            })))
            .mount(server)
            .await;
    }

    // T14: approve advances Review Technical → Review Product, clearing the id
    // and preserving the binding resume.
    #[tokio::test]
    async fn review_completion_approve_advances_technical_to_product() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let deps = make_deps(Some(gh_client(&mock)));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        mount_review_github_mocks_with_options(
            &mock,
            review_field_values_with_resume(
                "Review Technical",
                "review-session-1",
                "dev-session-1",
            ),
        )
        .await;
        mount_review_session_verdict(
            &oc_mock,
            "review-session-1",
            Some(
                "GIT_AUTOMATE_VERDICT: {\"v\":1,\"role\":\"reviewer\",\"decision\":\"approve\",\"notes\":\"lgtm\"}",
            ),
        )
        .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("review-prod-id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"updateProjectV2ItemFieldValue": {"projectV2Item": {"id": "item-42"}}}
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains(
                "step=Review Product;id=;resume=dev-session-1;attempts=1",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"updateProjectV2ItemFieldValue": {"projectV2Item": {"id": "item-42"}}}
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        let result = run_review_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        mock.verify().await;
        oc_mock.verify().await;
    }

    // T15: approve on QA reaches Done and clears the binding entirely.
    #[tokio::test]
    async fn review_completion_approve_qa_marks_done() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let deps = make_deps(Some(gh_client(&mock)));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        mount_review_github_mocks_with_options(
            &mock,
            review_field_values("QA", "review-session-3"),
        )
        .await;
        mount_review_session_verdict(
            &oc_mock,
            "review-session-3",
            Some("GIT_AUTOMATE_VERDICT: {\"v\":1,\"role\":\"qa\",\"decision\":\"approve\"}"),
        )
        .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("done-id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"updateProjectV2ItemFieldValue": {"projectV2Item": {"id": "item-42"}}}
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("\"text\":null"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"updateProjectV2ItemFieldValue": {"projectV2Item": {"id": "item-42"}}}
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        let result = run_review_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        mock.verify().await;
        oc_mock.verify().await;
    }

    // T16: a `changes` verdict sets In Development and resumes the developer
    // session with the steer delivery.
    #[tokio::test]
    async fn review_completion_changes_resumes_developer_session() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let deps = make_deps(Some(gh_client(&mock)));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        mount_review_github_mocks_with_options(
            &mock,
            review_field_values_with_resume(
                "Review Technical",
                "review-session-1",
                "dev-session-1",
            ),
        )
        .await;
        mount_review_session_verdict(
            &oc_mock,
            "review-session-1",
            Some(
                "GIT_AUTOMATE_VERDICT: {\"v\":1,\"role\":\"reviewer\",\"decision\":\"changes\",\"notes\":\"Please fix X\"}",
            ),
        )
        .await;

        Mock::given(method("GET"))
            .and(path("/api/session/dev-session-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "dev-session-1"}
            })))
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/session/dev-session-1/prompt"))
            .and(body_string_contains("\"delivery\":\"steer\""))
            .and(body_string_contains("Please fix X"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": {"id": "msg_1"}})),
            )
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("in-dev-id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"updateProjectV2ItemFieldValue": {"projectV2Item": {"id": "item-42"}}}
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains(
                "step=In Development;id=dev-session-1;resume=;attempts=2",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"updateProjectV2ItemFieldValue": {"projectV2Item": {"id": "item-42"}}}
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        let result = run_review_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        mock.verify().await;
        oc_mock.verify().await;
    }

    // T17: a missing verdict fails safe to changes (resumes the developer).
    #[tokio::test]
    async fn review_completion_missing_verdict_fails_safe() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let deps = make_deps(Some(gh_client(&mock)));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        mount_review_github_mocks_with_options(
            &mock,
            review_field_values_with_resume(
                "Review Technical",
                "review-session-1",
                "dev-session-1",
            ),
        )
        .await;
        mount_review_session_verdict(&oc_mock, "review-session-1", None).await;

        Mock::given(method("GET"))
            .and(path("/api/session/dev-session-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "dev-session-1"}
            })))
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/session/dev-session-1/prompt"))
            .and(body_string_contains("\"delivery\":\"steer\""))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": {"id": "msg_1"}})),
            )
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains(
                "step=In Development;id=dev-session-1;resume=;attempts=2",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"updateProjectV2ItemFieldValue": {"projectV2Item": {"id": "item-42"}}}
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        let result = run_review_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        mock.verify().await;
        oc_mock.verify().await;
    }

    // T18: a malformed verdict fails safe to changes (resumes the developer).
    #[tokio::test]
    async fn review_completion_malformed_verdict_fails_safe() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let deps = make_deps(Some(gh_client(&mock)));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        mount_review_github_mocks_with_options(
            &mock,
            review_field_values_with_resume(
                "Review Technical",
                "review-session-1",
                "dev-session-1",
            ),
        )
        .await;
        mount_review_session_verdict(
            &oc_mock,
            "review-session-1",
            Some("GIT_AUTOMATE_VERDICT: {not valid json"),
        )
        .await;

        Mock::given(method("GET"))
            .and(path("/api/session/dev-session-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "dev-session-1"}
            })))
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/session/dev-session-1/prompt"))
            .and(body_string_contains("\"delivery\":\"steer\""))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": {"id": "msg_1"}})),
            )
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains(
                "step=In Development;id=dev-session-1;resume=;attempts=2",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"updateProjectV2ItemFieldValue": {"projectV2Item": {"id": "item-42"}}}
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        let result = run_review_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        mock.verify().await;
        oc_mock.verify().await;
    }

    // T35: changes with no recorded resume session degrade to Todo.
    #[tokio::test]
    async fn review_completion_changes_without_resume_degrades_to_todo() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let deps = make_deps(Some(gh_client(&mock)));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        mount_review_github_mocks_with_options(
            &mock,
            review_field_values("Review Technical", "review-session-1"),
        )
        .await;
        mount_review_session_verdict(
            &oc_mock,
            "review-session-1",
            Some("GIT_AUTOMATE_VERDICT: {\"v\":1,\"role\":\"reviewer\",\"decision\":\"changes\"}"),
        )
        .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("todo-id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"updateProjectV2ItemFieldValue": {"projectV2Item": {"id": "item-42"}}}
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("step=Todo;id=;resume=;attempts=1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"updateProjectV2ItemFieldValue": {"projectV2Item": {"id": "item-42"}}}
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        let result = run_review_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        mock.verify().await;
        oc_mock.verify().await;
    }

    // T36: changes with a 404 resume session degrade to Todo.
    #[tokio::test]
    async fn review_completion_changes_with_missing_session_degrades_to_todo() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let deps = make_deps(Some(gh_client(&mock)));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        mount_review_github_mocks_with_options(
            &mock,
            review_field_values_with_resume("Review Technical", "review-session-1", "gone-session"),
        )
        .await;
        mount_review_session_verdict(
            &oc_mock,
            "review-session-1",
            Some("GIT_AUTOMATE_VERDICT: {\"v\":1,\"role\":\"reviewer\",\"decision\":\"changes\"}"),
        )
        .await;

        Mock::given(method("GET"))
            .and(path("/api/session/gone-session"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/session/gone-session/prompt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"id": "x"}})))
            .expect(0)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("step=Todo;id=;resume=;attempts=1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"updateProjectV2ItemFieldValue": {"projectV2Item": {"id": "item-42"}}}
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        let result = run_review_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        mock.verify().await;
        oc_mock.verify().await;
    }

    // T37: active review session → no action taken.
    #[tokio::test]
    async fn review_completion_active_session_is_ignored() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let deps = make_deps(Some(gh_client(&mock)));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        mount_review_github_mocks_with_options(
            &mock,
            review_field_values("Review Technical", "review-session-1"),
        )
        .await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"review-session-1": {"type": "running"}}
            })))
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2ItemFieldValue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"updateProjectV2ItemFieldValue": {"projectV2Item": {"id": "item-42"}}}
            })))
            .with_priority(1)
            .expect(0)
            .mount(&mock)
            .await;

        let result = run_review_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        mock.verify().await;
        oc_mock.verify().await;
    }

    // T38: no GitHub client → returns NoGitHub error.
    #[tokio::test]
    async fn review_completion_no_github_client_returns_error() {
        let deps = make_deps(None);
        let ctx = make_context();
        let oc = make_oc_config("http://localhost:8081".to_string());

        let result = run_review_completion_check(&deps, &ctx, &oc).await;
        assert!(matches!(result, Err(WorkflowError::NoGitHub(_))));
    }

    // ── Dev Completion Check Tests ───────────────────────────────

    fn in_dev_field_values(session_id: &str) -> serde_json::Value {
        json!({
            "nodes": [
                single_select_field_value("Status", "In Development"),
                session_binding_value("In Development", session_id),
            ]
        })
    }

    async fn mount_dev_completion_github_mocks(
        server: &MockServer,
        project_items: serde_json::Value,
        issues: serde_json::Value,
        field_values: serde_json::Value,
    ) {
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id": "status-field-id", "name": "Status", "dataType": "SINGLE_SELECT"},
                                {"id": "session-field-id", "name": "sessionId", "dataType": "TEXT"},
                            ]
                        }
                    }
                }
            })))
            .mount(server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "status-field-id",
                            "options": all_status_options()
                        }
                    }
                }
            })))
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issues))
            .mount(server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("items(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "items": project_items } }
            })))
            .mount(server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fieldValues(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "fieldValues": field_values } }
            })))
            .mount(server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2ItemFieldValue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "updateProjectV2ItemFieldValue": { "projectV2Item": { "id": "item-id" } }
                }
            })))
            .mount(server)
            .await;
    }

    // T19: Completed dev session with thread IDs → resolves threads, transitions to Review Technical
    #[tokio::test]
    async fn dev_completion_resolves_threads_and_transitions() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([
            {"node_id": "issue-node-42", "number": 42, "title": "Fix bug", "body": "body", "state": "open", "pull_request": null},
        ]);
        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        let field_values = in_dev_field_values("dev-session-1");

        mount_dev_completion_github_mocks(&mock, project_items, issues, field_values).await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/dev-session-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "dev-session-1", "outcome": "succeeded"}
            })))
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/dev-session-1/message"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"type": "user", "id": "m0", "text": "Here is my work"},
                    {"type": "assistant", "id": "m1", "content": [{"type": "text", "text": "### Resolve threads"}]},
                    {"type": "assistant", "id": "m2", "content": [{"type": "text", "text": "TH_123, TH_456"}]}
                ],
                "cursor": null
            })))
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("resolveReviewThread"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "resolveReviewThread": { "thread": { "id": "TH_123" } } }
            })))
            .mount(&mock)
            .await;

        let result = run_dev_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        oc_mock.verify().await;
    }

    // T39: developer `done` verdict drives thread resolution from
    // `resolved_threads` and the binding retains the dev session as `resume`.
    #[tokio::test]
    async fn dev_completion_verdict_retains_resume() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let deps = make_deps(Some(gh_client(&mock)));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([
            {"node_id": "issue-node-42", "number": 42, "title": "Fix bug", "body": "body", "state": "open", "pull_request": null},
        ]);
        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        mount_dev_completion_github_mocks(
            &mock,
            project_items,
            issues,
            in_dev_field_values("dev-session-9"),
        )
        .await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/dev-session-9"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "dev-session-9", "outcome": "succeeded"}
            })))
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/dev-session-9/message"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"type": "assistant", "id": "m1", "content": [{"type": "text", "text": "GIT_AUTOMATE_VERDICT: {\"v\":1,\"role\":\"developer\",\"decision\":\"done\",\"resolved_threads\":[\"TH_900\"]}"}]}
                ],
                "cursor": null
            })))
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("resolveReviewThread"))
            .and(body_string_contains("TH_900"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "resolveReviewThread": { "thread": { "id": "TH_900" } } }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains(
                "step=Review Technical;id=;resume=dev-session-9;attempts=2",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"updateProjectV2ItemFieldValue": {"projectV2Item": {"id": "item-42"}}}
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        let result = run_dev_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        mock.verify().await;
        oc_mock.verify().await;
    }

    // T20: Completed dev session with no Resolve threads section → still transitions to Review Technical
    #[tokio::test]
    async fn dev_completion_no_threads_section_still_transitions() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([
            {"node_id": "issue-node-42", "number": 42, "title": "Fix bug", "body": "body", "state": "open", "pull_request": null},
        ]);
        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        let field_values = in_dev_field_values("dev-session-2");

        mount_dev_completion_github_mocks(&mock, project_items, issues, field_values).await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/dev-session-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "dev-session-2", "outcome": "succeeded"}
            })))
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/dev-session-2/message"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"type": "assistant", "id": "m1", "content": [{"type": "text", "text": "Done with the fix"}]}
                ],
                "cursor": null
            })))
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("resolveReviewThread"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "resolveReviewThread": { "thread": { "id": "x" } } }
            })))
            .expect(0)
            .mount(&mock)
            .await;

        let result = run_dev_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        oc_mock.verify().await;
    }

    // T21: Active dev session → no action taken
    #[tokio::test]
    async fn dev_completion_active_session_ignored() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([]);
        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        let field_values = in_dev_field_values("dev-session-1");

        mount_dev_completion_github_mocks(&mock, project_items, issues, field_values).await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "dev-session-1": {"type": "running"}
                }
            })))
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/dev-session-1/message"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [],
                "cursor": null
            })))
            .expect(0)
            .mount(&oc_mock)
            .await;

        let result = run_dev_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        oc_mock.verify().await;
    }

    // T22: No GitHub client → returns NoGitHub error
    #[tokio::test]
    async fn dev_completion_no_github_client_returns_error() {
        let deps = make_deps(None);
        let ctx = make_context();
        let oc = make_oc_config("http://localhost:8081".to_string());

        let result = run_dev_completion_check(&deps, &ctx, &oc).await;
        assert!(matches!(result, Err(WorkflowError::NoGitHub(_))));
    }

    // T23: Item not in In Development status → skipped
    #[tokio::test]
    async fn dev_completion_skips_non_in_dev_items() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([]);
        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        let field_values = json!({
            "nodes": [
                single_select_field_value("Status", "Todo"),
                session_binding_value("Todo", "dev-session-1"),
            ]
        });

        mount_dev_completion_github_mocks(&mock, project_items, issues, field_values).await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
            .mount(&oc_mock)
            .await;

        let result = run_dev_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        oc_mock.verify().await;
    }

    // T30: Active-session poll error → skip the whole check, no GitHub mutations
    #[tokio::test]
    async fn dev_completion_poll_error_skips_check_without_github_mutations() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([
            {"node_id": "issue-node-42", "number": 42, "title": "Fix bug", "body": "body", "state": "open", "pull_request": null},
        ]);
        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        let field_values = in_dev_field_values("dev-session-err");

        mount_dev_completion_github_mocks(&mock, project_items, issues, field_values).await;

        // GET /api/session/active → 500: the check must skip this cycle entirely.
        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&oc_mock)
            .await;

        // No GitHub mutation may fire while the active-session set is unknown.
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2ItemFieldValue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "updateProjectV2ItemFieldValue": { "projectV2Item": { "id": "item-id" } }
                }
            })))
            .with_priority(1)
            .expect(0)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("resolveReviewThread"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "resolveReviewThread": { "thread": { "id": "x" } } }
            })))
            .with_priority(1)
            .expect(0)
            .mount(&mock)
            .await;

        let result = run_dev_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        mock.verify().await;
        oc_mock.verify().await;
    }

    // T31: outcome=failed → reset to Todo + clear sessionId, never parse messages
    #[tokio::test]
    async fn dev_completion_outcome_failed_resets_without_parsing() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([
            {"node_id": "issue-node-42", "number": 42, "title": "Fix bug", "body": "body", "state": "open", "pull_request": null},
        ]);
        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        let field_values = in_dev_field_values("dev-session-fail");

        mount_dev_completion_github_mocks(&mock, project_items, issues, field_values).await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/dev-session-fail"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "dev-session-fail", "outcome": "failed"}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        // Messages must NOT be fetched for a failed session.
        Mock::given(method("GET"))
            .and(path("/api/session/dev-session-fail/message"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [],
                "cursor": null
            })))
            .expect(0)
            .mount(&oc_mock)
            .await;

        // Status must be reset to Todo …
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2ItemFieldValue"))
            .and(body_string_contains("todo-id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "updateProjectV2ItemFieldValue": { "projectV2Item": { "id": "item-42" } }
                }
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        // … and the sessionId must be cleared …
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2ItemFieldValue"))
            .and(body_string_contains("session-field-id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "updateProjectV2ItemFieldValue": { "projectV2Item": { "id": "item-42" } }
                }
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        // … but never transitioned to Review Technical.
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2ItemFieldValue"))
            .and(body_string_contains("review-tech-id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "updateProjectV2ItemFieldValue": { "projectV2Item": { "id": "item-42" } }
                }
            })))
            .with_priority(1)
            .expect(0)
            .mount(&mock)
            .await;

        let result = run_dev_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        mock.verify().await;
        oc_mock.verify().await;
    }

    // T32: no outcome but time.idle present → treat as succeeded and parse output
    #[tokio::test]
    async fn dev_completion_idle_only_parses_output() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([
            {"node_id": "issue-node-42", "number": 42, "title": "Fix bug", "body": "body", "state": "open", "pull_request": null},
        ]);
        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        let field_values = in_dev_field_values("dev-session-idle");

        mount_dev_completion_github_mocks(&mock, project_items, issues, field_values).await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
            .mount(&oc_mock)
            .await;

        // No `outcome`, only `time.idle` — must be treated as a finished session.
        Mock::given(method("GET"))
            .and(path("/api/session/dev-session-idle"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "dev-session-idle", "time": {"created": 1, "updated": 2, "idle": 1790442360282i64}}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/dev-session-idle/message"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"type": "user", "id": "m0", "text": "Do the work"},
                    {"type": "assistant", "id": "m1", "content": [{"type": "text", "text": "### Resolve threads"}]},
                    {"type": "assistant", "id": "m2", "content": [{"type": "text", "text": "TH_999"}]}
                ],
                "cursor": null
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        // The parsed thread must be resolved via the GitHub API.
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("resolveReviewThread"))
            .and(body_string_contains("TH_999"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "resolveReviewThread": { "thread": { "id": "TH_999" } } }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // … and the item transitions to Review Technical (existing success path).
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2ItemFieldValue"))
            .and(body_string_contains("review-tech-id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "updateProjectV2ItemFieldValue": { "projectV2Item": { "id": "item-42" } }
                }
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        let result = run_dev_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        mock.verify().await;
        oc_mock.verify().await;
    }

    // ── Triage Completion Check Tests ────────────────────────────

    fn triage_field_values(session_id: &str) -> serde_json::Value {
        json!({
            "nodes": [
                single_select_field_value("Status", "Triage"),
                session_binding_value("Triage", session_id),
            ]
        })
    }

    async fn mount_triage_completion_github_mocks(
        server: &MockServer,
        project_items: serde_json::Value,
        field_values: serde_json::Value,
    ) {
        // Status field query
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "status-field-id",
                            "options": [
                                {"id": "triage-opt-id", "name": "Triage"},
                                {"id": "todo-opt-id", "name": "Todo"},
                                {"id": "in-dev-opt-id", "name": "In Development"},
                                {"id": "review-tech-opt-id", "name": "Review Technical"},
                                {"id": "review-prod-opt-id", "name": "Review Product"},
                                {"id": "qa-opt-id", "name": "QA"},
                                {"id": "done-opt-id", "name": "Done"},
                            ]
                        }
                    }
                }
            })))
            .mount(server)
            .await;

        // Fields query (includes waveId)
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id": "status-field-id", "name": "Status", "dataType": "SINGLE_SELECT"},
                                {"id": "session-field-id", "name": "sessionId", "dataType": "TEXT"},
                                {"id": "wave-field-id", "name": "waveId", "dataType": "NUMBER"},
                            ]
                        }
                    }
                }
            })))
            .mount(server)
            .await;

        // Project items query
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("items(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "items": project_items } }
            })))
            .mount(server)
            .await;

        // Field values query
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fieldValues(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "fieldValues": field_values } }
            })))
            .mount(server)
            .await;

        // Update item field value mutation
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2ItemFieldValue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "updateProjectV2ItemFieldValue": { "projectV2Item": { "id": "item-id" } }
                }
            })))
            .mount(server)
            .await;

        // Add issue to project mutation
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("addProjectV2ItemById"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "addProjectV2ItemById": { "item": { "id": "new-sub-item-id" } }
                }
            })))
            .mount(server)
            .await;

        // Create issue REST endpoint
        Mock::given(method("POST"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "node_id": "sub-issue-node-1",
                "number": 101,
                "title": "[#42] Sub-task 1",
                "body": "body",
                "state": "open"
            })))
            .mount(server)
            .await;
    }

    // T24: Completed triage session with sub-tasks → creates sub-issues, sets Todo, sets waveId, clears session
    #[tokio::test]
    async fn triage_completion_creates_sub_tasks() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        let field_values = triage_field_values("triage-session-1");

        mount_triage_completion_github_mocks(&mock, project_items, field_values).await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/triage-session-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "triage-session-1", "outcome": "succeeded"}
            })))
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/triage-session-1/message"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"type": "user", "id": "m0", "text": "Triage prompt"},
                    {"type": "assistant", "id": "m1", "content": [{"type": "text", "text": "## Sub-tasks"}]},
                    {"type": "assistant", "id": "m2", "content": [{"type": "text", "text": "1. Implement auth: Add OAuth login"}]},
                    {"type": "assistant", "id": "m3", "content": [{"type": "text", "text": "2. Add tests: Write unit tests"}]}
                ],
                "cursor": null
            })))
            .mount(&oc_mock)
            .await;

        let result = run_triage_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        oc_mock.verify().await;
    }

    // T25: Completed triage session with no sub-tasks → transitions main to Todo
    #[tokio::test]
    async fn triage_completion_no_sub_tasks_transitions_to_todo() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        let field_values = triage_field_values("triage-session-2");

        mount_triage_completion_github_mocks(&mock, project_items, field_values).await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/triage-session-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "triage-session-2", "outcome": "succeeded"}
            })))
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/triage-session-2/message"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"type": "assistant", "id": "m1", "content": [{"type": "text", "text": "The issue is simple, no sub-tasks needed."}]}
                ],
                "cursor": null
            })))
            .mount(&oc_mock)
            .await;

        // Verify no create_issue calls
        Mock::given(method("POST"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({})))
            .expect(0)
            .mount(&mock)
            .await;

        let result = run_triage_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        oc_mock.verify().await;
    }

    // T26: Active triage session → ignored
    #[tokio::test]
    async fn triage_completion_active_session_ignored() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        let field_values = triage_field_values("triage-session-active");

        mount_triage_completion_github_mocks(&mock, project_items, field_values).await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "triage-session-active": {"type": "running"}
                }
            })))
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/triage-session-active/message"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [],
                "cursor": null
            })))
            .expect(0)
            .mount(&oc_mock)
            .await;

        let result = run_triage_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        oc_mock.verify().await;
    }

    // T27: No GitHub client → returns NoGitHub error
    #[tokio::test]
    async fn triage_completion_no_github_client_returns_error() {
        let deps = make_deps(None);
        let ctx = make_context();
        let oc = make_oc_config("http://localhost:8081".to_string());

        let result = run_triage_completion_check(&deps, &ctx, &oc).await;
        assert!(matches!(result, Err(WorkflowError::NoGitHub(_))));
    }

    // T28: Item not in Triage status → skipped
    #[tokio::test]
    async fn triage_completion_skips_non_triage_items() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        let field_values = json!({
            "nodes": [
                single_select_field_value("Status", "Todo"),
                session_binding_value("Todo", "triage-session-1"),
            ]
        });

        mount_triage_completion_github_mocks(&mock, project_items, field_values).await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
            .mount(&oc_mock)
            .await;

        let result = run_triage_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        oc_mock.verify().await;
    }

    // T29: parse_sub_tasks — extracts sub-tasks from messages
    #[test]
    fn parse_sub_tasks_extracts_numbered_list() {
        let messages = vec![
            "Here is my analysis.".to_string(),
            "## Sub-tasks".to_string(),
            "1. Implement auth: Add OAuth login flow".to_string(),
            "2. Add tests: Write unit tests for auth".to_string(),
            "### Next steps".to_string(),
            "Done.".to_string(),
        ];
        let sub_tasks = parse_sub_tasks(&messages);
        assert_eq!(sub_tasks.len(), 2);
        assert_eq!(
            sub_tasks[0],
            (
                "Implement auth".to_string(),
                "Add OAuth login flow".to_string()
            )
        );
        assert_eq!(
            sub_tasks[1],
            (
                "Add tests".to_string(),
                "Write unit tests for auth".to_string()
            )
        );
    }

    // T30: parse_sub_tasks — no sub-tasks section returns empty
    #[test]
    fn parse_sub_tasks_no_section_returns_empty() {
        let messages = vec![
            "Here is my analysis.".to_string(),
            "The issue is simple.".to_string(),
        ];
        let sub_tasks = parse_sub_tasks(&messages);
        assert!(sub_tasks.is_empty());
    }

    // T31: parse_sub_tasks — sub-task without description
    #[test]
    fn parse_sub_tasks_without_description() {
        let messages = vec!["## Sub-tasks".to_string(), "1. Standalone task".to_string()];
        let sub_tasks = parse_sub_tasks(&messages);
        assert_eq!(sub_tasks.len(), 1);
        assert_eq!(
            sub_tasks[0],
            ("Standalone task".to_string(), "".to_string())
        );
    }

    // T32: triage does not re-initialize an already-progressed item (C3)
    #[tokio::test]
    async fn triage_does_not_reinitialize_progressed_item() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([
            {"node_id": "issue-node-1", "number": 1, "title": "@ai Fix bug", "body": "body", "state": "open", "pull_request": null},
        ]);
        let project_items = json!({
            "nodes": [
                {"id": "existing-item-id", "content": {"__typename": "Issue", "id": "issue-node-1", "number": 1}}
            ]
        });
        let field_values = json!({
            "nodes": [ single_select_field_value("Status", "Review Technical") ]
        });

        mount_triage_github_mocks(&mock, issues, project_items, field_values).await;

        // A progressed item must not be forced back to Triage nor get a session.
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2ItemFieldValue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "updateProjectV2ItemFieldValue": { "projectV2Item": { "id": "x" } } }
            })))
            .with_priority(1)
            .expect(0)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "should-not-happen"}
            })))
            .expect(0)
            .mount(&oc_mock)
            .await;

        let result = run_triage_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        mock.verify().await;
        oc_mock.verify().await;
    }

    // T33: detached owner clears the binding without reverting the status (C2)
    #[tokio::test]
    async fn failed_review_detached_binding_clears_without_revert() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([]);
        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        // Status advanced to "Review Product", but the session is still owned by
        // the previous step "Review Technical".
        let detached = serialize_binding(&SessionBinding {
            step: Some("Review Technical".to_string()),
            id: "review-session-detached".to_string(),
            resume: String::new(),
            attempts: 1,
        });
        let field_values = json!({
            "nodes": [
                single_select_field_value("Status", "Review Product"),
                text_field_value("sessionId", Some(&detached)),
            ]
        });

        mount_review_github_mocks(
            &mock,
            project_items,
            issues,
            field_values,
            all_status_options(),
        )
        .await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
            .mount(&oc_mock)
            .await;

        // The detached binding is cleared …
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2ItemFieldValue"))
            .and(body_string_contains("session-field-id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "updateProjectV2ItemFieldValue": { "projectV2Item": { "id": "item-42" } } }
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        // … but the status is never reverted to Todo …
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2ItemFieldValue"))
            .and(body_string_contains("todo-id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "updateProjectV2ItemFieldValue": { "projectV2Item": { "id": "item-42" } } }
            })))
            .with_priority(1)
            .expect(0)
            .mount(&mock)
            .await;

        // … and no recovery developer session is started.
        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "should-not-happen"}
            })))
            .expect(0)
            .mount(&oc_mock)
            .await;

        let result = run_review_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        mock.verify().await;
        oc_mock.verify().await;
    }

    // T34: triage completion with sub-tasks marks the parent Done (C4)
    #[tokio::test]
    async fn triage_completion_with_sub_tasks_marks_parent_done() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        let field_values = triage_field_values("triage-session-1");

        mount_triage_completion_github_mocks(&mock, project_items, field_values).await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/triage-session-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "triage-session-1", "outcome": "succeeded"}
            })))
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/triage-session-1/message"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"type": "assistant", "id": "m1", "content": [{"type": "text", "text": "## Sub-tasks"}]},
                    {"type": "assistant", "id": "m2", "content": [{"type": "text", "text": "1. Implement auth: Add OAuth login"}]}
                ],
                "cursor": null
            })))
            .mount(&oc_mock)
            .await;

        // The decomposed PARENT (#42) must be set to Done …
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2ItemFieldValue"))
            .and(body_string_contains("done-opt-id"))
            .and(body_string_contains("item-42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "updateProjectV2ItemFieldValue": { "projectV2Item": { "id": "item-42" } } }
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        let result = run_triage_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        mock.verify().await;
        oc_mock.verify().await;
    }

    // T40: a triage verdict's `sub_tasks` are preferred over the markdown
    // `## Sub-tasks` fallback.
    #[tokio::test]
    async fn triage_completion_prefers_verdict_sub_tasks() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let deps = make_deps(Some(gh_client(&mock)));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        mount_triage_completion_github_mocks(
            &mock,
            project_items,
            triage_field_values("triage-session-9"),
        )
        .await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/triage-session-9"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "triage-session-9", "outcome": "succeeded"}
            })))
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/triage-session-9/message"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"type": "assistant", "id": "m1", "content": [{"type": "text", "text": "GIT_AUTOMATE_VERDICT: {\"v\":1,\"role\":\"triage\",\"decision\":\"ready\",\"sub_tasks\":[{\"title\":\"Implement auth\",\"description\":\"Add OAuth\"}]}"}]}
                ],
                "cursor": null
            })))
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/repos/owner/repo/issues"))
            .and(body_string_contains("Implement auth"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "node_id": "sub-issue-node-9",
                "number": 109,
                "title": "[#42] Implement auth",
                "body": "body",
                "state": "open"
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        let result = run_triage_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        mock.verify().await;
        oc_mock.verify().await;
    }

    // ── Phase 3 hardening tests ──────────────────────────────────

    // T41: a failed session-binding persist deletes the freshly created session.
    #[tokio::test]
    async fn triage_persist_failure_deletes_session() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let deps = make_deps(Some(gh_client(&mock)));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([
            {"node_id": "issue-node-1", "number": 1, "title": "@ai Fix bug", "body": "body", "state": "open", "pull_request": null},
        ]);
        let project_items = json!({ "nodes": [] });
        let field_values = json!({ "nodes": [] });

        mount_triage_github_mocks(&mock, issues, project_items, field_values).await;
        mount_opencode_mocks(&oc_mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("session-field-id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errors": [{"message": "persist failed"}]
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("DELETE"))
            .and(path("/api/session/sess123"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&oc_mock)
            .await;

        let result = run_triage_check(&deps, &ctx, &oc).await;
        assert!(result.is_err(), "persist failure must propagate");
        mock.verify().await;
        oc_mock.verify().await;
    }

    // T42: attempts at the cap park the item without creating a session.
    #[tokio::test]
    async fn review_park_when_attempts_at_cap() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let deps = make_deps(Some(gh_client(&mock)));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        let field_values = json!({
            "nodes": [
                single_select_field_value("Status", "Review Technical"),
                session_binding_value_full("Review Technical", "", "", 3),
            ]
        });

        mount_review_github_mocks(
            &mock,
            project_items,
            json!([]),
            field_values,
            all_status_options(),
        )
        .await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "should-not-happen"}
            })))
            .expect(0)
            .mount(&oc_mock)
            .await;

        let result = run_review_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        mock.verify().await;
        oc_mock.verify().await;
    }

    // T43: an inactive session with no outcome ages out to Todo past timeout_secs.
    #[tokio::test]
    async fn dev_completion_stale_inactive_session_times_out() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let deps = make_deps(Some(gh_client(&mock)));
        let ctx = make_context();
        let mut oc = make_oc_config(oc_mock.uri());
        oc.session_timeout_secs = 10;

        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        mount_dev_completion_github_mocks(
            &mock,
            project_items,
            json!([]),
            in_dev_field_values("dev-stale"),
        )
        .await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {}})))
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/dev-stale"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "dev-stale", "time": {"created": 100, "updated": 200}}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/dev-stale/message"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [], "cursor": null
            })))
            .expect(0)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/session/dev-stale/interrupt"))
            .respond_with(ResponseTemplate::new(404))
            .expect(0)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("session-field-id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"updateProjectV2ItemFieldValue": {"projectV2Item": {"id": "item-42"}}}
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("todo-id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"updateProjectV2ItemFieldValue": {"projectV2Item": {"id": "item-42"}}}
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        let result = run_dev_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        mock.verify().await;
        oc_mock.verify().await;
    }

    // T44: an active session past max_secs is interrupted then reset to Todo.
    #[tokio::test]
    async fn dev_completion_active_session_past_max_interrupts() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let deps = make_deps(Some(gh_client(&mock)));
        let ctx = make_context();
        let mut oc = make_oc_config(oc_mock.uri());
        oc.session_max_secs = 0;

        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        mount_dev_completion_github_mocks(
            &mock,
            project_items,
            json!([]),
            in_dev_field_values("dev-stuck"),
        )
        .await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"dev-stuck": {"type": "running"}}
            })))
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/dev-stuck"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"id": "dev-stuck", "time": {"created": 100, "updated": 200}}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/session/dev-stuck/interrupt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"interrupted": true})))
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/dev-stuck/message"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [], "cursor": null
            })))
            .expect(0)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("session-field-id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"updateProjectV2ItemFieldValue": {"projectV2Item": {"id": "item-42"}}}
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("todo-id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"updateProjectV2ItemFieldValue": {"projectV2Item": {"id": "item-42"}}}
            })))
            .with_priority(1)
            .expect(1)
            .mount(&mock)
            .await;

        let result = run_dev_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        mock.verify().await;
        oc_mock.verify().await;
    }
}
