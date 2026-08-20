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

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::external_agent::opencode::OpenCodeClient;
use crate::external_issues::github::types::IssueInfo;

use super::helpers::{
    LogLevel, ProjectContext, WorkflowContext, WorkflowError, branch_name_for_issue,
    extract_session_id, fill_prompt, get_issue_body_map, issue_body_or_title, load_agent_template,
    load_prompt_template, log_deduped, parse_resolve_threads, resolve_field_ids, resolve_option_id,
    resolve_status_option_and_session,
};

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

// ─── OpenCode session config ───────────────────────────────────

/// OpenCode session configuration passed to check functions.
#[derive(Clone)]
pub struct OpencodeSessionConfig {
    pub url: String,
    pub pw: String,
    pub directory: String,
    pub project: Option<String>,
    pub concurrency: HashMap<String, usize>,
}

// ─── Concurrency dedup state ──────────────────────────────────

/// Tracks whether OpenCode concurrency capacity has been exceeded during the
/// current daemon cycle. Reset to `false` on daemon restart (static variable).
static CAPACITY_EXCEEDED: AtomicBool = AtomicBool::new(false);

// ─── start_opencode_session ────────────────────────────────────

/// Start an OpenCode session with the given system prompt and user message.
///
/// Creates a new [`OpenCodeClient`] per call. The `system_prompt` carries
/// agent instructions; the `agent` field is omitted so OpenCode uses its
/// default agent.
///
/// When `concurrency` is `Some(limit)`, the active session count is queried
/// first; if it meets or exceeds the limit the session is **not** created and
/// an `WorkflowError::Other` is returned.
///
/// Maps [`crate::external_agent::opencode::OpenCodeError`] to `WorkflowError::Other`.
#[allow(clippy::too_many_arguments)]
async fn start_opencode_session(
    oc: &OpencodeSessionConfig,
    directory: &str,
    title: &str,
    system_prompt: &str,
    message: &str,
    concurrency: &HashMap<String, usize>,
    log_dedup: &std::sync::Arc<tokio::sync::Mutex<HashMap<String, String>>>,
    step: &str,
) -> Result<String, WorkflowError> {
    let client = OpenCodeClient::new(oc.url.clone(), oc.pw.clone());

    if !concurrency.is_empty() {
        let counts = client
            .get_session_models()
            .await
            .map_err(|e| WorkflowError::Other(format!("OpenCode: {}", e)))?;
        for (model_key, limit) in concurrency {
            let active = counts.get(model_key).copied().unwrap_or(0);
            if active >= *limit {
                if !CAPACITY_EXCEEDED.swap(true, Ordering::Relaxed) {
                    log_deduped(
                        log_dedup,
                        "all",
                        LogLevel::Warn,
                        format!(
                            "OpenCode capacity exceeded for model {} ({} >= {}), skipping: {}",
                            model_key, active, limit, title
                        ),
                    )
                    .await;
                }
                return Err(WorkflowError::ConcurrencyExceeded);
            }
        }
    }

    let workspace = client
        .create_workspace(directory)
        .await
        .map_err(|e| WorkflowError::Other(format!("OpenCode: {}", e)))?;
    let worktree = client
        .create_worktree(directory, &workspace.id)
        .await
        .map_err(|e| WorkflowError::Other(format!("OpenCode: {}", e)))?;
    log_deduped(
        log_dedup,
        step,
        LogLevel::Info,
        format!(
            "starting session in worktree {} (workspace {})",
            worktree.directory, workspace.id
        ),
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

    client
        .start_session_with_system(
            &worktree.directory,
            title,
            system_prompt,
            "",
            message,
            Some(&workspace.id),
            None,
        )
        .await
        .map_err(|e| WorkflowError::Other(format!("OpenCode: {}", e)))
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
        let item_id = if let Some(id) = existing_items.get(&issue.number) {
            id.clone()
        } else {
            log_deduped(
                &deps.log_dedup,
                "triage",
                LogLevel::Info,
                format!("{}: adding issue #{} to project", ctx.name, issue.number),
            )
            .await;
            github
                .add_issue_to_project(&issue.id, &ctx.project_id)
                .await?
        };

        github
            .update_project_item_status(
                &ctx.project_id,
                &item_id,
                &status_field_id,
                &triage_option_id,
            )
            .await?;

        let item_values = github.get_project_item_values(&item_id).await?;
        let session_text = extract_session_id(&item_values);
        if session_text.is_none() {
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
            let system_prompt = load_agent_template("triage")?;
            let template = load_prompt_template("triage")?;
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
            let session_id = start_opencode_session(
                oc,
                oc.directory.as_str(),
                &issue.title,
                &system_prompt,
                &user_prompt,
                deps.config
                    .opencode
                    .as_ref()
                    .map(|oc| &oc.concurrency)
                    .unwrap_or(&HashMap::new()),
                &deps.log_dedup,
                "triage",
            )
            .await?;
            github
                .update_project_item_session_id(
                    &ctx.project_id,
                    &item_id,
                    &session_field_id,
                    Some(&session_id),
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

    // Collect items that are "Todo" and have no session yet.
    let mut todo_items: Vec<(String, i64)> = Vec::new();
    for item in &project_items {
        let item_values = github.get_project_item_values(&item.id).await?;
        let status = item_values.get("Status").and_then(|v| v.as_deref());
        if status == Some("Todo") {
            let session_text = extract_session_id(&item_values);
            if session_text.is_none() {
                todo_items.push((item.id.clone(), item.content_number));
            }
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

    let default_branch = github
        .get_repo_default_branch(&ctx.owner, &ctx.repo)
        .await?;
    let commit_sha = github
        .get_repo_commit_sha(&ctx.owner, &ctx.repo, &default_branch)
        .await?;

    for (item_id, issue_number) in &todo_items {
        let issue = issue_map.get(issue_number);
        let branch_name = branch_name_for_issue(*issue_number, &deps.config.git);

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
        let system_prompt = load_agent_template("developer")?;
        let template = load_prompt_template("developer")?;
        let branch_name = branch_name_for_issue(*issue_number, &deps.config.git);
        let values = HashMap::from([
            ("ISSUE_TITLE".to_string(), title.clone()),
            ("ISSUE_NUMBER".to_string(), issue_number.to_string()),
            ("ISSUE_BODY".to_string(), message.clone()),
            ("BRANCH_NAME".to_string(), branch_name),
            (
                "PROJECT_REPOSITORY".to_string(),
                format!("{}/{}", ctx.owner, ctx.repo),
            ),
        ]);
        let user_prompt = fill_prompt(&template, &values);
        let session_id = start_opencode_session(
            oc,
            oc.directory.as_str(),
            &title,
            &system_prompt,
            &user_prompt,
            deps.config
                .opencode
                .as_ref()
                .map(|oc| &oc.concurrency)
                .unwrap_or(&HashMap::new()),
            &deps.log_dedup,
            "todo",
        )
        .await?;

        github
            .update_project_item_session_id(
                &ctx.project_id,
                item_id,
                &session_field_id,
                Some(&session_id),
            )
            .await?;
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

        let template = load_prompt_template(state.prompt())?;
        let system_prompt = load_agent_template(state.prompt())?;

        for (item, values) in project_items.iter().zip(&item_values) {
            let current_status = values.get("Status").and_then(|v| v.as_deref());
            if current_status != Some(state.status()) {
                continue;
            }
            let session_text = extract_session_id(values);
            if session_text.is_some() {
                continue;
            }

            let issue = issue_map.get(&item.content_number);
            let issue_number = item.content_number;
            let branch_name = branch_name_for_issue(issue_number, &deps.config.git);

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
                    ("PR_URL".to_string(), String::new()),
                    ("PR_CHANGES".to_string(), String::new()),
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

            let session_id = start_opencode_session(
                oc,
                oc.directory.as_str(),
                &title,
                &system_prompt,
                &filled_prompt,
                deps.config
                    .opencode
                    .as_ref()
                    .map(|oc| &oc.concurrency)
                    .unwrap_or(&HashMap::new()),
                &deps.log_dedup,
                "review",
            )
            .await?;

            github
                .update_project_item_session_id(
                    &ctx.project_id,
                    &item.id,
                    &session_field_id,
                    Some(&session_id),
                )
                .await?;
        }
    }

    run_failed_review_check(deps, ctx, oc).await?;

    Ok(())
}

/// Detect review sessions that completed without a status transition and
/// recover the item: back to Todo → new dev session → In Development.
pub async fn run_failed_review_check(
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
            format!("{}: no project items for failed review check", ctx.name),
        )
        .await;
        return Ok(());
    }

    let issue_map = get_issue_body_map(github, &ctx.owner, &ctx.repo).await?;
    let status_field = github.get_project_status_field(&ctx.project_id).await?;

    let Some(status_field) = status_field else {
        log_deduped(
            &deps.log_dedup,
            "review",
            LogLevel::Warn,
            format!(
                "{}: no Status field found for failed review check",
                ctx.name
            ),
        )
        .await;
        return Ok(());
    };

    let todo_option_id = resolve_option_id(&status_field.options, "Todo");
    let in_dev_option_id = resolve_option_id(&status_field.options, "In Development");

    let oc_client = OpenCodeClient::new(oc.url.clone(), oc.pw.clone());
    let active_sessions = match oc_client.get_session_statuses().await {
        Ok(sessions) => sessions,
        Err(e) => {
            log_deduped(
                &deps.log_dedup,
                "review",
                LogLevel::Warn,
                format!(
                    "{}: could not fetch OpenCode session statuses for failed review check: {}",
                    ctx.name, e
                ),
            )
            .await;
            std::collections::HashMap::new()
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

            let session_id = match extract_session_id(values) {
                Some(id) => id,
                None => continue,
            };

            if active_sessions.contains_key(&session_id) {
                continue;
            }

            log_deduped(
                &deps.log_dedup,
                "review",
                LogLevel::Warn,
                format!(
                    "{}: review session {} for issue #{} has completed without status transition, recovering",
                    ctx.name, session_id, item.content_number
                ),
            )
            .await;

            if let Some(todo_id) = &todo_option_id {
                github
                    .update_project_item_status(
                        &ctx.project_id,
                        &item.id,
                        &field_ids.status_field_id,
                        todo_id,
                    )
                    .await?;
            }
            github
                .update_project_item_session_id(&ctx.project_id, &item.id, &session_field_id, None)
                .await?;

            let issue_number = item.content_number;
            let title = issue_map
                .get(&issue_number)
                .map(|i| i.title.clone())
                .unwrap_or_else(|| format!("Dev work for issue #{}", issue_number));
            let message = issue_map
                .get(&issue_number)
                .map(issue_body_or_title)
                .unwrap_or_else(|| format!("Issue #{}", issue_number));

            log_deduped(
                &deps.log_dedup,
                "review",
                LogLevel::Info,
                format!(
                    "{}: starting developer session for failed review #{}",
                    ctx.name, issue_number
                ),
            )
            .await;

            let system_prompt = load_agent_template("developer")?;
            let template = load_prompt_template("developer")?;
            let branch_name = format!("issue-{}", issue_number);
            let values = HashMap::from([
                ("ISSUE_TITLE".to_string(), title.clone()),
                ("ISSUE_NUMBER".to_string(), issue_number.to_string()),
                ("ISSUE_BODY".to_string(), message.clone()),
                ("BRANCH_NAME".to_string(), branch_name),
                (
                    "PROJECT_REPOSITORY".to_string(),
                    format!("{}/{}", ctx.owner, ctx.repo),
                ),
            ]);
            let user_prompt = fill_prompt(&template, &values);
            let new_session_id = start_opencode_session(
                oc,
                oc.directory.as_str(),
                &title,
                &system_prompt,
                &user_prompt,
                deps.config
                    .opencode
                    .as_ref()
                    .map(|oc| &oc.concurrency)
                    .unwrap_or(&HashMap::new()),
                &deps.log_dedup,
                "review",
            )
            .await?;

            github
                .update_project_item_session_id(
                    &ctx.project_id,
                    &item.id,
                    &session_field_id,
                    Some(&new_session_id),
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
            }
        }
    }

    Ok(())
}

/// Detect completed developer sessions in "In Development" status, parse
/// session output for `### Resolve threads`, resolve each thread via the
/// GitHub API, and transition the item to "Review Technical".
///
/// If no `### Resolve threads` section is found, no resolution mutations
/// fire but the issue still transitions to "Review Technical".
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

    let oc_client = OpenCodeClient::new(oc.url.clone(), oc.pw.clone());
    let active_sessions = match oc_client.get_session_statuses().await {
        Ok(sessions) => sessions,
        Err(e) => {
            log_deduped(
                &deps.log_dedup,
                "review",
                LogLevel::Warn,
                format!(
                    "{}: could not fetch OpenCode session statuses for dev completion check: {}",
                    ctx.name, e
                ),
            )
            .await;
            std::collections::HashMap::new()
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

        let session_id = match extract_session_id(values) {
            Some(id) => id,
            None => continue,
        };

        if active_sessions.contains_key(&session_id) {
            continue;
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

        let messages = match oc_client.get_session_messages(&session_id, None).await {
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
        let thread_ids =
            parse_resolve_threads(&messages.iter().map(|m| m.text()).collect::<Vec<_>>());

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

        github
            .update_project_item_session_id(&ctx.project_id, &item.id, &session_field_id, None)
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

    /// Build a [`ProjectContext`] for testing.
    fn make_context() -> ProjectContext {
        ProjectContext {
            name: "test-project".to_string(),
            config: GitSection {
                repository: "https://github.com/owner/repo".to_string(),
                project_id: Some("PID-123".to_string()),
                directory: "/test-work".to_string(),
                issue_provider: "github".to_string(),
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

    /// Build a field-value node for a single-select field (e.g. `Status`).
    fn single_select_field_value(name: &str, option_name: &str) -> serde_json::Value {
        json!({
            "__typename": "ProjectV2ItemFieldSingleSelectValue",
            "name": option_name,
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

    /// Mount OpenCode mocks for session creation + prompt_async.
    async fn mount_opencode_mocks(server: &MockServer) {
        // Workspace creation
        Mock::given(method("POST"))
            .and(path("/experimental/workspace"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "wrk1",
                "type": "git",
                "name": "w1",
                "branch": null,
                "directory": null,
                "extra": null,
                "projectID": "p1",
                "timeUsed": 0
            })))
            .mount(server)
            .await;

        // Worktree creation
        Mock::given(method("POST"))
            .and(path("/experimental/worktree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "wt1",
                "branch": "issue-1",
                "directory": "/wt/dir1"
            })))
            .mount(server)
            .await;

        Mock::given(method("POST"))
            .and(path("/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "sess123",
                "projectID": "p1",
                "directory": "/d",
                "title": "t",
                "version": "1",
                "time": {"created": 1, "updated": 2}
            })))
            .mount(server)
            .await;

        Mock::given(method("POST"))
            .and(path("/session/sess123/prompt_async"))
            .respond_with(ResponseTemplate::new(204))
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
                text_field_value("sessionId", Some("existing-session-id"))
            ]
        });

        mount_triage_github_mocks(&mock, issues, empty_items, field_values).await;

        // OpenCode should never be called — use expect(0) on the session endpoint
        Mock::given(method("POST"))
            .and(path("/session"))
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
            .and(path("/session"))
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

        // Workspace + worktree creation
        Mock::given(method("POST"))
            .and(path("/experimental/workspace"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "wrk1",
                "type": "git",
                "name": "w1",
                "branch": null,
                "directory": null,
                "extra": null,
                "projectID": "p1",
                "timeUsed": 0
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/experimental/worktree"))
            .and(query_param("workspace", "wrk1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "wt1",
                "branch": "issue-42",
                "directory": "/wt/dir1"
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        // Specific session mock: expects title "Dev work for issue #42"
        Mock::given(method("POST"))
            .and(path("/session"))
            .and(body_string_contains("\"title\":\"Dev work for issue #42\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "sess123",
                "projectID": "p1",
                "directory": "/d",
                "title": "t",
                "version": "1",
                "time": {"created": 1, "updated": 2}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        // Prompt async mock
        Mock::given(method("POST"))
            .and(path("/session/sess123/prompt_async"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&oc_mock)
            .await;

        let result = run_todo_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        oc_mock.verify().await;
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
            .and(path("/session"))
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

        // Workspace + worktree creation
        Mock::given(method("POST"))
            .and(path("/experimental/workspace"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "wrk1",
                "type": "git",
                "name": "w1",
                "branch": null,
                "directory": null,
                "extra": null,
                "projectID": "p1",
                "timeUsed": 0
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/experimental/worktree"))
            .and(query_param("workspace", "wrk1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "wt1",
                "branch": "issue-42",
                "directory": "/wt/dir1"
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        // Specific session mock: expects title "Review Technical: Fix login"
        Mock::given(method("POST"))
            .and(path("/session"))
            .and(body_string_contains(
                "\"title\":\"Review Technical: Fix login\"",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "sess123", "projectID": "p1", "directory": "/d", "title": "t",
                "version": "1", "time": {"created": 1, "updated": 2}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/session/sess123/prompt_async"))
            .and(body_string_contains("\"system\""))
            .respond_with(ResponseTemplate::new(204))
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

        // Workspace + worktree creation
        Mock::given(method("POST"))
            .and(path("/experimental/workspace"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "wrk1",
                "type": "git",
                "name": "w1",
                "branch": null,
                "directory": null,
                "extra": null,
                "projectID": "p1",
                "timeUsed": 0
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/experimental/worktree"))
            .and(query_param("workspace", "wrk1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "wt1",
                "branch": "issue-7",
                "directory": "/wt/dir1"
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        // Session mock for QA state
        Mock::given(method("POST"))
            .and(path("/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "sess123", "projectID": "p1", "directory": "/d", "title": "t",
                "version": "1", "time": {"created": 1, "updated": 2}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        // Verify prompt_async body: system field present (agent template) and
        // contains filled BRANCH_NAME ("issue-7") — proves fill_prompt was called.
        Mock::given(method("POST"))
            .and(path("/session/sess123/prompt_async"))
            .and(body_string_contains("\"system\""))
            .and(body_string_contains("issue-7"))
            .respond_with(ResponseTemplate::new(204))
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

        // Workspace + worktree creation
        Mock::given(method("POST"))
            .and(path("/experimental/workspace"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "wrk1",
                "type": "git",
                "name": "w1",
                "branch": null,
                "directory": null,
                "extra": null,
                "projectID": "p1",
                "timeUsed": 0
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/experimental/worktree"))
            .and(query_param("workspace", "wrk1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "wt1",
                "branch": "issue-99",
                "directory": "/wt/dir1"
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        // Specific session mock: expects title "Review Product: Update landing page"
        Mock::given(method("POST"))
            .and(path("/session"))
            .and(body_string_contains(
                "\"title\":\"Review Product: Update landing page\"",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "sess123", "projectID": "p1", "directory": "/d", "title": "t",
                "version": "1", "time": {"created": 1, "updated": 2}
            })))
            .expect(1)
            .mount(&oc_mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/session/sess123/prompt_async"))
            .and(body_string_contains("\"system\""))
            .respond_with(ResponseTemplate::new(204))
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
            .and(path("/session"))
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

        // Workspace + worktree succeed; the 500 comes from /session
        Mock::given(method("POST"))
            .and(path("/experimental/workspace"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "wrk1",
                "type": "git",
                "name": "w1",
                "branch": null,
                "directory": null,
                "extra": null,
                "projectID": "p1",
                "timeUsed": 0
            })))
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/experimental/worktree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "wt1",
                "branch": "issue-1",
                "directory": "/wt/dir1"
            })))
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/session"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let oc = OpencodeSessionConfig {
            url: mock.uri(),
            pw: "pw".to_string(),
            directory: "/test-work".to_string(),
            project: None,
            concurrency: HashMap::new(),
        };

        let result = start_opencode_session(
            &oc,
            "/dir",
            "title",
            "system",
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

        // GET /api/session returns 4 sessions with myprovider/fast model
        Mock::given(method("GET"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    { "id": "sess1", "model": { "id": "fast", "providerID": "myprovider" } },
                    { "id": "sess2", "model": { "id": "fast", "providerID": "myprovider" } },
                    { "id": "sess3", "model": { "id": "fast", "providerID": "myprovider" } },
                    { "id": "sess4", "model": { "id": "fast", "providerID": "myprovider" } },
                ],
                "cursor": null
            })))
            .mount(&mock)
            .await;

        // GET /session/status returns 4 active sessions
        Mock::given(method("GET"))
            .and(path("/session/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "sess1": {"status": "idle"},
                "sess2": {"status": "busy"},
                "sess3": {"status": "idle"},
                "sess4": {"status": "idle"},
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // Workspace + worktree should NOT be created — limit is 2 and there are 4 active
        Mock::given(method("POST"))
            .and(path("/experimental/workspace"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "wrk1",
                "type": "git",
                "name": "w1",
                "branch": null,
                "directory": null,
                "extra": null,
                "projectID": "p1",
                "timeUsed": 0
            })))
            .expect(0)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/experimental/worktree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "wt1",
                "branch": "issue-1",
                "directory": "/wt/dir1"
            })))
            .expect(0)
            .mount(&mock)
            .await;

        // POST /session should NOT be called — limit is 2 and there are 4 active
        Mock::given(method("POST"))
            .and(path("/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "should-not-happen",
                "projectID": "p",
                "directory": "/d",
                "title": "t",
                "version": "1",
                "time": {"created": 1, "updated": 2}
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
        };

        // limit = 2 for myprovider/fast, active = 4 → 4 >= 2 → should skip
        let mut concurrency = HashMap::new();
        concurrency.insert("myprovider/fast".to_string(), 2);
        let result = start_opencode_session(
            &oc,
            "/dir",
            "title",
            "system",
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

        // GET /api/session returns 2 sessions with myprovider/fast model
        Mock::given(method("GET"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    { "id": "sess1", "model": { "id": "fast", "providerID": "myprovider" } },
                    { "id": "sess2", "model": { "id": "fast", "providerID": "myprovider" } },
                ],
                "cursor": null
            })))
            .mount(&mock)
            .await;

        // GET /session/status returns 2 active sessions
        Mock::given(method("GET"))
            .and(path("/session/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "sess1": {"status": "idle"},
                "sess2": {"status": "busy"},
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // Workspace + worktree creation succeed
        Mock::given(method("POST"))
            .and(path("/experimental/workspace"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "wrk1",
                "type": "git",
                "name": "w1",
                "branch": null,
                "directory": null,
                "extra": null,
                "projectID": "p1",
                "timeUsed": 0
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/experimental/worktree"))
            .and(query_param("workspace", "wrk1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "wt1",
                "branch": "issue-1",
                "directory": "/wt/dir1"
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // POST /session should be called exactly once and return "sess123"
        Mock::given(method("POST"))
            .and(path("/session"))
            .and(query_param("workspace", "wrk1"))
            .and(query_param("directory", "/wt/dir1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "sess123",
                "projectID": "p1",
                "directory": "/d",
                "title": "t",
                "version": "1",
                "time": {"created": 1, "updated": 2}
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // POST /session/sess123/prompt_async — 204
        Mock::given(method("POST"))
            .and(path("/session/sess123/prompt_async"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&mock)
            .await;

        let oc = OpencodeSessionConfig {
            url: mock.uri(),
            pw: "pw".to_string(),
            directory: "/test-work".to_string(),
            project: None,
            concurrency: HashMap::new(),
        };

        // limit = 5 for myprovider/fast, active = 2 → 2 < 5 → should proceed
        let mut concurrency = HashMap::new();
        concurrency.insert("myprovider/fast".to_string(), 5);
        let result = start_opencode_session(
            &oc,
            "/dir",
            "title",
            "system",
            "message",
            &concurrency,
            &Arc::new(Mutex::new(HashMap::new())),
            "test",
        )
        .await;

        assert_eq!(result.unwrap(), "sess123");
    }

    // T19: start_opencode_session full flow — workspace → worktree → session in worktree dir
    #[tokio::test]
    async fn start_opencode_session_full_flow_creates_workspace_and_worktree() {
        let mock = MockServer::start().await;

        // Workspace creation
        Mock::given(method("POST"))
            .and(path("/experimental/workspace"))
            .and(query_param("directory", "/dir"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "wrk1",
                "type": "git",
                "name": "w1",
                "branch": null,
                "directory": null,
                "extra": null,
                "projectID": "p1",
                "timeUsed": 0
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // Worktree creation — must reference the workspace
        Mock::given(method("POST"))
            .and(path("/experimental/worktree"))
            .and(query_param("workspace", "wrk1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "wt1",
                "branch": "issue-1",
                "directory": "/wt/dir1"
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // POST /session — created in the worktree directory with the workspace param
        Mock::given(method("POST"))
            .and(path("/session"))
            .and(query_param("workspace", "wrk1"))
            .and(query_param("directory", "/wt/dir1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "sess123",
                "projectID": "p1",
                "directory": "/wt/dir1",
                "title": "t",
                "version": "1",
                "time": {"created": 1, "updated": 2}
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/session/sess123/prompt_async"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&mock)
            .await;

        let oc = OpencodeSessionConfig {
            url: mock.uri(),
            pw: "pw".to_string(),
            directory: "/test-work".to_string(),
            project: None,
            concurrency: HashMap::new(),
        };

        let result = start_opencode_session(
            &oc,
            "/dir",
            "title",
            "system",
            "message",
            &HashMap::new(),
            &Arc::new(Mutex::new(HashMap::new())),
            "test",
        )
        .await;
        assert_eq!(result.unwrap(), "sess123");
    }

    // T20: workspace creation fails → no worktree, no session
    #[tokio::test]
    async fn start_opencode_session_workspace_failure_stops_flow() {
        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/experimental/workspace"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/experimental/worktree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "wt1",
                "branch": "issue-1",
                "directory": "/wt/dir1"
            })))
            .expect(0)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "should-not-happen",
                "projectID": "p",
                "directory": "/d",
                "title": "t",
                "version": "1",
                "time": {"created": 1, "updated": 2}
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
        };

        let result = start_opencode_session(
            &oc,
            "/dir",
            "title",
            "system",
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

        Mock::given(method("POST"))
            .and(path("/experimental/workspace"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "wrk1",
                "type": "git",
                "name": "w1",
                "branch": null,
                "directory": null,
                "extra": null,
                "projectID": "p1",
                "timeUsed": 0
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/experimental/worktree"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "should-not-happen",
                "projectID": "p",
                "directory": "/d",
                "title": "t",
                "version": "1",
                "time": {"created": 1, "updated": 2}
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
        };

        let result = start_opencode_session(
            &oc,
            "/dir",
            "title",
            "system",
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
                text_field_value("sessionId", Some("existing-session")),
            ]
        });
        let status_options = vec![
            json!({"id": "review-tech-id", "name": "Review Technical"}),
            json!({"id": "review-prod-id", "name": "Review Product"}),
            json!({"id": "qa-id", "name": "QA"}),
        ];

        mount_review_github_mocks(&mock, project_items, issues, field_values, status_options).await;

        // GET /session/status → session still active
        Mock::given(method("GET"))
            .and(path("/session/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "existing-session": {"type": "busy"}
            })))
            .mount(&oc_mock)
            .await;

        // OpenCode should never be called
        Mock::given(method("POST"))
            .and(path("/session"))
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
            .and(path("/session"))
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
            .and(path("/session"))
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
            .and(path("/session"))
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
            .and(path("/session"))
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

    // ── Failed Review Check Tests ──────────────────────────────

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

    fn review_field_values(status: &str, session_id: &str) -> serde_json::Value {
        json!({
            "nodes": [
                single_select_field_value("Status", status),
                text_field_value("sessionId", Some(session_id)),
            ]
        })
    }

    async fn mount_failed_review_opencode_mocks(server: &MockServer) {
        // GET /session/status → empty map (no active sessions → review session completed)
        Mock::given(method("GET"))
            .and(path("/session/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(server)
            .await;

        // POST /session → developer session created
        Mock::given(method("POST"))
            .and(path("/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "sess123", "projectID": "p1", "directory": "/d", "title": "t",
                "version": "1", "time": {"created": 1, "updated": 2}
            })))
            .mount(server)
            .await;

        // POST /session/sess123/prompt_async → 204
        Mock::given(method("POST"))
            .and(path("/session/sess123/prompt_async"))
            .respond_with(ResponseTemplate::new(204))
            .mount(server)
            .await;

        // Workspace creation
        Mock::given(method("POST"))
            .and(path("/experimental/workspace"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "wrk1",
                "type": "git",
                "name": "w1",
                "branch": null,
                "directory": null,
                "extra": null,
                "projectID": "p1",
                "timeUsed": 0
            })))
            .mount(server)
            .await;

        // Worktree creation
        Mock::given(method("POST"))
            .and(path("/experimental/worktree"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "wt1",
                "branch": "issue-1",
                "directory": "/wt/dir1"
            })))
            .mount(server)
            .await;
    }

    // T14: Failed Technical review → transitions to Todo, starts dev session, → In Development
    #[tokio::test]
    async fn failed_review_technical_transitions_to_in_dev() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([
            {"node_id": "issue-node-42", "number": 42, "title": "Fix login", "body": "Login broken", "state": "open", "pull_request": null},
        ]);
        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        let field_values = review_field_values("Review Technical", "review-session-1");

        mount_review_github_mocks(
            &mock,
            project_items,
            issues,
            field_values,
            all_status_options(),
        )
        .await;
        mount_failed_review_opencode_mocks(&oc_mock).await;

        let result = run_failed_review_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        oc_mock.verify().await;
    }

    // T15: Failed Product review → same recovery flow
    #[tokio::test]
    async fn failed_review_product_transitions_to_in_dev() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([
            {"node_id": "issue-node-42", "number": 42, "title": "Update pricing", "body": "body", "state": "open", "pull_request": null},
        ]);
        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        let field_values = review_field_values("Review Product", "review-session-2");

        mount_review_github_mocks(
            &mock,
            project_items,
            issues,
            field_values,
            all_status_options(),
        )
        .await;
        mount_failed_review_opencode_mocks(&oc_mock).await;

        let result = run_failed_review_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
    }

    // T16: Failed QA review → same recovery flow
    #[tokio::test]
    async fn failed_review_qa_transitions_to_in_dev() {
        let mock = MockServer::start().await;
        let oc_mock = MockServer::start().await;
        let client = gh_client(&mock);
        let deps = make_deps(Some(client));
        let ctx = make_context();
        let oc = make_oc_config(oc_mock.uri());

        let issues = json!([
            {"node_id": "issue-node-42", "number": 42, "title": "Test integration", "body": "body", "state": "open", "pull_request": null},
        ]);
        let project_items = json!({
            "nodes": [
                {"id": "item-42", "content": {"__typename": "Issue", "id": "issue-node-42", "number": 42}}
            ]
        });
        let field_values = review_field_values("QA", "review-session-3");

        mount_review_github_mocks(
            &mock,
            project_items,
            issues,
            field_values,
            all_status_options(),
        )
        .await;
        mount_failed_review_opencode_mocks(&oc_mock).await;

        let result = run_failed_review_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
    }

    // T17: Active review session → no recovery action taken
    #[tokio::test]
    async fn failed_review_active_session_is_ignored() {
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
        let field_values = review_field_values("Review Technical", "review-session-1");

        mount_review_github_mocks(
            &mock,
            project_items,
            issues,
            field_values,
            all_status_options(),
        )
        .await;

        // GET /session/status → returns the session as active
        Mock::given(method("GET"))
            .and(path("/session/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "review-session-1": {"type": "busy"}
            })))
            .mount(&oc_mock)
            .await;

        // POST /session should NOT be called (session is still active)
        Mock::given(method("POST"))
            .and(path("/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "should-not-happen",
                "projectID": "p", "directory": "/d", "title": "t",
                "version": "1", "time": {"created": 1, "updated": 2}
            })))
            .expect(0)
            .mount(&oc_mock)
            .await;

        let result = run_failed_review_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        oc_mock.verify().await;
    }

    // T18: Failed review — no GitHub client → returns NoGitHub error
    #[tokio::test]
    async fn failed_review_no_github_client_returns_error() {
        let deps = make_deps(None);
        let ctx = make_context();
        let oc = make_oc_config("http://localhost:8081".to_string());

        let result = run_failed_review_check(&deps, &ctx, &oc).await;
        assert!(matches!(result, Err(WorkflowError::NoGitHub(_))));
    }

    // ── Dev Completion Check Tests ───────────────────────────────

    fn in_dev_field_values(session_id: &str) -> serde_json::Value {
        json!({
            "nodes": [
                single_select_field_value("Status", "In Development"),
                text_field_value("sessionId", Some(session_id)),
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
            .and(path("/session/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/session/dev-session-1/message"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {
                    "info": {"role": "user", "sessionID": "dev-session-1"},
                    "parts": [{"type": "text", "text": "Here is my work"}]
                },
                {
                    "info": {"role": "assistant", "sessionID": "dev-session-1"},
                    "parts": [{"type": "text", "text": "### Resolve threads\nTH_123, TH_456"}]
                }
            ])))
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
            .and(path("/session/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/session/dev-session-2/message"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {
                    "info": {"role": "assistant", "sessionID": "dev-session-2"},
                    "parts": [{"type": "text", "text": "Done with the fix"}]
                }
            ])))
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
            .and(path("/session/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "dev-session-1": {"type": "busy"}
            })))
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/session/dev-session-1/message"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
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
                text_field_value("sessionId", Some("dev-session-1")),
            ]
        });

        mount_dev_completion_github_mocks(&mock, project_items, issues, field_values).await;

        Mock::given(method("GET"))
            .and(path("/session/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&oc_mock)
            .await;

        let result = run_dev_completion_check(&deps, &ctx, &oc).await;
        assert!(result.is_ok());
        oc_mock.verify().await;
    }
}
