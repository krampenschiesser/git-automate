//! End-to-end test: runs the real workflow cycle against GitHub + OpenCode.
//!
//! Requires `GITHUB_TOKEN` and `OPENCODE_PW` to be set (loaded from `.env` via `dotenv`).
//! Skips (returns early) if either is missing.
//!
//! Flow:
//! 1. Parse `git-automate.yml` and create a real `@ai` issue via the GitHub API.
//! 2. Run `workflow.run_all()` once (single-cycle mode).
//! 3. Verify:
//!    - The issue appears in the project board
//!    - The issue's Status field is "Triage"
//!    - The issue has a non-empty `sessionId` field
//!    - An OpenCode session exists (`GET /session/status` reports active sessions)
//!    - The session's initial prompt contains the issue body (`GET /session/{id}/message`)

use std::path::PathBuf;
use std::sync::Arc;

use dotenv::dotenv;

use git_automate::config::parse_config;
use git_automate::external_agent::opencode::client::OpenCodeClient;
use git_automate::external_issues::github::client::GitHubClient;
use git_automate::external_issues::github::repo::parse_repository_url;
use git_automate::shell::{ShellFn, ShellOutput};
use git_automate::workflow::Workflow;
use git_automate::workflow::helpers::{WorkflowContext, resolve_project_id};

use git_automate::test_utils::SET_CWD_MUTEX;

/// Build a shell function that delegates to the real `sh -c`.
///
/// Used so the workflow can clone repos and run git commands during the e2e run.
fn real_shell() -> ShellFn {
    Arc::new(|cmd: String| {
        Box::pin(async move {
            let output = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .output()
                .await
                .unwrap_or_else(|e| {
                    panic!("Failed to spawn shell command '{}': {}", cmd, e);
                });
            ShellOutput {
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                exit_code: output.status.code().unwrap_or(-1),
            }
        })
    })
}

/// Resolve the project's Status field ID and build a lookup from status name
/// to option ID.
///
/// Uses the real GitHub API — only valid in e2e tests with a live client.
async fn resolve_status_field_ids(
    github: &GitHubClient,
    project_id: &str,
) -> Result<
    (String, std::collections::HashMap<String, String>),
    git_automate::external_issues::github::client::GitHubError,
> {
    let status_field = github
        .get_project_status_field(project_id)
        .await?
        .expect("Status field must exist");

    let option_map: std::collections::HashMap<String, String> = status_field
        .options
        .into_iter()
        .map(|opt| (opt.name, opt.id))
        .collect();

    Ok((status_field.id, option_map))
}

/// Simulate an agent completing its work by transitioning the project item
/// to `next_status` and clearing the `sessionId` field so the next workflow
/// check can start a new session.
async fn simulate_agent_completion(
    github: &GitHubClient,
    project_id: &str,
    item_id: &str,
    status_field_id: &str,
    session_field_id: &str,
    next_status: &str,
    option_map: &std::collections::HashMap<String, String>,
) -> Result<(), git_automate::external_issues::github::client::GitHubError> {
    let option_id = option_map
        .get(next_status)
        .expect(&format!("status option '{}' must exist", next_status));

    github
        .update_project_item_status(project_id, item_id, status_field_id, option_id)
        .await?;

    github
        .update_project_item_session_id(project_id, item_id, session_field_id, None)
        .await?;

    Ok(())
}

/// Resolve the sessionId field ID for a project.
async fn resolve_session_field_id(
    github: &GitHubClient,
    project_id: &str,
) -> Result<String, git_automate::external_issues::github::client::GitHubError> {
    let fields = github.get_project_fields(project_id).await?;
    fields
        .iter()
        .find(|f| f.name == "sessionId")
        .map(|f| f.id.clone())
        .ok_or_else(|| {
            git_automate::external_issues::github::client::GitHubError::Other(
                "sessionId field not found".to_string(),
            )
        })
}

#[tokio::test]
async fn e2e_triage_flow_creates_session() {
    // ── 1. Load .env ──────────────────────────────────────────────
    dotenv().ok();
    let _ = tracing_subscriber::fmt::try_init();

    let token = std::env::var("GITHUB_TOKEN").unwrap_or_default();
    let opencode_pw = std::env::var("OPENCODE_PW").unwrap_or_default();

    if token.is_empty() || opencode_pw.is_empty() {
        eprintln!("e2e test skipped: GITHUB_TOKEN or OPENCODE_PW is not set in .env");
        return;
    }

    // ── 2. Parse config ───────────────────────────────────────────
    let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let config_path = project_root.join("git-automate.yml");
    let config = parse_config(&config_path).expect("Failed to parse git-automate.yml");

    // Pick a project with OpenCode settings: BTreeMap iteration is sorted, so
    // `keys().next()` is not guaranteed to return the project intended for e2e.
    let (project_name, project_config) = config
        .projects
        .iter()
        .find(|(_, c)| c.opencode.is_some())
        .expect("No project with opencode settings found in config");
    let _project_name = project_name.clone();

    let oc_config = project_config
        .opencode
        .as_ref()
        .expect("Project config missing opencode settings");

    // Parse repository URL → owner/repo
    let parsed =
        parse_repository_url(&project_config.repository).expect("Failed to parse repository URL");

    // ── 3. Create GitHub client ───────────────────────────────────
    let github = GitHubClient::new(token.clone()).expect("Failed to create GitHub client");

    // ── 4. Create OpenCode client (for verification) ──────────────
    let opencode = OpenCodeClient::new(oc_config.url.clone(), oc_config.pw.clone());

    // Skip if the required git-automate agents are not installed. The agents
    // must be present in the OpenCode server's agents directory for the
    // workflow to start triage/todo/review sessions.
    let required_agents = [
        "git-automate-triage",
        "git-automate-taskmanager",
        "git-automate-developer",
        "git-automate-reviewer",
        "git-automate-product",
        "git-automate-qa",
    ];
    let installed_agents = match opencode.get_agents(None).await {
        Ok(agents) => agents
            .into_iter()
            .map(|a| a.name)
            .collect::<std::collections::HashSet<_>>(),
        Err(e) => {
            eprintln!("e2e test skipped: failed to list OpenCode agents: {}", e);
            return;
        }
    };
    let missing: Vec<_> = required_agents
        .iter()
        .filter(|name| !installed_agents.contains(**name))
        .copied()
        .collect();
    if !missing.is_empty() {
        eprintln!(
            "e2e test skipped: required OpenCode agents missing: {}",
            missing.join(", ")
        );
        return;
    }

    // ── 5. Create a unique @ai issue ──────────────────────────────
    let issue_title = format!(
        "@ai e2e test: verify triage flow {}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let issue_body = "This is an e2e test issue created by tests/e2e_test.rs to verify \
        that git-automate picks up @ai-tagged issues, adds them to the project \
        board with Status=Triage, and sets a sessionId field.";

    let created_issue = github
        .create_issue(&parsed.owner, &parsed.repo, &issue_title, issue_body)
        .await
        .expect("Failed to create GitHub issue");

    eprintln!(
        "Created issue #{} ('{}') in {}/{}",
        created_issue.number, issue_title, parsed.owner, parsed.repo
    );

    // Wait for the newly created issue to be visible via the REST API before
    // driving the workflow; GitHub's issue list is eventually consistent.
    let mut visible = false;
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        match github.list_repo_issues(&parsed.owner, &parsed.repo).await {
            Ok(issues) if issues.iter().any(|i| i.number == created_issue.number) => {
                visible = true;
                break;
            }
            _ => continue,
        }
    }
    assert!(
        visible,
        "Created issue #{} not visible in repo issues",
        created_issue.number
    );

    // ── 6. Run the workflow ───────────────────────────────────────
    // Serialize cwd changes; set to project root where git-automate.yml lives
    let _cwd_guard = SET_CWD_MUTEX.lock().await;
    let original_dir = std::env::current_dir().unwrap();
    std::env::set_current_dir(&project_root)
        .unwrap_or_else(|e| panic!("Failed to set cwd to {:?}: {}", project_root, e));

    let deps = WorkflowContext {
        config: config.clone(),
        github: Some(github.clone()),
        shell: real_shell(),
    };

    let workflow = Workflow::new(deps);
    workflow.run_all().await.expect("workflow.run_all() failed");

    // ── 7. Resolve the project ID to a global node ID ─────────────
    // Numeric project IDs are resolved at runtime and are not persisted to
    // config, so re-parsing the file would still yield the numeric value.
    // Explicitly resolve it here before using it for project-item queries.
    let config_project_id = project_config
        .project_id
        .as_ref()
        .expect("Project ID missing from config");
    let project_id = resolve_project_id(&github, &parsed.owner, config_project_id)
        .await
        .expect("Failed to resolve project ID")
        .0;

    // ── 8. Verify: issue is in the project ────────────────────────
    let items = github
        .list_project_items(&project_id)
        .await
        .expect("Failed to list project items");

    let item = items
        .iter()
        .find(|i| i.content_number == created_issue.number)
        .unwrap_or_else(|| {
            panic!(
                "Issue #{} not found in project items (project_id={})",
                created_issue.number, project_id
            )
        });

    eprintln!(
        "Verified: issue #{} is in project (item_id={})",
        created_issue.number, item.id
    );

    // ── 9. Verify: Status = "Triage" ──────────────────────────────
    let values = github
        .get_project_item_values(&item.id)
        .await
        .expect("Failed to get project item values");

    let status = values
        .get("Status")
        .and_then(|v| v.as_ref())
        .expect("Status field not present on project item");
    assert_eq!(
        status, "Triage",
        "Expected Status='Triage', got '{}'",
        status
    );

    // ── 10. Verify: sessionId is set ──────────────────────────────
    let session_id = values
        .get("sessionId")
        .and_then(|v| v.as_ref())
        .expect("sessionId field not present on project item");
    assert!(
        !session_id.is_empty(),
        "sessionId should be non-empty, got: {:?}",
        session_id
    );

    eprintln!(
        "Verified: issue #{} has sessionId={}",
        created_issue.number, session_id
    );

    // ── 11. Verify: OpenCode session exists ───────────────────────
    let active_count = opencode
        .count_active_sessions()
        .await
        .expect("Failed to count active OpenCode sessions");
    assert!(
        active_count > 0,
        "Expected at least one active OpenCode session, got {}",
        active_count
    );

    eprintln!("Verified: OpenCode has {} active session(s)", active_count);

    // ── 12. Verify: initial prompt contains issue content ───────────
    let messages = opencode
        .get_session_messages(session_id, None)
        .await
        .expect("Failed to fetch session messages");

    let first_user_message = messages
        .iter()
        .find(|m| m.is_user())
        .unwrap_or_else(|| panic!("No user message found in session {}", session_id));

    let prompt_text = first_user_message.text();
    assert!(
        prompt_text.contains("git-automate picks up @ai-tagged issues"),
        "Expected the initial prompt to contain the issue body, got: {}",
        prompt_text
    );

    eprintln!(
        "Verified: session {} initial prompt contains issue content",
        session_id
    );

    // ── 13. Cleanup: restore cwd ──────────────────────────────────
    std::env::set_current_dir(&original_dir)
        .unwrap_or_else(|e| panic!("Failed to restore cwd: {}", e));
    drop(_cwd_guard);

    eprintln!(
        "e2e test passed: issue #{}, Status=Triage, sessionId={}",
        created_issue.number, session_id
    );
}

/// End-to-end test: drives the full workflow state flow from Triage to Done.
///
/// Verifies that each workflow check starts the correct agent session and that
/// simulating agent completion (status transition + sessionId clear) allows the
/// next check to pick up the item and start a new session.
///
/// State flow:
///   Triage → Todo → In Development → Review Technical → Review Product → QA → Done
#[tokio::test]
async fn e2e_full_workflow_state_flow() {
    dotenv().ok();
    let _ = tracing_subscriber::fmt::try_init();

    let token = std::env::var("GITHUB_TOKEN").unwrap_or_default();
    let opencode_pw = std::env::var("OPENCODE_PW").unwrap_or_default();

    if token.is_empty() || opencode_pw.is_empty() {
        eprintln!("e2e test skipped: GITHUB_TOKEN or OPENCODE_PW is not set in .env");
        return;
    }

    // ── 1. Parse config ───────────────────────────────────────────
    let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let config_path = project_root.join("git-automate.yml");
    let config = parse_config(&config_path).expect("Failed to parse git-automate.yml");

    let (project_name, project_config) = config
        .projects
        .iter()
        .find(|(_, c)| c.opencode.is_some())
        .expect("No project with opencode settings found in config");
    let _project_name = project_name.clone();

    let oc_config = project_config
        .opencode
        .as_ref()
        .expect("Project config missing opencode settings");

    let parsed =
        parse_repository_url(&project_config.repository).expect("Failed to parse repository URL");

    // ── 2. Create clients ─────────────────────────────────────────
    let github = GitHubClient::new(token.clone()).expect("Failed to create GitHub client");
    let opencode = OpenCodeClient::new(oc_config.url.clone(), oc_config.pw.clone());

    // Skip if required agents are not installed.
    let required_agents = [
        "git-automate-triage",
        "git-automate-taskmanager",
        "git-automate-developer",
        "git-automate-reviewer",
        "git-automate-product",
        "git-automate-qa",
    ];
    let installed_agents = match opencode.get_agents(None).await {
        Ok(agents) => agents
            .into_iter()
            .map(|a| a.name)
            .collect::<std::collections::HashSet<_>>(),
        Err(e) => {
            eprintln!("e2e test skipped: failed to list OpenCode agents: {}", e);
            return;
        }
    };
    let missing: Vec<_> = required_agents
        .iter()
        .filter(|name| !installed_agents.contains(**name))
        .copied()
        .collect();
    if !missing.is_empty() {
        eprintln!(
            "e2e test skipped: required OpenCode agents missing: {}",
            missing.join(", ")
        );
        return;
    }

    // ── 3. Create a unique @ai issue ──────────────────────────────
    let issue_title = format!(
        "@ai e2e test: full workflow state flow {}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let issue_body = "This is an e2e test issue for the full workflow state flow test.";

    let created_issue = github
        .create_issue(&parsed.owner, &parsed.repo, &issue_title, issue_body)
        .await
        .expect("Failed to create GitHub issue");

    eprintln!(
        "Created issue #{} ('{}') in {}/{}",
        created_issue.number, issue_title, parsed.owner, parsed.repo
    );

    // Wait for the issue to be visible.
    let mut visible = false;
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        match github.list_repo_issues(&parsed.owner, &parsed.repo).await {
            Ok(issues) if issues.iter().any(|i| i.number == created_issue.number) => {
                visible = true;
                break;
            }
            _ => continue,
        }
    }
    assert!(
        visible,
        "Created issue #{} not visible in repo issues",
        created_issue.number
    );

    // ── 4. Setup: cwd + workflow context ──────────────────────────
    let _cwd_guard = SET_CWD_MUTEX.lock().await;
    let original_dir = std::env::current_dir().unwrap();
    std::env::set_current_dir(&project_root)
        .unwrap_or_else(|e| panic!("Failed to set cwd to {:?}: {}", project_root, e));

    let deps = WorkflowContext {
        config: config.clone(),
        github: Some(github.clone()),
        shell: real_shell(),
    };
    let workflow = Workflow::new(deps);

    // ── 5. Resolve project ID and field IDs ───────────────────────
    let config_project_id = project_config
        .project_id
        .as_ref()
        .expect("Project ID missing from config");
    let project_id = resolve_project_id(&github, &parsed.owner, config_project_id)
        .await
        .expect("Failed to resolve project ID")
        .0;

    let (status_field_id, status_option_map) = resolve_status_field_ids(&github, &project_id)
        .await
        .expect("Failed to resolve status field IDs");

    let session_field_id = resolve_session_field_id(&github, &project_id)
        .await
        .expect("Failed to resolve session field ID");

    // ── 6. Run triage check ───────────────────────────────────────
    workflow
        .run_triage_check()
        .await
        .expect("triage check failed");

    let items = github
        .list_project_items(&project_id)
        .await
        .expect("Failed to list project items");
    let item = items
        .iter()
        .find(|i| i.content_number == created_issue.number)
        .unwrap_or_else(|| panic!("Issue #{} not in project", created_issue.number));

    let values = github
        .get_project_item_values(&item.id)
        .await
        .expect("Failed to get item values");

    assert_eq!(
        values.get("Status").and_then(|v| v.as_deref()),
        Some("Triage"),
        "Expected Status=Triage after triage check"
    );
    let triage_session_id = values
        .get("sessionId")
        .and_then(|v| v.as_deref())
        .expect("Expected sessionId after triage check");
    assert!(!triage_session_id.is_empty());
    eprintln!("Triage: Status=Triage, sessionId={}", triage_session_id);

    // ── 7. Simulate triage agent completion → Todo ────────────────
    simulate_agent_completion(
        &github,
        &project_id,
        &item.id,
        &status_field_id,
        &session_field_id,
        "Todo",
        &status_option_map,
    )
    .await
    .expect("Failed to transition to Todo");

    // ── 8. Run todo check ─────────────────────────────────────────
    workflow.run_todo_check().await.expect("todo check failed");

    let values = github
        .get_project_item_values(&item.id)
        .await
        .expect("Failed to get item values");
    let todo_session_id = values
        .get("sessionId")
        .and_then(|v| v.as_deref())
        .expect("Expected sessionId after todo check");
    assert!(!todo_session_id.is_empty());
    eprintln!("Todo: sessionId={}", todo_session_id);

    // ── 9. Simulate developer agent completion → Review Technical ─
    // Developer agent sets In Development then Review Technical in one go.
    simulate_agent_completion(
        &github,
        &project_id,
        &item.id,
        &status_field_id,
        &session_field_id,
        "Review Technical",
        &status_option_map,
    )
    .await
    .expect("Failed to transition to Review Technical");

    // ── 10. Run review check (Review Technical) ───────────────────
    workflow
        .run_review_check()
        .await
        .expect("review check failed");

    let values = github
        .get_project_item_values(&item.id)
        .await
        .expect("Failed to get item values");
    let reviewer_session_id = values
        .get("sessionId")
        .and_then(|v| v.as_deref())
        .expect("Expected sessionId after review check (Technical)");
    assert!(!reviewer_session_id.is_empty());
    eprintln!("Review Technical: sessionId={}", reviewer_session_id);

    // ── 11. Simulate reviewer agent completion → Review Product ───
    simulate_agent_completion(
        &github,
        &project_id,
        &item.id,
        &status_field_id,
        &session_field_id,
        "Review Product",
        &status_option_map,
    )
    .await
    .expect("Failed to transition to Review Product");

    // ── 12. Run review check (Review Product) ─────────────────────
    workflow
        .run_review_check()
        .await
        .expect("review check failed");

    let values = github
        .get_project_item_values(&item.id)
        .await
        .expect("Failed to get item values");
    let product_session_id = values
        .get("sessionId")
        .and_then(|v| v.as_deref())
        .expect("Expected sessionId after review check (Product)");
    assert!(!product_session_id.is_empty());
    eprintln!("Review Product: sessionId={}", product_session_id);

    // ── 13. Simulate product agent completion → QA ────────────────
    simulate_agent_completion(
        &github,
        &project_id,
        &item.id,
        &status_field_id,
        &session_field_id,
        "QA",
        &status_option_map,
    )
    .await
    .expect("Failed to transition to QA");

    // ── 14. Run review check (QA) ─────────────────────────────────
    workflow
        .run_review_check()
        .await
        .expect("review check failed");

    let values = github
        .get_project_item_values(&item.id)
        .await
        .expect("Failed to get item values");
    let qa_session_id = values
        .get("sessionId")
        .and_then(|v| v.as_deref())
        .expect("Expected sessionId after review check (QA)");
    assert!(!qa_session_id.is_empty());
    eprintln!("QA: sessionId={}", qa_session_id);

    // ── 15. Simulate QA agent completion → Done ───────────────────
    simulate_agent_completion(
        &github,
        &project_id,
        &item.id,
        &status_field_id,
        &session_field_id,
        "Done",
        &status_option_map,
    )
    .await
    .expect("Failed to transition to Done");

    // ── 16. Run review check — should NOT start a new session ─────
    let active_before = opencode
        .count_active_sessions()
        .await
        .expect("Failed to count active sessions");

    workflow
        .run_review_check()
        .await
        .expect("review check failed");

    let active_after = opencode
        .count_active_sessions()
        .await
        .expect("Failed to count active sessions");

    assert_eq!(
        active_before, active_after,
        "Expected no new session after Done; active sessions changed from {} to {}",
        active_before, active_after
    );

    // ── 17. Final verification: Status = Done, sessionId cleared ──
    let values = github
        .get_project_item_values(&item.id)
        .await
        .expect("Failed to get item values");
    assert_eq!(
        values.get("Status").and_then(|v| v.as_deref()),
        Some("Done"),
        "Expected Status=Done at end of workflow"
    );
    let final_session = values.get("sessionId").and_then(|v| v.as_deref());
    assert!(
        final_session.map(|s| s.is_empty()).unwrap_or(true),
        "Expected sessionId to be empty after Done, got: {:?}",
        final_session
    );

    // ── 18. Cleanup ───────────────────────────────────────────────
    std::env::set_current_dir(&original_dir)
        .unwrap_or_else(|e| panic!("Failed to restore cwd: {}", e));
    drop(_cwd_guard);

    eprintln!(
        "e2e full workflow test passed: issue #{} reached Done",
        created_issue.number
    );
}

/// End-to-end test: failed review recovery flow.
///
/// Verifies that when a review session ends without a status transition,
/// `run_failed_review_check` detects the stale session, transitions the item
/// back to Todo, clears the session, and starts a new developer session.
///
/// Flow:
///   1. Create @ai issue, run triage → Status=Triage, sessionId set
///   2. Simulate triage completion → Todo, run todo check → dev session
///   3. Simulate dev completion → Review Technical, run review check → reviewer session
///   4. Simulate reviewer session ending (set stale session ID)
///   5. Run review check again → run_failed_review_check recovers:
///      - Status transitions to Todo
///      - Old session ID cleared
///      - New developer session started
#[tokio::test]
async fn e2e_failed_review_recovery_flow() {
    dotenv().ok();
    let _ = tracing_subscriber::fmt::try_init();

    let token = std::env::var("GITHUB_TOKEN").unwrap_or_default();
    let opencode_pw = std::env::var("OPENCODE_PW").unwrap_or_default();

    if token.is_empty() || opencode_pw.is_empty() {
        eprintln!("e2e test skipped: GITHUB_TOKEN or OPENCODE_PW is not set in .env");
        return;
    }

    // ── 1. Parse config ───────────────────────────────────────────
    let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let config_path = project_root.join("git-automate.yml");
    let config = parse_config(&config_path).expect("Failed to parse git-automate.yml");

    let (project_name, project_config) = config
        .projects
        .iter()
        .find(|(_, c)| c.opencode.is_some())
        .expect("No project with opencode settings found in config");
    let _project_name = project_name.clone();

    let oc_config = project_config
        .opencode
        .as_ref()
        .expect("Project config missing opencode settings");

    let parsed =
        parse_repository_url(&project_config.repository).expect("Failed to parse repository URL");

    // ── 2. Create clients ─────────────────────────────────────────
    let github = GitHubClient::new(token.clone()).expect("Failed to create GitHub client");
    let opencode = OpenCodeClient::new(oc_config.url.clone(), oc_config.pw.clone());

    // Skip if required agents are not installed.
    let required_agents = [
        "git-automate-triage",
        "git-automate-taskmanager",
        "git-automate-developer",
        "git-automate-reviewer",
        "git-automate-product",
        "git-automate-qa",
    ];
    let installed_agents = match opencode.get_agents(None).await {
        Ok(agents) => agents
            .into_iter()
            .map(|a| a.name)
            .collect::<std::collections::HashSet<_>>(),
        Err(e) => {
            eprintln!("e2e test skipped: failed to list OpenCode agents: {}", e);
            return;
        }
    };
    let missing: Vec<_> = required_agents
        .iter()
        .filter(|name| !installed_agents.contains(**name))
        .copied()
        .collect();
    if !missing.is_empty() {
        eprintln!(
            "e2e test skipped: required OpenCode agents missing: {}",
            missing.join(", ")
        );
        return;
    }

    // ── 3. Create a unique @ai issue ──────────────────────────────
    let issue_title = format!(
        "@ai e2e test: failed review recovery {}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let issue_body = "This is an e2e test issue for the failed review recovery flow. \
        It verifies that run_failed_review_check detects stale sessions and starts new dev work.";

    let created_issue = github
        .create_issue(&parsed.owner, &parsed.repo, &issue_title, issue_body)
        .await
        .expect("Failed to create GitHub issue");

    eprintln!(
        "Created issue #{} in {}/{}",
        created_issue.number, parsed.owner, parsed.repo
    );

    // Wait for the issue to be visible.
    let mut visible = false;
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        match github.list_repo_issues(&parsed.owner, &parsed.repo).await {
            Ok(issues) if issues.iter().any(|i| i.number == created_issue.number) => {
                visible = true;
                break;
            }
            _ => continue,
        }
    }
    assert!(
        visible,
        "Created issue #{} not visible in repo issues",
        created_issue.number
    );

    // ── 4. Setup: cwd + workflow context ──────────────────────────
    let _cwd_guard = SET_CWD_MUTEX.lock().await;
    let original_dir = std::env::current_dir().unwrap();
    std::env::set_current_dir(&project_root)
        .unwrap_or_else(|e| panic!("Failed to set cwd to {:?}: {}", project_root, e));

    let deps = WorkflowContext {
        config: config.clone(),
        github: Some(github.clone()),
        shell: real_shell(),
    };
    let workflow = Workflow::new(deps);

    // Resolve project ID and field IDs.
    let config_project_id = project_config
        .project_id
        .as_ref()
        .expect("Project ID missing from config");
    let project_id = resolve_project_id(&github, &parsed.owner, config_project_id)
        .await
        .expect("Failed to resolve project ID")
        .0;

    let (status_field_id, status_option_map) = resolve_status_field_ids(&github, &project_id)
        .await
        .expect("Failed to resolve status field IDs");

    let session_field_id = resolve_session_field_id(&github, &project_id)
        .await
        .expect("Failed to resolve session field ID");

    // ── 5. Run triage check ───────────────────────────────────────
    workflow
        .run_triage_check()
        .await
        .expect("triage check failed");

    let items = github
        .list_project_items(&project_id)
        .await
        .expect("Failed to list project items");
    let item = items
        .iter()
        .find(|i| i.content_number == created_issue.number)
        .unwrap_or_else(|| panic!("Issue #{} not in project", created_issue.number));

    let values = github
        .get_project_item_values(&item.id)
        .await
        .expect("Failed to get item values");

    assert_eq!(
        values.get("Status").and_then(|v| v.as_deref()),
        Some("Triage"),
        "Expected Status=Triage after triage check"
    );
    eprintln!("Triage: Status=Triage");

    // ── 6. Simulate triage completion → Todo ──────────────────────
    simulate_agent_completion(
        &github,
        &project_id,
        &item.id,
        &status_field_id,
        &session_field_id,
        "Todo",
        &status_option_map,
    )
    .await
    .expect("Failed to transition to Todo");

    // ── 7. Run todo check ─────────────────────────────────────────
    workflow.run_todo_check().await.expect("todo check failed");

    let values = github
        .get_project_item_values(&item.id)
        .await
        .expect("Failed to get item values");
    let dev_session_id = values
        .get("sessionId")
        .and_then(|v| v.as_deref())
        .expect("Expected sessionId after todo check");
    assert!(!dev_session_id.is_empty());
    eprintln!("Todo: dev session started, sessionId={}", dev_session_id);

    // ── 8. Simulate developer completion → Review Technical ───────
    simulate_agent_completion(
        &github,
        &project_id,
        &item.id,
        &status_field_id,
        &session_field_id,
        "Review Technical",
        &status_option_map,
    )
    .await
    .expect("Failed to transition to Review Technical");

    // ── 9. Run review check (Review Technical) ────────────────────
    workflow
        .run_review_check()
        .await
        .expect("review check failed");

    let values = github
        .get_project_item_values(&item.id)
        .await
        .expect("Failed to get item values");
    let reviewer_session_id = values
        .get("sessionId")
        .and_then(|v| v.as_deref())
        .expect("Expected sessionId after review check (Technical)");
    assert!(!reviewer_session_id.is_empty());
    assert_ne!(
        reviewer_session_id, dev_session_id,
        "Reviewer session should differ from dev session"
    );
    eprintln!(
        "Review Technical: reviewer session started, sessionId={}",
        reviewer_session_id
    );

    // Verify the reviewer session is active.
    let active_count = opencode
        .count_active_sessions()
        .await
        .expect("Failed to count active sessions");
    assert!(active_count > 0, "Expected active reviewer session");
    eprintln!("Active sessions: {}", active_count);

    // ── 10. Simulate reviewer session ending ──────────────────────
    github
        .update_project_item_session_id(
            &project_id,
            &item.id,
            &session_field_id,
            Some("stale-session-id-that-no-longer-exists"),
        )
        .await
        .expect("Failed to set stale session ID");

    eprintln!("Set stale session ID on item #{}", created_issue.number);

    // ── 11. Run review check again ─────────────────────────────────
    // This should trigger run_failed_review_check which detects the stale
    // session, transitions the item to Todo, clears the session, and starts
    // a new developer session.
    workflow
        .run_review_check()
        .await
        .expect("review check with failed review failed");

    // ── 12. Verify: recovery completed ────────────────────────────
    // After run_failed_review_check:
    // - Status should have changed from "Review Technical" to "Todo"
    // - The old stale session ID should be cleared
    // - A new developer session should have been started
    let values = github
        .get_project_item_values(&item.id)
        .await
        .expect("Failed to get item values");

    let recovered_status = values
        .get("Status")
        .and_then(|v| v.as_deref())
        .expect("Status should be set after recovery");
    assert_eq!(
        recovered_status, "Todo",
        "Expected Status=Todo after failed review recovery, got '{}'",
        recovered_status
    );
    eprintln!("Recovery: Status=Todo");

    let recovered_session_id = values
        .get("sessionId")
        .and_then(|v| v.as_deref())
        .expect("sessionId should be set after recovery");
    assert!(
        !recovered_session_id.is_empty(),
        "sessionId should be non-empty after recovery (new dev session started)"
    );
    assert_ne!(
        recovered_session_id, "stale-session-id-that-no-longer-exists",
        "sessionId should be replaced with new dev session ID"
    );
    assert_ne!(
        recovered_session_id, reviewer_session_id,
        "Recovered session should differ from the stale reviewer session"
    );
    // The new session should be a developer session (different from the
    // reviewer session we set earlier).
    assert_ne!(
        recovered_session_id, reviewer_session_id,
        "New dev session ID should differ from reviewer session"
    );
    eprintln!(
        "Recovery: new dev session started, sessionId={}",
        recovered_session_id
    );

    // ── 13. Cleanup ───────────────────────────────────────────────
    std::env::set_current_dir(&original_dir)
        .unwrap_or_else(|e| panic!("Failed to restore cwd: {}", e));
    drop(_cwd_guard);

    eprintln!(
        "e2e failed review recovery test passed: issue #{}, recovered with new dev session",
        created_issue.number
    );
}
