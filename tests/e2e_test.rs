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

use std::path::PathBuf;
use std::sync::Arc;

use dotenv::dotenv;

use git_automate::config::parse_config;
use git_automate::external_agent::opencode::client::OpenCodeClient;
use git_automate::external_issues::github::client::GitHubClient;
use git_automate::external_issues::github::repo::parse_repository_url;
use git_automate::shell::{ShellFn, ShellOutput};
use git_automate::workflow::Workflow;
use git_automate::workflow::helpers::WorkflowContext;

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

#[tokio::test]
async fn e2e_triage_flow_creates_session() {
    // ── 1. Load .env ──────────────────────────────────────────────
    dotenv().ok();

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

    // Resolve project name and config (assume one project)
    let project_name = config
        .projects
        .keys()
        .next()
        .expect("Config has no projects")
        .clone();
    let project_config = config
        .projects
        .get(&project_name)
        .expect("Project missing from config");

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

    // ── 7. Re-parse config to get resolved project ID ─────────────
    // The setup step may have resolved a numeric projectId to a global ID
    // and persisted it back via write_project_id.
    let resolved_config = parse_config(&config_path).expect("Failed to re-parse config");
    let resolved_project = resolved_config
        .projects
        .get(&project_name)
        .expect("Project missing after workflow");
    let project_id = resolved_project
        .project_id
        .as_ref()
        .expect("Project ID not resolved after workflow");

    // ── 8. Verify: issue is in the project ────────────────────────
    let items = github
        .list_project_items(project_id)
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

    // ── 12. Cleanup: restore cwd ──────────────────────────────────
    std::env::set_current_dir(&original_dir)
        .unwrap_or_else(|e| panic!("Failed to restore cwd: {}", e));
    drop(_cwd_guard);

    eprintln!(
        "e2e test passed: issue #{}, Status=Triage, sessionId={}",
        created_issue.number, session_id
    );
}
