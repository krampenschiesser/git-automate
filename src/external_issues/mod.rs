//! External issue sources — abstraction layer + provider implementations.

pub mod common;
pub mod github;
pub mod trello;

// Convenience re-exports for the common abstractions.
pub use common::{ExternalIssue, ExternalIssueError, ExternalIssueSource};
