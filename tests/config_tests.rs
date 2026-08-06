//! Config integration tests: env var substitution and Trello provider.
//!
//! Tests 4 and 13 from the original `integration.rs`.
//!
//! These tests are self-contained — they only use `tempdir` and `parse_config`,
//! so no `mod common;` declaration is needed.

use tempfile::tempdir;

use git_automate::config::parse_config;

// ─── Test 4: Config env substitution in integration ───────────

#[tokio::test]
async fn test_config_env_substitution_in_integration() {
    // Set env var (Rust 2024 marks set_var/remove_var as unsafe for thread-safety).
    let original = std::env::var("TEST_VAR").ok();
    unsafe {
        std::env::set_var("TEST_VAR", "hello");
    }

    // Write a temp YAML config that uses the env var.
    let tmp = tempdir().expect("tempdir should succeed");
    let config_path = tmp.path().join("git-automate.yml");
    let yaml = r#"
projects:
  my-proj:
    repository: "https://github.com/${env:TEST_VAR}/repo"
    opencode:
      url: "http://localhost"
      pw: "pw"
"#;
    std::fs::write(&config_path, yaml).expect("write should succeed");

    let config = parse_config(&config_path).expect("parse_config should succeed");

    let project = config
        .projects
        .get("my-proj")
        .expect("project should exist");
    assert_eq!(
        project.repository, "https://github.com/hello/repo",
        "env var should be substituted"
    );

    // Restore original env var state.
    unsafe {
        match original {
            Some(val) => std::env::set_var("TEST_VAR", val),
            None => std::env::remove_var("TEST_VAR"),
        }
    }
}

// ─── Test 13: Config with Trello provider and env var substitution ───

#[tokio::test]
async fn test_config_with_trello_provider() {
    unsafe {
        std::env::set_var("GA_TEST_TRELLO_KEY", "trello-secret");
        std::env::set_var("GA_TEST_TRELLO_TOKEN", "token-secret");
    }

    let tmp = tempdir().expect("tempdir should succeed");
    let config_path = tmp.path().join("git-automate.yml");
    let yaml = r#"
projects:
  trello-board:
    repository: "https://github.com/owner/repo"
    issueProvider: trello
    titlePattern: "@ai.*"
    trelloApiKey: ${env:GA_TEST_TRELLO_KEY}
    trelloToken: ${env:GA_TEST_TRELLO_TOKEN}
    trelloBoardId: BRD-123
"#;
    std::fs::write(&config_path, yaml).expect("write should succeed");

    let config = parse_config(&config_path).expect("parse_config should succeed");
    let project = config
        .projects
        .get("trello-board")
        .expect("project should exist");
    assert_eq!(project.issue_provider.as_deref(), Some("trello"));
    assert_eq!(project.trello_api_key.as_deref(), Some("trello-secret"));
    assert_eq!(project.trello_token.as_deref(), Some("token-secret"));
    assert_eq!(project.trello_board_id.as_deref(), Some("BRD-123"));

    // Restore env vars
    unsafe {
        std::env::remove_var("GA_TEST_TRELLO_KEY");
        std::env::remove_var("GA_TEST_TRELLO_TOKEN");
    }
}
