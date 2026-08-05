//! Project V2 field types for the GitHub API client.
//!
//! Mirrors the TypeScript types from `src/github.ts`.

use serde::Deserialize;

/// Summary of a GitHub Project V2, returned by `get_project`.
///
/// `number` is kept as a `String` to match the TypeScript interface
/// (`github.ts:16`) where `String(result.node.number)` is used.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ProjectV2Summary {
    pub id: String,
    pub number: String,
    pub title: String,
}
