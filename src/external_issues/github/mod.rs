//! GitHub API client implementation (GraphQL + REST).

pub mod client;
pub mod issues;
pub mod project;
pub mod repo;
pub mod types;

// Convenience re-exports
pub use client::{GitHubClient, GitHubError};
