//! GitHub API client implementation (GraphQL + REST).

pub mod client;
pub mod issues;
pub mod pr_comments;
pub mod project;
pub mod repo;
pub mod types;

#[cfg(test)]
mod queries_test;

// Convenience re-exports
pub use client::{GitHubClient, GitHubError};
