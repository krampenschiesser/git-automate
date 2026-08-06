//! Repository URL parsing and standalone helper functions.
//!
//! Provides the `ParsedRepo` re-export for convenience.

use super::types::ParsedRepo;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RepoParseError {
    #[error("Could not parse repository URL: {0}")]
    ParseError(String),
}

/// Extract owner and repo name from common GitHub repository URL formats.
///
/// Handles all four formats:
/// - `https://github.com/owner/repo`
/// - `https://github.com/owner/repo.git`
/// - `git@github.com:owner/repo.git`
/// - `owner/repo`
///
/// Steps:
/// 1. Trim surrounding whitespace.
/// 2. Strip a trailing `.git` suffix.
/// 3. Match HTTPS, then SSH, then plain `owner/repo` format.
/// 4. Return error if none match.
pub fn parse_repository_url(repository: &str) -> Result<ParsedRepo, RepoParseError> {
    let trimmed = repository.trim().trim_end_matches(".git");

    // HTTPS / HTTP: https://github.com/owner/repo
    if let Some(caps) = https_match(trimmed) {
        return Ok(ParsedRepo {
            owner: caps.0,
            repo: caps.1,
        });
    }

    // SSH: git@github.com:owner/repo
    if let Some(caps) = ssh_match(trimmed) {
        return Ok(ParsedRepo {
            owner: caps.0,
            repo: caps.1,
        });
    }

    // Plain: owner/repo
    if let Some(caps) = plain_match(trimmed) {
        return Ok(ParsedRepo {
            owner: caps.0,
            repo: caps.1,
        });
    }

    Err(RepoParseError::ParseError(repository.to_string()))
}

// ─── Internal matchers (no capturing group allocation beyond what's needed) ───

fn https_match(s: &str) -> Option<(String, String)> {
    let prefix = "https://github.com/";
    let prefix_http = "http://github.com/";
    let rest = s
        .strip_prefix(prefix)
        .or_else(|| s.strip_prefix(prefix_http))?;
    split_owner_repo(rest)
}

fn ssh_match(s: &str) -> Option<(String, String)> {
    let prefix = "git@github.com:";
    let rest = s.strip_prefix(prefix)?;
    split_owner_repo(rest)
}

fn plain_match(s: &str) -> Option<(String, String)> {
    split_owner_repo(s)
}

/// Split `owner/repo` into owned Strings.
/// Returns None if there is no `/` or if there are extra path segments
/// (e.g. `owner/repo/sub` is rejected — it has more than one `/`).
///
/// This regex `^([^/]+)\/([^/]+)$` requires exactly one `/` with no
/// additional segments.
fn split_owner_repo(s: &str) -> Option<(String, String)> {
    let idx = s.find('/')?;
    let owner = &s[..idx];
    let rest = &s[idx + 1..];
    // Must not contain another `/` — regex [^/]+ for both groups
    if rest.contains('/') {
        return None;
    }
    if owner.is_empty() || rest.is_empty() {
        return None;
    }
    Some((owner.to_string(), rest.to_string()))
}

// ─── Re-export ParsedRepo for callers that import from this module ───

pub use super::types::ParsedRepo as RepoInfo;

#[cfg(test)]
mod tests {
    use super::*;

    // ── HTTPS format ──

    #[test]
    fn https_standard() {
        let r = parse_repository_url("https://github.com/owner/repo").unwrap();
        assert_eq!(r.owner, "owner");
        assert_eq!(r.repo, "repo");
    }

    #[test]
    fn http_standard() {
        let r = parse_repository_url("http://github.com/owner/repo").unwrap();
        assert_eq!(r.owner, "owner");
        assert_eq!(r.repo, "repo");
    }

    #[test]
    fn https_with_git_suffix() {
        let r = parse_repository_url("https://github.com/owner/repo.git").unwrap();
        assert_eq!(r.owner, "owner");
        assert_eq!(r.repo, "repo");
    }

    // ── SSH format ──

    #[test]
    fn ssh_standard() {
        let r = parse_repository_url("git@github.com:owner/repo.git").unwrap();
        assert_eq!(r.owner, "owner");
        assert_eq!(r.repo, "repo");
    }

    #[test]
    fn ssh_without_git() {
        let r = parse_repository_url("git@github.com:owner/repo").unwrap();
        assert_eq!(r.owner, "owner");
        assert_eq!(r.repo, "repo");
    }

    // ── Plain format ──

    #[test]
    fn plain_owner_repo() {
        let r = parse_repository_url("owner/repo").unwrap();
        assert_eq!(r.owner, "owner");
        assert_eq!(r.repo, "repo");
    }

    // ── Edge cases ──

    #[test]
    fn trims_whitespace() {
        let r = parse_repository_url("  https://github.com/owner/repo  ").unwrap();
        assert_eq!(r.owner, "owner");
        assert_eq!(r.repo, "repo");
    }

    #[test]
    fn invalid_no_slash() {
        let r = parse_repository_url("invalid");
        assert!(r.is_err());
    }

    #[test]
    fn invalid_extra_segments() {
        // Has more than one '/' — should not match plain pattern
        let r = parse_repository_url("owner/repo/sub");
        assert!(r.is_err());
    }

    #[test]
    fn invalid_empty_parts() {
        let r = parse_repository_url("owner/");
        assert!(r.is_err());
    }

    #[test]
    fn preserves_dots_in_owner() {
        let r = parse_repository_url("https://github.com/foo.bar/repo-name").unwrap();
        assert_eq!(r.owner, "foo.bar");
        assert_eq!(r.repo, "repo-name");
    }
}
