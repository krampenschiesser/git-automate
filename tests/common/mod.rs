//! Shared test helpers for git-automate integration tests.
//!
//! This module provides reusable mock server fixtures and dependency builders
//! so each test file can focus on its specific test cases without duplicating
//! boilerplate. Each test file in `tests/` that needs these helpers declares
//! `mod common;` and then uses `common::*` or `common::specific_fn`.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use git_automate::config::{GitAutomateConfig, GitSection, OpencodeConfig};
use git_automate::external_agent::opencode::OpenCodeClient;
use git_automate::external_issues::github::client::GitHubClient;
use git_automate::workflow::helpers::WorkflowContext;

pub use git_automate::test_utils::SET_CWD_MUTEX;

// ─── Helpers ────────────────────────────────────────────────────

/// Build a `GitHubClient` pointed at a mock server.
pub fn gh_client(server: &MockServer) -> GitHubClient {
    GitHubClient::new_with_base_url("test-token".to_string(), server.uri())
        .expect("token is non-empty")
}

/// Build an `OpenCodeClient` pointed at a mock server.
#[allow(dead_code)]
pub fn oc_client(server: &MockServer) -> OpenCodeClient {
    OpenCodeClient::new(server.uri(), "pw".to_string())
}
pub fn project_with_opencode(_url: String) -> GitSection {
    GitSection {
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
    }
}

/// Build a `GitSection` without opencode config.
pub fn project_without_opencode() -> GitSection {
    GitSection {
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
        project_with_opencode(
            opencode_url
                .as_ref()
                .expect("opencode_url must be set when with_opencode")
                .clone(),
        )
    } else {
        project_without_opencode()
    };
    let opencode = if with_opencode {
        Some(OpencodeConfig {
            url: opencode_url.expect("opencode_url must be set when with_opencode"),
            pw: "pw".to_string(),
            cwd: "/test-work".to_string(),
            project: "test-project".to_string(),
            concurrency: HashMap::new(),
        })
    } else {
        None
    };

    WorkflowContext {
        config: GitAutomateConfig {
            git: project,
            opencode,
        },
        github,
        project_id_cache: Arc::new(Mutex::new(HashMap::new())),
        log_dedup: Arc::new(Mutex::new(HashMap::new())),
    }
}

// ── GitHub mock fixtures ──────────────────────────────────────

/// Mount the GraphQL mock that returns the project status field with all 7 workflow options.
///
/// Matches `field(name:` queries (used by `get_project_status_field`).
pub async fn mount_status_field_mock(server: &MockServer) {
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
}

/// Mount the GraphQL mock that returns the project field definitions.
///
/// Matches `fields(first:` queries (used by `get_project_fields`).
pub async fn mount_project_fields_mock(server: &MockServer) {
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
}

/// Mount the GraphQL mock that returns an empty list of project items.
///
/// Matches `items(first:` queries (used by `list_project_items`).
pub async fn mount_empty_items_mock(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("items(first:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "node": { "items": { "nodes": [] } } }
        })))
        .mount(server)
        .await;
}

/// Mount the GraphQL mock for the `addProjectV2ItemById` mutation.
///
/// Used by `add_issue_to_project` to attach an issue to a project board.
pub async fn mount_add_item_mock(server: &MockServer) {
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
}

/// Mount the GraphQL mock for the `updateProjectV2ItemFieldValue` mutation.
///
/// Used for status transitions and session ID updates.
pub async fn mount_update_field_mock(server: &MockServer) {
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
}

/// Mount the GraphQL mock that returns an empty list of field values.
///
/// Matches `fieldValues(first:` queries (used by `get_project_item_values`,
/// empty response signals no existing session).
pub async fn mount_empty_field_values_mock(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("fieldValues(first:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "node": { "fieldValues": { "nodes": [] } } }
        })))
        .mount(server)
        .await;
}

/// Mount the REST mock that returns a single `@ai`-tagged issue.
///
/// Matches `GET /repos/owner/repo/issues` (used by issue polling).
pub async fn mount_ai_issue_mock(server: &MockServer) {
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

/// Mount all GitHub GraphQL + REST mocks needed by setup + triage + todo + review.
///
/// Convenience wrapper that calls every individual mock helper.
/// Prefer the individual helpers when a test only needs a subset.
pub async fn mount_github_graphql_mocks(server: &MockServer) {
    mount_status_field_mock(server).await;
    mount_project_fields_mock(server).await;
    mount_empty_items_mock(server).await;
    mount_add_item_mock(server).await;
    mount_update_field_mock(server).await;
    mount_empty_field_values_mock(server).await;
    mount_ai_issue_mock(server).await;
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
}

/// Mount OpenCode experimental workspace + worktree creation mocks.
///
/// Call alongside `mount_opencode_mocks` when a test starts sessions (which
/// now create a workspace and worktree before the session).
#[allow(dead_code)]
pub async fn mount_opencode_workspace_worktree_mocks(server: &MockServer) {
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
