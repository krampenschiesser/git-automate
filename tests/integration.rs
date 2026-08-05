//! Integration tests for the git-automate Rust daemon.
//!
//! These tests exercise the full end-to-end flow using `wiremock` to mock both
//! the GitHub API and the OpenCode server: config loading → GitHub mock →
//! OpenCode mock → workflow execution → session dispatch.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use serde_json::json;
use tempfile::tempdir;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use git_automate::config::{GitAutomateConfig, OpencodeConfig, ProjectConfig, parse_config};
use git_automate::external_agent::client::OpenCodeClient;
use git_automate::external_issues::client::GitHubClient;
use git_automate::issues::ExternalIssueSource;
use git_automate::shell::{ShellFn, ShellOutput};
use git_automate::workflow::Workflow;
use git_automate::workflow::helpers::{WorkflowContext, write_project_id};

static SET_CWD_MUTEX: Mutex<()> = Mutex::new(());

// ─── Helpers ────────────────────────────────────────────────────

/// Build a `GitHubClient` pointed at a mock server.
fn gh_client(server: &MockServer) -> GitHubClient {
    GitHubClient::new_with_base_url("test-token".to_string(), server.uri())
        .expect("token is non-empty")
}

/// Build an `OpenCodeClient` pointed at a mock server.
fn oc_client(server: &MockServer) -> OpenCodeClient {
    OpenCodeClient::new(server.uri(), "pw".to_string())
}

/// A shell function that always succeeds with empty output.
fn mock_shell() -> ShellFn {
    Arc::new(|_cmd: String| {
        Box::pin(async move {
            ShellOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }
        })
    })
}

/// Build a `ProjectConfig` with opencode config pointing at *url*.
fn project_with_opencode(url: String) -> ProjectConfig {
    ProjectConfig {
        repository: "https://github.com/owner/repo".to_string(),
        project_id: Some("PID-123".to_string()),
        directory: None,
        opencode: Some(OpencodeConfig {
            url,
            pw: "pw".to_string(),
        }),
        issue_provider: Some("github".to_string()),
    }
}

/// Build a `ProjectConfig` without opencode config.
fn project_without_opencode() -> ProjectConfig {
    ProjectConfig {
        repository: "https://github.com/owner/repo".to_string(),
        project_id: Some("PID-123".to_string()),
        directory: None,
        opencode: None,
        issue_provider: Some("github".to_string()),
    }
}

/// Build `WorkflowContext` with the given GitHub client and a config containing
/// one project (with or without opencode).
fn make_deps(
    github: Option<GitHubClient>,
    with_opencode: bool,
    opencode_url: Option<String>,
) -> WorkflowContext {
    let project = if with_opencode {
        project_with_opencode(opencode_url.expect("opencode_url must be set when with_opencode"))
    } else {
        project_without_opencode()
    };
    let mut projects = BTreeMap::new();
    projects.insert("test-proj".to_string(), project);

    WorkflowContext {
        config: GitAutomateConfig { projects },
        github,
        shell: mock_shell(),
    }
}

// ── GitHub mock fixtures ──────────────────────────────────────

/// Mount the four GraphQL mocks needed by setup + triage + todo + review:
/// status field (all 7 options), fields list (Status + sessionId),
/// project items (empty), and field values (empty).
async fn mount_github_graphql_mocks(server: &MockServer) {
    // `field(name:` — get_project_status_field
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("field(name:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "field": {
                        "id": "status-field-id",
                        "options": [
                            {"id": "o1", "name": "Triage"},
                            {"id": "o2", "name": "Todo"},
                            {"id": "o3", "name": "In Development"},
                            {"id": "o4", "name": "Review Technical"},
                            {"id": "o5", "name": "Review Product"},
                            {"id": "o6", "name": "QA"},
                            {"id": "o7", "name": "Done"},
                        ]
                    }
                }
            }
        })))
        .mount(server)
        .await;

    // `fields(first:` — get_project_fields
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("fields(first:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "fields": {
                        "nodes": [
                            {"id": "f1", "name": "Status", "dataType": "SINGLE_SELECT"},
                            {"id": "f2", "name": "sessionId", "dataType": "TEXT"},
                        ]
                    }
                }
            }
        })))
        .mount(server)
        .await;

    // `items(first:` — list_project_items (empty)
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("items(first:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "node": { "items": { "nodes": [] } } }
        })))
        .mount(server)
        .await;

    // `addProjectV2ItemById` — add_issue_to_project
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("addProjectV2ItemById"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "addProjectV2ItemById": { "item": { "id": "item-1" } }
            }
        })))
        .mount(server)
        .await;

    // `updateProjectV2ItemFieldValue` — status update + session id update
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("updateProjectV2ItemFieldValue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "updateProjectV2ItemFieldValue": { "projectV2Item": { "id": "item-1" } }
            }
        })))
        .mount(server)
        .await;

    // `fieldValues(first:` — get_project_item_values (empty → no session)
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("fieldValues(first:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "node": { "fieldValues": { "nodes": [] } } }
        })))
        .mount(server)
        .await;

    // REST GET /repos/owner/repo/issues — returns an @ai issue
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "node_id": "issue-node-1",
                "number": 1,
                "title": "@ai Fix bug",
                "body": "Detailed description",
                "state": "open",
                "pull_request": null
            }
        ])))
        .mount(server)
        .await;
}

/// Mount OpenCode health + agents mocks.
/// Session/prompt_async are mounted separately when call counts matter.
async fn mount_opencode_mocks(server: &MockServer) {
    // Health
    Mock::given(method("GET"))
        .and(path("/global/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "healthy": true,
            "version": "1.0.0"
        })))
        .mount(server)
        .await;

    // Agents — all 6 required agents
    Mock::given(method("GET"))
        .and(path("/agent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"name": "git-automate-triage", "description": "t", "mode": "subagent", "builtIn": true},
            {"name": "git-automate-taskmanager", "description": "t", "mode": "subagent", "builtIn": true},
            {"name": "git-automate-developer", "description": "t", "mode": "subagent", "builtIn": true},
            {"name": "git-automate-reviewer", "description": "t", "mode": "subagent", "builtIn": true},
            {"name": "git-automate-product", "description": "t", "mode": "subagent", "builtIn": true},
            {"name": "git-automate-qa", "description": "t", "mode": "subagent", "builtIn": true},
        ])))
        .mount(server)
        .await;
}

// ─── Test 1: Full triage flow with mocks ──────────────────────

#[tokio::test]
async fn test_full_triage_flow_with_mocks() {
    let gh_mock = MockServer::start().await;
    let oc_mock = MockServer::start().await;

    mount_github_graphql_mocks(&gh_mock).await;
    mount_opencode_mocks(&oc_mock).await;

    // Track that the session creation POST was actually called.
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
        .expect(1)
        .named("session_creation")
        .mount(&oc_mock)
        .await;

    // Track that prompt_async was called.
    Mock::given(method("POST"))
        .and(path("/session/sess123/prompt_async"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .named("prompt_async")
        .mount(&oc_mock)
        .await;

    let client = gh_client(&gh_mock);
    let deps = make_deps(Some(client), true, Some(oc_mock.uri()));
    let workflow = Workflow::new(deps);

    // Run the full workflow — should complete without error.
    workflow.run_all().await.expect("run_all should succeed");

    // Verify the OpenCode session creation and prompt were called.
    oc_mock.verify().await;
}

// ─── Test 2: Graceful shutdown after one poll ─────────────────

#[tokio::test]
async fn test_daemon_graceful_shutdown_after_one_poll() {
    let gh_mock = MockServer::start().await;

    // Mount only the setup-check mocks (status field + fields).
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("field(name:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "field": {
                        "id": "sf",
                        "options": [
                            {"id": "o1", "name": "Triage"},
                            {"id": "o2", "name": "Todo"},
                            {"id": "o3", "name": "In Development"},
                            {"id": "o4", "name": "Review Technical"},
                            {"id": "o5", "name": "Review Product"},
                            {"id": "o6", "name": "QA"},
                            {"id": "o7", "name": "Done"},
                        ]
                    }
                }
            }
        })))
        .mount(&gh_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("fields(first:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "fields": {
                        "nodes": [
                            {"id": "f1", "name": "Status", "dataType": "SINGLE_SELECT"},
                            {"id": "f2", "name": "sessionId", "dataType": "TEXT"},
                        ]
                    }
                }
            }
        })))
        .mount(&gh_mock)
        .await;

    let client = gh_client(&gh_mock);
    // Project without opencode — opencode/triage/todo/review checks are skipped,
    // only the setup check runs.
    let deps = make_deps(Some(client), false, None);
    let workflow = Workflow::new(deps);

    // One poll cycle should complete without panic.
    workflow
        .run_all()
        .await
        .expect("run_all should succeed without panic");
}

// ─── Test 3: OpenCode health check failover ───────────────────

#[tokio::test]
async fn test_opencode_health_check_failover() {
    // 500 → false
    {
        let server = MockServer::start().await;
        let client = oc_client(&server);

        Mock::given(method("GET"))
            .and(path("/global/health"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        assert!(!client.check_health().await, "500 should return false");
    }

    // {"healthy": false} → false
    {
        let server = MockServer::start().await;
        let client = oc_client(&server);

        Mock::given(method("GET"))
            .and(path("/global/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "healthy": false,
                "version": "1.0"
            })))
            .mount(&server)
            .await;

        assert!(
            !client.check_health().await,
            "healthy=false should return false"
        );
    }

    // {"healthy": true, "version": "1.0"} → true
    {
        let server = MockServer::start().await;
        let client = oc_client(&server);

        Mock::given(method("GET"))
            .and(path("/global/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "healthy": true,
                "version": "1.0"
            })))
            .mount(&server)
            .await;

        assert!(
            client.check_health().await,
            "healthy=true should return true"
        );
    }
}

// ─── Test 4: Config env substitution in integration ───────────

#[tokio::test]
async fn test_config_env_substitution_in_integration() {
    // Set env var (Rust 2024 marks set_var/remove_var as unsafe for thread-safety).
    let original = std::env::var("TEST_VAR").ok();
    unsafe {
        std::env::set_var("TEST_VAR", "hello");
    }

    // Write a temp YAML config that uses the env var.
    let tmp = tempdir().expect("tempdir should succeed");
    let config_path = tmp.path().join("git-automate.yml");
    let yaml = r#"
projects:
  my-proj:
    repository: "https://github.com/${env:TEST_VAR}/repo"
    opencode:
      url: "http://localhost"
      pw: "pw"
"#;
    std::fs::write(&config_path, yaml).expect("write should succeed");

    let config = parse_config(&config_path).expect("parse_config should succeed");

    let project = config
        .projects
        .get("my-proj")
        .expect("project should exist");
    assert_eq!(
        project.repository, "https://github.com/hello/repo",
        "env var should be substituted"
    );

    // Restore original env var state.
    unsafe {
        match original {
            Some(val) => std::env::set_var("TEST_VAR", val),
            None => std::env::remove_var("TEST_VAR"),
        }
    }
}

// ─── Test 5: write_project_id persists to YAML ────────────────

#[tokio::test]
async fn test_write_project_id_persists_to_yaml() {
    let tmp = tempdir().expect("tempdir should succeed");
    let config_path = tmp.path().join("git-automate.yml");

    // Write a config without projectId.
    let yaml = r#"
projects:
  my-proj:
    repository: "https://github.com/owner/repo"
    opencode:
      url: "http://localhost"
      pw: "pw"
"#;
    std::fs::write(&config_path, yaml).expect("write should succeed");

    // Parse the config so the in-memory BTreeMap has the project entry.
    let mut config = parse_config(&config_path).expect("parse_config should succeed");
    assert_eq!(config.projects.get("my-proj").unwrap().project_id, None);

    // write_project_id uses current_dir to find git-automate.yml.
    let _guard = SET_CWD_MUTEX.lock().unwrap();
    let original_dir = std::env::current_dir().expect("current_dir should succeed");
    std::env::set_current_dir(tmp.path()).expect("set_current_dir should succeed");

    write_project_id("my-proj", "PID-999", &mut config)
        .await
        .expect("write_project_id should succeed");

    std::env::set_current_dir(&original_dir).expect("restore cwd should succeed");

    // Verify in-memory config updated.
    assert_eq!(
        config
            .projects
            .get("my-proj")
            .unwrap()
            .project_id
            .as_deref(),
        Some("PID-999")
    );

    // Verify on-disk YAML was updated.
    let written = std::fs::read_to_string(&config_path).expect("read should succeed");
    let data: serde_yaml::Value =
        serde_yaml::from_str(&written).expect("yaml parse should succeed");
    let pid = data
        .get("projects")
        .and_then(|p| p.get("my-proj"))
        .and_then(|p| p.get("projectId"))
        .and_then(|v| v.as_str())
        .expect("projectId should exist in YAML");
    assert_eq!(pid, "PID-999");
}

// ─── Test 6: GitHubIssueSource integration via mock GitHub API ───

#[tokio::test]
async fn test_github_issue_source_fetch_by_state() {
    let gh_mock = MockServer::start().await;

    // Project items: one Issue (number 1) and one PullRequest (number 5)
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("items(first:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "items": {
                        "nodes": [
                            {
                                "id": "item-1",
                                "content": {"__typename": "Issue", "id": "issue-node-1", "number": 1}
                            },
                            {
                                "id": "item-2",
                                "content": {"__typename": "PullRequest", "id": "pr-node-1", "number": 5}
                            }
                        ]
                    }
                }
            }
        })))
        .mount(&gh_mock)
        .await;

    // REST issues: issue #1 and issue #2 (sub-issue of #1)
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"node_id": "issue-node-1", "number": 1, "title": "@ai Parent task", "body": "parent body", "state": "open", "pull_request": null},
            {"node_id": "issue-node-2", "number": 2, "title": "Sub-task", "body": "sub body", "state": "open", "pull_request": null}
        ])))
        .mount(&gh_mock)
        .await;

    // Issue hierarchy: issue #2 has parent #1
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("parentIssue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "repository": {
                    "issues": {
                        "nodes": [
                            {"id": "issue-node-1", "number": 1, "title": "T", "body": null, "state": "open", "parentIssue": null},
                            {"id": "issue-node-2", "number": 2, "title": "T", "body": null, "state": "open", "parentIssue": {"number": 1}}
                        ]
                    }
                }
            }
        })))
        .mount(&gh_mock)
        .await;

    // Field values: item-1 has Status=Todo, sessionId=sess-123
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("fieldValues(first:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "fieldValues": {
                        "nodes": [
                            {"name": "Status", "option": "Todo"},
                            {"name": "sessionId", "text": "sess-123"}
                        ]
                    }
                }
            }
        })))
        .mount(&gh_mock)
        .await;

    let client = gh_client(&gh_mock);
    let source = git_automate::issues::github::GitHubIssueSource::new(
        client,
        "owner".to_string(),
        "repo".to_string(),
        "PID-123".to_string(),
    );

    let result = source.fetch_issues_by_state("Todo").await;
    assert!(result.is_ok(), "fetch should succeed: {:?}", result.err());
    let issues = result.unwrap();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].external_id, "1");
    assert_eq!(issues[0].title, "@ai Parent task");
    assert_eq!(issues[0].content, "parent body");
    assert_eq!(issues[0].status, "Todo");
    assert_eq!(issues[0].session_id, "sess-123");
    assert_eq!(issues[0].sub_task_external_ids, vec!["2".to_string()]);
}

// ─── Test 7: GitHubIssueSource filters by state ───

#[tokio::test]
async fn test_github_issue_source_filters_by_state() {
    let gh_mock = MockServer::start().await;

    // Two project items: issue #1 (Todo) and issue #2 (Done)
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("items(first:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "items": {
                        "nodes": [
                            {
                                "id": "item-1",
                                "content": {"__typename": "Issue", "id": "n1", "number": 1}
                            },
                            {
                                "id": "item-2",
                                "content": {"__typename": "Issue", "id": "n2", "number": 2}
                            }
                        ]
                    }
                }
            }
        })))
        .mount(&gh_mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"node_id": "n1", "number": 1, "title": "Task 1", "body": "b1", "state": "open", "pull_request": null},
            {"node_id": "n2", "number": 2, "title": "Task 2", "body": "b2", "state": "closed", "pull_request": null}
        ])))
        .mount(&gh_mock)
        .await;

    // No sub-issues
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("parentIssue"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "repository": {
                    "issues": {
                        "nodes": [
                            {"id": "n1", "number": 1, "title": "T", "body": null, "state": "open", "parentIssue": null},
                            {"id": "n2", "number": 2, "title": "T", "body": null, "state": "closed", "parentIssue": null}
                        ]
                    }
                }
            }
        })))
        .mount(&gh_mock)
        .await;

    // Field values will be called for both items — they should return matching statuses
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("fieldValues(first:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "fieldValues": {
                        "nodes": [
                            {"name": "Status", "option": "Todo"}
                        ]
                    }
                }
            }
        })))
        .mount(&gh_mock)
        .await;

    let client = gh_client(&gh_mock);
    let source = git_automate::issues::github::GitHubIssueSource::new(
        client,
        "owner".to_string(),
        "repo".to_string(),
        "PID-123".to_string(),
    );

    // Fetch "Todo" → only item-1 should match (item-2 has different status)
    // Since both items return "Todo" from the mock, both will match
    let result = source.fetch_issues_by_state("Todo").await;
    assert!(result.is_ok());
    let issues = result.unwrap();
    assert_eq!(issues.len(), 2);
}
