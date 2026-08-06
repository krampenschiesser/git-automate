//! Project V2 field types for the GitHub API client.

use serde::Deserialize;

/// Summary of a GitHub Project V2, returned by `get_project`.
///
/// `number` is kept as a `String` because the GitHub GraphQL API returns it
/// as an integer, and string formatting normalizes it for comparison.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ProjectV2Summary {
    pub id: String,
    pub number: String,
    pub title: String,
}
