//! GitHub-based implementation of external issue sources.

pub mod client;
pub mod issues;
pub mod project;
pub mod repo;
pub mod source;
pub mod types;

// Convenience re-exports
pub use client::{GitHubClient, GitHubError};
pub use source::GitHubIssueSource;
