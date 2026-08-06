//! External issue source abstraction layer.
//!
//! Provides a trait-based abstraction over any issue management system
//! (GitHub, GitLab, Jira, etc.). Each provider implements [`ExternalIssueSource`]
//! and returns issues in the canonical [`ExternalIssue`] shape, decoupling the
//! workflow engine from the underlying issue backend.
//!
//! The concrete GitHub implementation lives in [`crate::external_issues::github`].

/// Canonical issue representation returned by any [`ExternalIssueSource`].
///
/// This struct abstracts away provider-specific concepts (GitHub issues,
/// project board items, etc.) into a uniform shape that the workflow engine
/// can reason about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalIssue {
    /// The OpenCode session ID associated with this issue (empty if none).
    pub session_id: String,
    /// The current workflow status (e.g. "Triage", "Todo", "In Development").
    pub status: String,
    /// The issue title.
    pub title: String,
    /// The issue body / content (may be empty).
    pub content: String,
    /// The external ID — typically the issue number as a string.
    pub external_id: String,
    /// IDs of sub-tasks (child issues) that belong to this issue.
    pub sub_task_external_ids: Vec<String>,
}

/// Error type for operations performed by [`ExternalIssueSource`] implementations.
#[derive(Debug, thiserror::Error)]
pub enum ExternalIssueError {
    #[error("External issue source error: {0}")]
    Other(String),
    #[error("GitHub error: {0}")]
    GitHub(#[from] crate::external_issues::github::client::GitHubError),
    #[error("Trello error: {0}")]
    Trello(#[from] crate::external_issues::trello::client::TrelloError),
    #[error("Issue not found: {0}")]
    IssueNotFound(String),
    #[error("Project not found: {0}")]
    ProjectNotFound(String),
}

/// Abstraction over any external issue management system.
///
/// Implementations fetch issues from a specific backend (GitHub, GitLab, etc.)
/// and return them in the canonical [`ExternalIssue`] shape. The trait is
/// `Send + Sync` so it can be used from async workflow code behind an `Arc`.
pub trait ExternalIssueSource: Send + Sync {
    /// Fetch all issues whose workflow status matches *state*.
    ///
    /// *state* is a provider-agnostic workflow status name such as "Triage",
    /// "Todo", "In Development", "Review Technical", "Review Product", "QA",
    /// or "Done". Implementations translate *state* into the provider-specific
    /// query (e.g. GitHub project item field values).
    ///
    /// Only issues in the requested state are returned. Sub-task
    /// relationships are resolved and populated in `sub_task_external_ids`.
    fn fetch_issues_by_state<'a>(
        &'a self,
        state: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Vec<ExternalIssue>, ExternalIssueError>>
                + Send
                + 'a,
        >,
    >;
}

// ─── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // Test 1: ExternalIssue can be constructed with all fields
    #[test]
    fn external_issue_constructs_with_all_fields() {
        let issue = ExternalIssue {
            session_id: "sess-123".to_string(),
            status: "Triage".to_string(),
            title: "@ai Fix bug".to_string(),
            content: "Detailed description".to_string(),
            external_id: "42".to_string(),
            sub_task_external_ids: vec!["43".to_string(), "44".to_string()],
        };
        assert_eq!(issue.session_id, "sess-123");
        assert_eq!(issue.status, "Triage");
        assert_eq!(issue.title, "@ai Fix bug");
        assert_eq!(issue.content, "Detailed description");
        assert_eq!(issue.external_id, "42");
        assert_eq!(issue.sub_task_external_ids.len(), 2);
    }

    // Test 2: ExternalIssue with no sub-tasks has empty vec
    #[test]
    fn external_issue_no_sub_tasks_has_empty_vec() {
        let issue = ExternalIssue {
            session_id: String::new(),
            status: "Todo".to_string(),
            title: "Implement feature".to_string(),
            content: String::new(),
            external_id: "7".to_string(),
            sub_task_external_ids: Vec::new(),
        };
        assert!(issue.sub_task_external_ids.is_empty());
        assert!(issue.session_id.is_empty());
        assert!(issue.content.is_empty());
    }

    // Test 3: ExternalIssue is Clone + PartialEq + Eq
    #[test]
    fn external_issue_clone_and_equality() {
        let original = ExternalIssue {
            session_id: "sess".to_string(),
            status: "Done".to_string(),
            title: "T".to_string(),
            content: "C".to_string(),
            external_id: "1".to_string(),
            sub_task_external_ids: vec!["2".to_string()],
        };
        let cloned = original.clone();
        assert_eq!(original, cloned);
    }

    // Test 4: ExternalIssueError variants
    #[test]
    fn external_issue_error_other() {
        let err = ExternalIssueError::Other("something broke".to_string());
        assert!(err.to_string().contains("something broke"));
    }

    // Test 5: ExternalIssueError IssueNotFound
    #[test]
    fn external_issue_error_issue_not_found() {
        let err = ExternalIssueError::IssueNotFound("issue-42".to_string());
        assert!(err.to_string().contains("issue-42"));
    }

    // Test 6: ExternalIssueError ProjectNotFound
    #[test]
    fn external_issue_error_project_not_found() {
        let err = ExternalIssueError::ProjectNotFound("PROJ-1".to_string());
        assert!(err.to_string().contains("PROJ-1"));
    }

    // Test 7: HashSet can be used for filtering issue IDs
    #[test]
    fn hashset_filter_for_issue_ids() {
        let seen: HashSet<String> = std::iter::once("42".to_string()).collect();
        assert!(seen.contains("42"));
        assert!(!seen.contains("43"));
    }
}
