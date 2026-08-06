//! Shared test helpers for git-automate integration tests.
//!
//! This module provides reusable mock server fixtures and dependency builders
//! so each test file can focus on its specific test cases without duplicating
//! boilerplate. Each test file in `tests/` that needs these helpers declares
//! `mod common;` and then uses `common::*` or `common::specific_fn`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use git_automate::config::{GitAutomateConfig, OpencodeConfig, ProjectConfig};
use git_automate::external_agent::opencode::OpenCodeClient;
use git_automate::external_issues::github::client::GitHubClient;
use git_automate::external_issues::trello::client::TrelloClient;
use git_automate::shell::{ShellFn, ShellOutput};
use git_automate::workflow::helpers::WorkflowContext;

/// Mutex to serialize tests that change the process current directory.
///
/// `write_project_id` reads `git-automate.yml` from `cwd`, so any test that
/// calls it must hold this lock to avoid flakiness when tests run in parallel.
pub static SET_CWD_MUTEX: Mutex<()> = Mutex::new(());

// ─── Helpers ────────────────────────────────────────────────────

/// Build a `GitHubClient` pointed at a mock server.
pub fn gh_client(server: &MockServer) -> GitHubClient {
    GitHubClient::new_with_base_url("test-token".to_string(), server.uri())
        .expect("token is non-empty")
}

/// Build an `OpenCodeClient` pointed at a mock server.
pub fn oc_client(server: &MockServer) -> OpenCodeClient {
    OpenCodeClient::new(server.uri(), "pw".to_string())
}

/// A shell function that always succeeds with empty output.
pub fn mock_shell() -> ShellFn {
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
pub fn project_with_opencode(url: String) -> ProjectConfig {
    ProjectConfig {
        repository: "https://github.com/owner/repo".to_string(),
        project_id: Some("PID-123".to_string()),
        directory: None,
        opencode: Some(OpencodeConfig {
            url,
            pw: "pw".to_string(),
        }),
        issue_provider: Some("github".to_string()),
        title_pattern: "@ai.*".to_string(),
        trello_api_key: None,
        trello_token: None,
        trello_board_id: None,
    }
}

/// Build a `ProjectConfig` without opencode config.
pub fn project_without_opencode() -> ProjectConfig {
    ProjectConfig {
        repository: "https://github.com/owner/repo".to_string(),
        project_id: Some("PID-123".to_string()),
        directory: None,
        opencode: None,
        issue_provider: Some("github".to_string()),
        title_pattern: "@ai.*".to_string(),
        trello_api_key: None,
        trello_token: None,
        trello_board_id: None,
    }
}

/// Build `WorkflowContext` with the given GitHub client and a config containing
/// one project (with or without opencode).
pub fn make_deps(
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
        config: GitAutomateConfig {
            projects,
            concurrency: None,
        },
        github,
        shell: mock_shell(),
    }
}

// ── GitHub mock fixtures ──────────────────────────────────────

/// Mount the four GraphQL mocks needed by setup + triage + todo + review:
/// status field (all 7 options), fields list (Status + sessionId),
/// project items (empty), and field values (empty).
pub async fn mount_github_graphql_mocks(server: &MockServer) {
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
pub async fn mount_opencode_mocks(server: &MockServer) {
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

/// Build a TrelloClient pointed at a mock server.
pub fn tl_client(server: &MockServer) -> TrelloClient {
    TrelloClient::new_with_base_url(
        "test-key".to_string(),
        "test-token".to_string(),
        server.uri(),
    )
    .expect("credentials are non-empty")
}
