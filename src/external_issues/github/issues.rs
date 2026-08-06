//! Issue-related type definitions for the GitHub API client.

use serde::Deserialize;

/// A GitHub issue (or PR) as returned by REST endpoints.
///
/// `id` corresponds to `node_id` in REST responses and `id` in GraphQL.
/// `body` is `None` when the issue has no body or the API returns `null`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct IssueInfo {
    pub id: String,
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
}
