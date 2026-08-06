//! GitHub implementation of [`ExternalIssueSource`].
//!
//! Wraps [`GitHubClient`] together with the resolved `owner`, `repo`, and
//! `project_id` to produce [`ExternalIssue`] results from GitHub's Issues +
//! Projects V2 APIs.

use std::collections::HashMap;
use std::pin::Pin;

use super::client::GitHubClient;
use super::types::IssueInfo;
use crate::external_issues::common::{ExternalIssue, ExternalIssueError, ExternalIssueSource};

/// Issues source backed by GitHub Issues + Projects V2.
///
/// Resolves project items, their field values (`Status`, `sessionId`), and
/// the issue hierarchy (parent → sub-tasks) to produce canonical
/// [`ExternalIssue`] records.
pub struct GitHubIssueSource {
    github: GitHubClient,
    owner: String,
    repo: String,
    project_id: String,
}

impl GitHubIssueSource {
    /// Create a new GitHub issue source for the given project.
    pub fn new(github: GitHubClient, owner: String, repo: String, project_id: String) -> Self {
        Self {
            github,
            owner,
            repo,
            project_id,
        }
    }
}

impl ExternalIssueSource for GitHubIssueSource {
    fn fetch_issues_by_state<'a>(
        &'a self,
        state: &'a str,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<Vec<ExternalIssue>, ExternalIssueError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            // ── Gather data in parallel-compatible order ───────────

            // 1. All project items (Issues + PullRequests linked to the board)
            let project_items = self.github.list_project_items(&self.project_id).await?;

            // 2. All issues in the repo → issue map keyed by number
            let issues = self
                .github
                .list_repo_issues(&self.owner, &self.repo)
                .await?;
            let issue_map: HashMap<i64, &IssueInfo> =
                issues.iter().map(|i| (i.number, i)).collect();

            // 3. Issue hierarchy (parent → children) for sub-task resolution
            let issues_with_parents = self
                .github
                .list_issues_with_parents(&self.owner, &self.repo)
                .await?;
            let mut sub_tasks: HashMap<i64, Vec<String>> = HashMap::new();
            for issue in &issues_with_parents {
                if let Some(parent_number) = issue.parent_number {
                    sub_tasks
                        .entry(parent_number)
                        .or_default()
                        .push(issue.number.to_string());
                }
            }

            // ── Filter + map ─────────────────────────────────────

            let mut result = Vec::new();

            for item in &project_items {
                if item.content_type != "Issue" {
                    continue;
                }

                let field_values = self
                    .github
                    .get_project_item_values(&self.project_id, &item.id)
                    .await?;

                let status = field_values
                    .get("Status")
                    .and_then(|v| v.as_deref())
                    .unwrap_or("");

                if status != state {
                    continue;
                }

                let session_id = field_values
                    .get("sessionId")
                    .and_then(|v| v.as_deref())
                    .unwrap_or("")
                    .to_string();

                let issue = issue_map.get(&item.content_number);

                let title = issue
                    .map(|i| i.title.clone())
                    .unwrap_or_else(|| format!("Issue #{}", item.content_number));

                let content = issue.and_then(|i| i.body.clone()).unwrap_or_default();

                let sub_task_ids = sub_tasks
                    .get(&item.content_number)
                    .cloned()
                    .unwrap_or_default();

                result.push(ExternalIssue {
                    session_id,
                    status: status.to_string(),
                    title,
                    content,
                    external_id: item.content_number.to_string(),
                    sub_task_external_ids: sub_task_ids,
                });
            }

            Ok(result)
        })
    }
}

// ─── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_issues::github::client::GitHubClient;
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Build a GitHubClient pointed at a mock server.
    fn gh_client(server: &MockServer) -> GitHubClient {
        GitHubClient::new_with_base_url("test-token".to_string(), server.uri())
            .expect("token is non-empty")
    }

    // ── Test 1: fetch_issues_by_state returns issues matching the state ──

    #[tokio::test]
    async fn fetch_issues_by_state_returns_matching_issues() {
        let mock = MockServer::start().await;
        let client = gh_client(&mock);
        let source = GitHubIssueSource::new(
            client,
            "owner".to_string(),
            "repo".to_string(),
            "PID-123".to_string(),
        );

        // Project items: one issue (number 1)
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
                                    "content": {"__typename": "Issue", "id": "i1", "number": 1}
                                }
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        // REST issues endpoint
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"node_id": "i1", "number": 1, "title": "@ai Fix bug", "body": "bug body", "state": "open", "pull_request": null}
            ])))
            .mount(&mock)
            .await;

        // Issue hierarchy: no sub-issues
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("parentIssue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "repository": {
                        "issues": {
                            "nodes": [
                                {"id": "i1", "number": 1, "title": "T", "body": null, "state": "open", "parentIssue": null}
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        // Field values: Status=Triage, no sessionId
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fieldValues(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fieldValues": {
                            "nodes": [
                                {"name": "Status", "option": "Triage"},
                                {"name": "sessionId"}
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        // Fetch "Triage" → should return 1 issue
        let result = source.fetch_issues_by_state("Triage").await;
        assert!(result.is_ok(), "fetch should succeed: {:?}", result.err());
        let issues = result.unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].external_id, "1");
        assert_eq!(issues[0].title, "@ai Fix bug");
        assert_eq!(issues[0].content, "bug body");
        assert_eq!(issues[0].status, "Triage");
        assert!(issues[0].session_id.is_empty());
        assert!(issues[0].sub_task_external_ids.is_empty());
    }

    // ── Test 2: fetch_issues_by_state with sub-tasks ──

    #[tokio::test]
    async fn fetch_issues_by_state_with_sub_tasks() {
        let mock = MockServer::start().await;
        let client = gh_client(&mock);
        let source = GitHubIssueSource::new(
            client,
            "owner".to_string(),
            "repo".to_string(),
            "PID-123".to_string(),
        );

        // One project item (issue #1 with status "Todo")
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
                                    "content": {"__typename": "Issue", "id": "i1", "number": 1}
                                }
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        // REST issues
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"node_id": "i1", "number": 1, "title": "@ai Big task", "body": "body", "state": "open", "pull_request": null},
                {"node_id": "i2", "number": 2, "title": "Sub-task", "body": "sub", "state": "open", "pull_request": null}
            ])))
            .mount(&mock)
            .await;

        // Issue hierarchy: issue #2 has parent #1 → #1 has sub-task #2
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("parentIssue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "repository": {
                        "issues": {
                            "nodes": [
                                {"id": "i1", "number": 1, "title": "T", "body": null, "state": "open", "parentIssue": null},
                                {"id": "i2", "number": 2, "title": "T", "body": null, "state": "open", "parentIssue": {"number": 1}}
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        // Field values: Status=Todo, sessionId=sess-123
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
            .mount(&mock)
            .await;

        let result = source.fetch_issues_by_state("Todo").await;
        assert!(result.is_ok(), "fetch should succeed: {:?}", result.err());
        let issues = result.unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].external_id, "1");
        assert_eq!(issues[0].session_id, "sess-123");
        assert_eq!(issues[0].sub_task_external_ids, vec!["2".to_string()]);
    }

    // ── Test 3: fetch_issues_by_state with no matching issues ──

    #[tokio::test]
    async fn fetch_issues_by_state_no_match_returns_empty() {
        let mock = MockServer::start().await;
        let client = gh_client(&mock);
        let source = GitHubIssueSource::new(
            client,
            "owner".to_string(),
            "repo".to_string(),
            "PID-123".to_string(),
        );

        // One project item with status "Done"
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
                                    "content": {"__typename": "Issue", "id": "i1", "number": 1}
                                }
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"node_id": "i1", "number": 1, "title": "Done issue", "body": "body", "state": "closed", "pull_request": null}
            ])))
            .mount(&mock)
            .await;

        // Hierarchy: no children
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("parentIssue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "repository": {
                        "issues": {
                            "nodes": [
                                {"id": "i1", "number": 1, "title": "T", "body": null, "state": "closed", "parentIssue": null}
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        // Field values: Status=Done
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fieldValues(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fieldValues": {
                            "nodes": [
                                {"name": "Status", "option": "Done"}
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        // Fetch "Triage" → no matches
        let result = source.fetch_issues_by_state("Triage").await;
        assert!(result.is_ok());
        let issues = result.unwrap();
        assert!(issues.is_empty());
    }

    // ── Test 4: fetch_issues_by_state skips non-Issue content type ──

    #[tokio::test]
    async fn fetch_issues_by_state_skips_pull_requests() {
        let mock = MockServer::start().await;
        let client = gh_client(&mock);
        let source = GitHubIssueSource::new(
            client,
            "owner".to_string(),
            "repo".to_string(),
            "PID-123".to_string(),
        );

        // One Issue item and one PullRequest item
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
                                    "content": {"__typename": "Issue", "id": "i1", "number": 1}
                                },
                                {
                                    "id": "item-2",
                                    "content": {"__typename": "PullRequest", "id": "pr1", "number": 5}
                                }
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"node_id": "i1", "number": 1, "title": "Issue", "body": "body", "state": "open", "pull_request": null}
            ])))
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("parentIssue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "repository": {
                        "issues": {
                            "nodes": [
                                {"id": "i1", "number": 1, "title": "T", "body": null, "state": "open", "parentIssue": null}
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        // Field values: Status=Triage for item-1
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fieldValues(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fieldValues": {
                            "nodes": [
                                {"name": "Status", "option": "Triage"}
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        let result = source.fetch_issues_by_state("Triage").await;
        assert!(result.is_ok());
        let issues = result.unwrap();
        // Only 1 (the PR was skipped)
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].external_id, "1");
    }

    // ── Test 5: GitHubIssueSource::new stores fields correctly ──

    #[test]
    fn github_issue_source_new_stores_fields() {
        let client = GitHubClient::new("token".to_string()).unwrap();
        let source = GitHubIssueSource::new(
            client,
            "my-owner".to_string(),
            "my-repo".to_string(),
            "PROJ-1".to_string(),
        );
        assert_eq!(source.owner, "my-owner");
        assert_eq!(source.repo, "my-repo");
        assert_eq!(source.project_id, "PROJ-1");
    }
}
