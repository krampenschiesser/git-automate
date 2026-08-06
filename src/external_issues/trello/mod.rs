//! Trello-based implementation of external issue sources.
//!
//! Provides [`TrelloClient`] — a reqwest-based REST client for the Trello API
//! — and [`TrelloIssueSource`] which implements the
//! [`ExternalIssueSource`] trait, fetching cards from a Trello board and
//! returning them as canonical [`ExternalIssue`] records.

pub mod client;
pub mod source;
pub mod types;

// Convenience re-exports
pub use client::{TrelloClient, TrelloError};
pub use source::TrelloIssueSource;
