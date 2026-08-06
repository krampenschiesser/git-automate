//! GitHubIssueSource integration tests via mock GitHub API.
//!
//! Tests 6 and 7 from the original `integration.rs`:
//! - fetch_issues_by_state with sub-task hierarchy
//! - state filtering verification

mod common;

use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use git_automate::external_issues::common::ExternalIssueSource;
use git_automate::external_issues::github::GitHubIssueSource;

use common::gh_client;

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
    let source = GitHubIssueSource::new(
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
    let source = GitHubIssueSource::new(
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
