//! Workflow integration tests: triage flow, setup initialization, and helpers.
//!
//! Tests 1, 2, 5, 8, 9, 10 from the original `integration.rs`.

mod common;

use serde_json::json;
use tempfile::tempdir;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::*;
use git_automate::config::{GitAutomateConfig, GitSection, OpencodeConfig, parse_config};
use git_automate::workflow::Workflow;
use git_automate::workflow::checks::{OpencodeSessionConfig, run_review_check};
use git_automate::workflow::helpers::{ProjectContext, WorkflowContext, write_project_id};

// ─── Test 1: Full triage flow with mocks ──────────────────────

#[tokio::test]
async fn test_full_triage_flow_with_mocks() {
    let gh_mock = MockServer::start().await;
    let oc_mock = MockServer::start().await;

    mount_github_graphql_mocks(&gh_mock).await;
    mount_opencode_mocks(&oc_mock).await;

    // Track that the session creation POST was actually called.
    Mock::given(method("POST"))
        .and(path("/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "sess123",
            "projectID": "p1",
            "directory": "/d",
            "title": "t",
            "version": "1",
            "time": {"created": 1, "updated": 2}
        })))
        .expect(1)
        .named("session_creation")
        .mount(&oc_mock)
        .await;

    // Track that prompt_async was called.
    Mock::given(method("POST"))
        .and(path("/session/sess123/prompt_async"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .named("prompt_async")
        .mount(&oc_mock)
        .await;

    let client = gh_client(&gh_mock);
    let deps = make_deps(Some(client), true, Some(oc_mock.uri()));
    let workflow = Workflow::new(deps);

    // Run the full workflow — should complete without error.
    workflow.run_all().await.expect("run_all should succeed");

    // Verify the OpenCode session creation and prompt were called.
    oc_mock.verify().await;
}

// ─── Test 2: Graceful shutdown after one poll ─────────────────

#[tokio::test]
async fn test_daemon_graceful_shutdown_after_one_poll() {
    let gh_mock = MockServer::start().await;

    // Mount only the setup-check mocks (status field + fields).
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("field(name:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "field": {
                        "id": "sf",
                        "options": [
                            {"id": "o1", "name": "Triage"},
                            {"id": "o2", "name": "Todo"},
                            {"id": "o3", "name": "In Development"},
                            {"id": "o4", "name": "Review Technical"},
                            {"id": "o5", "name": "Review Product"},
                            {"id": "o6", "name": "QA"},
                            {"id": "o7", "name": "Done"},
                        ]
                    }
                }
            }
        })))
        .mount(&gh_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("fields(first:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "fields": {
                        "nodes": [
                            {"id": "f1", "name": "Status", "dataType": "SINGLE_SELECT"},
                            {"id": "f2", "name": "sessionId", "dataType": "TEXT"},
                        ]
                    }
                }
            }
        })))
        .mount(&gh_mock)
        .await;

    let client = gh_client(&gh_mock);
    // Project without opencode — opencode/triage/todo/review checks are skipped,
    // only the setup check runs.
    let deps = make_deps(Some(client), false, None);
    let workflow = Workflow::new(deps);

    // One poll cycle should complete without panic.
    workflow
        .run_all()
        .await
        .expect("run_all should succeed without panic");
}

// ─── Test 5: write_project_id persists to YAML ────────────────

#[tokio::test]
async fn test_write_project_id_persists_to_yaml() {
    let tmp = tempdir().expect("tempdir should succeed");
    let config_path = tmp.path().join("git-automate.yml");

    // Write a config without projectId.
    let yaml = r#"
opencode:
  url: "http://localhost"
  pw: "pw"
git:
  repository: "https://github.com/owner/repo"
"#;
    std::fs::write(&config_path, yaml).expect("write should succeed");

    // Parse the config so the in-memory git section is populated.
    let mut config = parse_config(&config_path).expect("parse_config should succeed");
    assert_eq!(config.git.project_id, None);

    // write_project_id uses current_dir to find git-automate.yml.
    let _guard = SET_CWD_MUTEX.lock().await;
    let original_dir = std::env::current_dir().expect("current_dir should succeed");
    std::env::set_current_dir(tmp.path()).expect("set_current_dir should succeed");

    write_project_id("my-proj", "PID-999", &mut config)
        .await
        .expect("write_project_id should succeed");

    std::env::set_current_dir(&original_dir).expect("restore cwd should succeed");

    // Verify in-memory config updated.
    assert_eq!(config.git.project_id.as_deref(), Some("PID-999"));

    // Verify on-disk YAML was updated.
    let written = std::fs::read_to_string(&config_path).expect("read should succeed");
    let data: serde_yaml::Value =
        serde_yaml::from_str(&written).expect("yaml parse should succeed");
    let pid = data
        .get("git")
        .and_then(|g| g.get("projectId"))
        .and_then(|v| v.as_str())
        .expect("projectId should exist in YAML");
    assert_eq!(pid, "PID-999");
}

// ─── Test 8: Setup initialization creates project, fields, and statuses ───
//
// When a project has no `projectId` in config, `run_setup_check` must:
//   1. Resolve the owner node ID (user/organization query).
//   2. Create the GitHub Project V2.
//   3. Persist the new project ID to `git-automate.yml`.
//   4. Ensure all 7 status options exist on the Status field.
//   5. Ensure the `sessionId` text field exists on the project.
//
// Each GraphQL call is matched by `body_string_contains` on a unique substring
// so the mocks are mutually exclusive:
//   - `user(login:`       → get_owner_id query
//   - `createProjectV2(input` → createProjectV2 mutation (not createProjectV2Field)
//   - `field(name:`       → get_project_status_field query
//   - `updateProjectV2Field` → add_project_status_options mutation
//   - `fields(first:`     → get_project_fields query
//   - `createProjectV2Field` → add_project_field mutation

#[tokio::test]
async fn test_setup_initialization_creates_project_fields_and_statuses() {
    let gh_mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("user(login:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "user": { "id": "owner-node-id" } }
        })))
        .expect(1)
        .named("get_owner_id")
        .mount(&gh_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("createProjectV2(input"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "createProjectV2": { "id": "PVT-123" } }
        })))
        .expect(1)
        .named("create_project_v2")
        .mount(&gh_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("field(name:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "field": { "id": "status-field-id", "options": [] }
                }
            }
        })))
        .expect(1)
        .named("get_status_field")
        .mount(&gh_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("updateProjectV2Field"))
        .and(body_string_contains("... on ProjectV2Field"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "updateProjectV2Field": {
                    "projectV2Field": { "id": "status-field-id" }
                }
            }
        })))
        .expect(1)
        .named("add_status_options")
        .mount(&gh_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("fields(first:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "fields": {
                        "nodes": [
                            { "id": "f1", "name": "Status", "dataType": "SINGLE_SELECT" }
                        ]
                    }
                }
            }
        })))
        .expect(1)
        .named("get_project_fields")
        .mount(&gh_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("createProjectV2Field"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "createProjectV2Field": { "projectField": { "id": "session-field-id" } }
            }
        })))
        .expect(1)
        .named("add_session_id_field")
        .mount(&gh_mock)
        .await;

    // ── Arrange: config file without projectId ──────────────────
    let tmp = tempdir().expect("tempdir should succeed");
    let config_path = tmp.path().join("git-automate.yml");
    let yaml = r#"
git:
  repository: "https://github.com/owner/repo"
  titlePattern: "@ai.*"
"#;
    std::fs::write(&config_path, yaml).expect("write should succeed");

    let config = parse_config(&config_path).expect("parse_config should succeed");
    assert_eq!(
        config.git.project_id, None,
        "project should start without a projectId"
    );

    // ── Act ─────────────────────────────────────────────────────
    let client = gh_client(&gh_mock);
    let deps = WorkflowContext {
        config,
        github: Some(client),
        shell: mock_shell(),
    };

    // write_project_id writes to git-automate.yml in cwd, so chdir to temp dir.
    let _guard = SET_CWD_MUTEX.lock().await;
    let original_dir = std::env::current_dir().expect("current_dir should succeed");
    std::env::set_current_dir(tmp.path()).expect("set_current_dir should succeed");

    let workflow = Workflow::new(deps);
    let result = workflow.run_setup_check().await;

    std::env::set_current_dir(&original_dir).expect("restore cwd should succeed");

    assert!(result.is_ok(), "run_setup_check should succeed");

    gh_mock.verify().await;

    let written = std::fs::read_to_string(&config_path).expect("read should succeed");
    let data: serde_yaml::Value =
        serde_yaml::from_str(&written).expect("yaml parse should succeed");
    let pid = data
        .get("git")
        .and_then(|g| g.get("projectId"))
        .and_then(|v| v.as_str())
        .expect("projectId should exist in YAML after setup");
    assert_eq!(
        pid, "PVT-123",
        "projectId in YAML should match the ID returned by createProjectV2"
    );
}

// ─── Test 9: Setup is idempotent when project, all statuses, and sessionId exist ───

#[tokio::test]
async fn test_setup_initialization_idempotent_when_everything_exists() {
    let gh_mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("field(name:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "field": {
                        "id": "status-field-id",
                        "options": [
                            { "id": "o1", "name": "Triage" },
                            { "id": "o2", "name": "Todo" },
                            { "id": "o3", "name": "In Development" },
                            { "id": "o4", "name": "Review Technical" },
                            { "id": "o5", "name": "Review Product" },
                            { "id": "o6", "name": "QA" },
                            { "id": "o7", "name": "Done" },
                        ]
                    }
                }
            }
        })))
        .expect(1)
        .named("get_status_field")
        .mount(&gh_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("updateProjectV2Field"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "updateProjectV2Field": {
                    "projectV2Field": { "id": "status-field-id" }
                }
            }
        })))
        .expect(0)
        .named("add_status_options_should_not_be_called")
        .mount(&gh_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("fields(first:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "fields": {
                        "nodes": [
                            { "id": "f1", "name": "Status", "dataType": "SINGLE_SELECT" },
                            { "id": "f2", "name": "sessionId", "dataType": "TEXT" },
                        ]
                    }
                }
            }
        })))
        .expect(1)
        .named("get_project_fields")
        .mount(&gh_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("createProjectV2Field"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "createProjectV2Field": { "projectField": { "id": "session-field-id" } }
            }
        })))
        .expect(0)
        .named("add_session_id_field_should_not_be_called")
        .mount(&gh_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("user(login:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "user": { "id": "owner-node-id" } }
        })))
        .expect(0)
        .named("get_owner_id_should_not_be_called")
        .mount(&gh_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("createProjectV2(input"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "createProjectV2": { "id": "PVT-999" } }
        })))
        .expect(0)
        .named("create_project_v2_should_not_be_called")
        .mount(&gh_mock)
        .await;

    // ── Arrange: config with projectId already set ──────────────
    let tmp = tempdir().expect("tempdir should succeed");
    let config_path = tmp.path().join("git-automate.yml");
    let yaml = r#"
git:
  repository: "https://github.com/owner/repo"
  projectId: "PID-123"
  titlePattern: "@ai.*"
"#;
    std::fs::write(&config_path, yaml).expect("write should succeed");

    let config = parse_config(&config_path).expect("parse_config should succeed");
    assert_eq!(
        config.git.project_id.as_deref(),
        Some("PID-123"),
        "project should already have projectId"
    );

    // ── Act ─────────────────────────────────────────────────────
    let client = gh_client(&gh_mock);
    let deps = WorkflowContext {
        config,
        github: Some(client),
        shell: mock_shell(),
    };

    let workflow = Workflow::new(deps);
    let result = workflow.run_setup_check().await;

    assert!(result.is_ok(), "run_setup_check should succeed");

    gh_mock.verify().await;
}

// ─── Test 10: Setup adds missing status options and sessionId field ───
//
// When the project already has an ID but is missing some status options
// and has no sessionId field, setup should add the missing options and
// create the sessionId field — without creating a new project.

#[tokio::test]
async fn test_setup_initialization_adds_missing_status_options_and_field() {
    let gh_mock = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("field(name:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "field": {
                        "id": "status-field-id",
                        "options": [
                            { "id": "o1", "name": "Triage" },
                            { "id": "o2", "name": "Todo" },
                        ]
                    }
                }
            }
        })))
        .expect(1)
        .named("get_status_field")
        .mount(&gh_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("updateProjectV2Field"))
        .and(body_string_contains("... on ProjectV2Field"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "updateProjectV2Field": {
                    "projectV2Field": { "id": "status-field-id" }
                }
            }
        })))
        .expect(1)
        .named("add_status_options")
        .mount(&gh_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("fields(first:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "fields": {
                        "nodes": [
                            { "id": "f1", "name": "Status", "dataType": "SINGLE_SELECT" }
                        ]
                    }
                }
            }
        })))
        .expect(1)
        .named("get_project_fields")
        .mount(&gh_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("createProjectV2Field"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "createProjectV2Field": { "projectField": { "id": "session-field-id" } }
            }
        })))
        .expect(1)
        .named("add_session_id_field")
        .mount(&gh_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("createProjectV2(input"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "createProjectV2": { "id": "PVT-999" } }
        })))
        .expect(0)
        .named("create_project_v2_should_not_be_called")
        .mount(&gh_mock)
        .await;

    // ── Arrange: config with projectId, missing some statuses + no sessionId
    let tmp = tempdir().expect("tempdir should succeed");
    let config_path = tmp.path().join("git-automate.yml");
    let yaml = r#"
git:
  repository: "https://github.com/owner/repo"
  projectId: "PID-123"
  titlePattern: "@ai.*"
"#;
    std::fs::write(&config_path, yaml).expect("write should succeed");

    let config = parse_config(&config_path).expect("parse_config should succeed");
    assert_eq!(config.git.project_id.as_deref(), Some("PID-123"));

    // ── Act ─────────────────────────────────────────────────────
    let client = gh_client(&gh_mock);
    let deps = WorkflowContext {
        config,
        github: Some(client),
        shell: mock_shell(),
    };

    let workflow = Workflow::new(deps);
    let result = workflow.run_setup_check().await;

    // ── Assert ──────────────────────────────────────────────────
    assert!(result.is_ok(), "run_setup_check should succeed");

    gh_mock.verify().await;
}

// ─── Test 11: Numeric projectId is resolved to a global ID ──────────
//
// When `projectId` in the config is a numeric project number (e.g. `4`)
// rather than a relay global ID, `setup_project` must resolve it via
// `get_project_by_number` before using it in `node(id:)` queries.
//
// Mocks:
//   - `projectV2(number:`  → returns global ID "PVT-resolved" for project #4
//   - `field(name:`        → returns Status field with all 7 options
//   - `updateProjectV2Field` → should NOT be called (all options present)
//   - `fields(first:`      → returns Status + sessionId fields
//   - `createProjectV2Field` → should NOT be called (sessionId exists)

#[tokio::test]
async fn test_setup_resolves_numeric_project_id_to_global_id() {
    let gh_mock = MockServer::start().await;

    // 1. get_project_by_number → resolve numeric "4" to global ID
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("projectV2(number:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "user": { "projectV2": { "id": "PVT-resolved" } },
                "organization": null
            }
        })))
        .expect(1)
        .named("resolve_project_number")
        .mount(&gh_mock)
        .await;

    // 2. ensure_status_options: Status field with all 7 options
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("field(name:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "field": {
                        "id": "status-field-id",
                        "options": [
                            {"id": "o1", "name": "Triage"},
                            {"id": "o2", "name": "Todo"},
                            {"id": "o3", "name": "In Development"},
                            {"id": "o4", "name": "Review Technical"},
                            {"id": "o5", "name": "Review Product"},
                            {"id": "o6", "name": "QA"},
                            {"id": "o7", "name": "Done"},
                        ]
                    }
                }
            }
        })))
        .expect(1)
        .named("get_status_field")
        .mount(&gh_mock)
        .await;

    // 3. updateProjectV2Field should NOT be called (all options present)
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("updateProjectV2Field"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .named("add_status_options_should_not_be_called")
        .mount(&gh_mock)
        .await;

    // 4. ensure_session_id_field: project fields include sessionId
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("fields(first:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "fields": {
                        "nodes": [
                            {"id": "f1", "name": "Status", "dataType": "SINGLE_SELECT"},
                            {"id": "f2", "name": "sessionId", "dataType": "TEXT"},
                        ]
                    }
                }
            }
        })))
        .expect(1)
        .named("get_project_fields")
        .mount(&gh_mock)
        .await;

    // 5. createProjectV2Field should NOT be called (sessionId exists)
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("createProjectV2Field"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .named("add_session_id_field_should_not_be_called")
        .mount(&gh_mock)
        .await;

    // 6. createProjectV2 should NOT be called (project_id is set)
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("createProjectV2(input"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .named("create_project_v2_should_not_be_called")
        .mount(&gh_mock)
        .await;

    // 7. get_owner_id should NOT be called (project_id is set, no creation)
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("user(login:"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .named("get_owner_id_should_not_be_called")
        .mount(&gh_mock)
        .await;

    // ── Arrange: config with numeric projectId ────────────────────
    let tmp = tempdir().expect("tempdir should succeed");
    let config_path = tmp.path().join("git-automate.yml");
    let yaml = r#"
git:
  repository: "https://github.com/owner/repo"
  projectId: 4
  titlePattern: "@ai.*"
"#;
    std::fs::write(&config_path, yaml).expect("write should succeed");

    let config = parse_config(&config_path).expect("parse_config should succeed");
    assert_eq!(
        config.git.project_id.as_deref(),
        Some("4"),
        "project should have numeric projectId '4'"
    );

    // ── Act ────────────────────────────────────────────────────────
    let client = gh_client(&gh_mock);
    let deps = WorkflowContext {
        config,
        github: Some(client),
        shell: mock_shell(),
    };

    // write_project_id writes to git-automate.yml in cwd, so chdir to temp dir.
    let _guard = SET_CWD_MUTEX.lock().await;
    let original_dir = std::env::current_dir().expect("current_dir should succeed");
    std::env::set_current_dir(tmp.path()).expect("set_current_dir should succeed");

    let workflow = Workflow::new(deps);
    let result = workflow.run_setup_check().await;

    std::env::set_current_dir(&original_dir).expect("restore cwd should succeed");

    // ── Assert ────────────────────────────────────────────────────
    assert!(
        result.is_ok(),
        "run_setup_check should succeed with numeric projectId"
    );

    gh_mock.verify().await;

    // Numeric projectId should NOT be persisted — the original numeric value
    // is kept in the config file so users can keep `projectId: 4` and have
    // it resolved at runtime each time.
    let written = std::fs::read_to_string(&config_path).expect("read should succeed");
    assert!(
        written.contains("projectId: 4"),
        "config should still have original numeric projectId: 4, got: {written}"
    );
    assert!(
        !written.contains("PVT-resolved"),
        "resolved global ID should NOT be written to config, got: {written}"
    );
}

// ─── Test 12: Doctor does NOT persist resolved projectId ────────────
//
// When running `git-automate doctor`, the numeric projectId should be
// resolved in-memory (so status options and sessionId checks still run),
// but the resolved global ID must NOT be written back to `git-automate.yml`.
//
// Mocks:
//   - `projectV2(number:`  → returns global ID "PVT-resolved" for project #4
//   - `field(name:`        → returns Status field with all 7 options
//   - `fields(first:`      → returns Status + sessionId fields
//   - `createProjectV2Field` / `updateProjectV2Field` → should NOT be called

#[tokio::test]
async fn test_doctor_does_not_persist_resolved_project_id() {
    let gh_mock = MockServer::start().await;

    // 1. get_project_by_number → resolve numeric "4" to global ID
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("projectV2(number:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "user": { "projectV2": { "id": "PVT-resolved" } },
                "organization": null
            }
        })))
        .expect(1)
        .named("resolve_project_number")
        .mount(&gh_mock)
        .await;

    // 2. ensure_status_options: Status field with all 7 options
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("field(name:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "field": {
                        "id": "status-field-id",
                        "options": [
                            {"id": "o1", "name": "Triage"},
                            {"id": "o2", "name": "Todo"},
                            {"id": "o3", "name": "In Development"},
                            {"id": "o4", "name": "Review Technical"},
                            {"id": "o5", "name": "Review Product"},
                            {"id": "o6", "name": "QA"},
                            {"id": "o7", "name": "Done"},
                        ]
                    }
                }
            }
        })))
        .expect(1)
        .named("get_status_field")
        .mount(&gh_mock)
        .await;

    // 3. updateProjectV2Field should NOT be called (all options present)
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("updateProjectV2Field"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .named("add_status_options_should_not_be_called")
        .mount(&gh_mock)
        .await;

    // 4. ensure_session_id_field: project fields include sessionId
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("fields(first:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "fields": {
                        "nodes": [
                            {"id": "f1", "name": "Status", "dataType": "SINGLE_SELECT"},
                            {"id": "f2", "name": "sessionId", "dataType": "TEXT"},
                        ]
                    }
                }
            }
        })))
        .expect(1)
        .named("get_project_fields")
        .mount(&gh_mock)
        .await;

    // 5. createProjectV2Field should NOT be called (sessionId exists)
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("createProjectV2Field"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .named("add_session_id_field_should_not_be_called")
        .mount(&gh_mock)
        .await;

    // 6. createProjectV2 should NOT be called (project_id is set)
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("createProjectV2(input"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .named("create_project_v2_should_not_be_called")
        .mount(&gh_mock)
        .await;

    // ── Arrange: config with numeric projectId ────────────────────
    let tmp = tempdir().expect("tempdir should succeed");
    let config_path = tmp.path().join("git-automate.yml");
    let yaml = r#"
git:
  repository: "https://github.com/owner/repo"
  projectId: 4
  titlePattern: "@ai.*"
"#;
    std::fs::write(&config_path, yaml).expect("write should succeed");

    let config = parse_config(&config_path).expect("parse_config should succeed");
    assert_eq!(
        config.git.project_id.as_deref(),
        Some("4"),
        "project should have numeric projectId '4'"
    );

    // ── Act ────────────────────────────────────────────────────────
    let client = gh_client(&gh_mock);
    let deps = WorkflowContext {
        config,
        github: Some(client),
        shell: mock_shell(),
    };

    // write_project_id writes to git-automate.yml in cwd, so chdir to temp dir.
    let _guard = SET_CWD_MUTEX.lock().await;
    let original_dir = std::env::current_dir().expect("current_dir should succeed");
    std::env::set_current_dir(tmp.path()).expect("set_current_dir should succeed");

    let workflow = Workflow::new(deps);
    let result = workflow.run_doctor_check().await;

    std::env::set_current_dir(&original_dir).expect("restore cwd should succeed");

    // ── Assert ────────────────────────────────────────────────────
    assert!(
        result.is_ok(),
        "run_doctor_check should succeed with numeric projectId"
    );

    gh_mock.verify().await;

    // Verify the config file was NOT modified — projectId should still be
    // the original numeric value `4`, not the resolved global ID "PVT-resolved".
    let written = std::fs::read_to_string(&config_path).expect("read should succeed");
    let data: serde_yaml::Value =
        serde_yaml::from_str(&written).expect("yaml parse should succeed");
    let pid = data
        .get("git")
        .and_then(|g| g.get("projectId"))
        .expect("projectId should still exist in YAML after doctor");

    // The value should be the original number/string "4", NOT "PVT-resolved".
    assert_ne!(
        pid.as_str(),
        Some("PVT-resolved"),
        "projectId in YAML should NOT be overwritten with the resolved global ID during doctor"
    );
    // Verify it's still the original numeric value (either as integer or string).
    assert!(
        pid.as_i64() == Some(4) || pid.as_str() == Some("4"),
        "projectId in YAML should still be the original value '4', got: {:?}",
        pid
    );
}

// ─── Failed Review Flow Integration Tests ──────────────────────

async fn mount_failed_review_github_mocks(
    server: &MockServer,
    review_state: &str,
    session_id: &str,
) {
    mount_status_field_mock(server).await;
    mount_project_fields_mock(server).await;
    mount_add_item_mock(server).await;
    mount_update_field_mock(server).await;

    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("items(first:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "items": {
                        "nodes": [
                            {
                                "id": "item-42",
                                "content": {
                                    "__typename": "Issue",
                                    "id": "issue-node-42",
                                    "number": 42
                                }
                            }
                        ]
                    }
                }
            }
        })))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("fieldValues(first:"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "fieldValues": {
                        "nodes": [
                            {"__typename": "ProjectV2ItemFieldSingleSelectValue", "name": review_state, "field": {"name": "Status"}},
                            {"__typename": "ProjectV2ItemFieldTextValue", "text": session_id, "field": {"name": "sessionId"}}
                        ]
                    }
                }
            }
        })))
        .mount(server)
        .await;
}

fn failed_review_test_ctx(
    _review_state: &str,
    _session_id: &str,
) -> (WorkflowContext, ProjectContext, OpencodeSessionConfig) {
    let project_config = GitSection {
        repository: "https://github.com/owner/repo".to_string(),
        project_id: Some("PID-123".to_string()),
        directory: None,
        issue_provider: "github".to_string(),
        title_pattern: "@ai.*".to_string(),
        trello_api_key: None,
        trello_token: None,
        trello_board_id: None,
    };

    let deps = WorkflowContext {
        config: GitAutomateConfig {
            git: project_config.clone(),
            concurrency: None,
            github_token: None,
            opencode: Some(OpencodeConfig {
                url: "http://localhost:8081".to_string(),
                pw: "pw".to_string(),
            }),
        },
        github: None,
        shell: mock_shell(),
    };

    let ctx = ProjectContext {
        name: "test-proj".to_string(),
        config: project_config,
        owner: "owner".to_string(),
        repo: "repo".to_string(),
        project_id: "PID-123".to_string(),
    };

    let oc = OpencodeSessionConfig {
        url: "http://localhost:8081".to_string(),
        pw: "pw".to_string(),
        directory: None,
    };

    (deps, ctx, oc)
}

#[tokio::test]
async fn test_failed_review_technical_transitions_to_in_dev() {
    let gh_mock = MockServer::start().await;
    let oc_mock = MockServer::start().await;

    mount_failed_review_github_mocks(&gh_mock, "Review Technical", "old-review-session").await;

    Mock::given(method("GET"))
        .and(path("/session/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&oc_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "dev-sess-999",
            "projectID": "p1",
            "directory": "/d",
            "title": "Review Technical: Fix login",
            "version": "1",
            "time": {"created": 1, "updated": 2}
        })))
        .expect(1)
        .mount(&oc_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/session/dev-sess-999/prompt_async"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&oc_mock)
        .await;

    let client = gh_client(&gh_mock);
    let (mut deps, ctx, mut oc) = failed_review_test_ctx("Review Technical", "old-review-session");
    deps.github = Some(client.clone());
    oc.url = oc_mock.uri();
    let result = run_review_check(&deps, &ctx, &oc).await;
    assert!(
        result.is_ok(),
        "run_review_check failed: {:?}",
        result.err()
    );
    oc_mock.verify().await;
}

#[tokio::test]
async fn test_failed_review_product_transitions_to_in_dev() {
    let gh_mock = MockServer::start().await;
    let oc_mock = MockServer::start().await;

    mount_failed_review_github_mocks(&gh_mock, "Review Product", "old-review-session").await;

    Mock::given(method("GET"))
        .and(path("/session/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&oc_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "dev-sess-999",
            "projectID": "p1",
            "directory": "/d",
            "title": "Review Product: Fix",
            "version": "1",
            "time": {"created": 1, "updated": 2}
        })))
        .expect(1)
        .mount(&oc_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/session/dev-sess-999/prompt_async"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&oc_mock)
        .await;

    let client = gh_client(&gh_mock);
    let (mut deps, ctx, mut oc) = failed_review_test_ctx("Review Product", "old-review-session");
    deps.github = Some(client.clone());
    oc.url = oc_mock.uri();
    let result = run_review_check(&deps, &ctx, &oc).await;
    assert!(
        result.is_ok(),
        "run_review_check failed: {:?}",
        result.err()
    );
    oc_mock.verify().await;
}

#[tokio::test]
async fn test_failed_review_qa_transitions_to_in_dev() {
    let gh_mock = MockServer::start().await;
    let oc_mock = MockServer::start().await;

    mount_failed_review_github_mocks(&gh_mock, "QA", "old-review-session").await;

    Mock::given(method("GET"))
        .and(path("/session/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&oc_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "dev-sess-999",
            "projectID": "p1",
            "directory": "/d",
            "title": "QA: Test integration",
            "version": "1",
            "time": {"created": 1, "updated": 2}
        })))
        .expect(1)
        .mount(&oc_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/session/dev-sess-999/prompt_async"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&oc_mock)
        .await;

    let client = gh_client(&gh_mock);
    let (mut deps, ctx, mut oc) = failed_review_test_ctx("QA", "old-review-session");
    deps.github = Some(client.clone());
    oc.url = oc_mock.uri();
    let result = run_review_check(&deps, &ctx, &oc).await;
    assert!(
        result.is_ok(),
        "run_review_check failed: {:?}",
        result.err()
    );
    oc_mock.verify().await;
}

#[tokio::test]
async fn test_active_review_session_not_recovered() {
    let gh_mock = MockServer::start().await;
    let oc_mock = MockServer::start().await;

    mount_failed_review_github_mocks(&gh_mock, "Review Technical", "active-session").await;

    Mock::given(method("GET"))
        .and(path("/session/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "active-session": {"type": "busy"}
        })))
        .mount(&oc_mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "should-not-happen",
            "projectID": "p", "directory": "/d", "title": "t",
            "version": "1", "time": {"created": 1, "updated": 2}
        })))
        .expect(0)
        .mount(&oc_mock)
        .await;

    let client = gh_client(&gh_mock);
    let (mut deps, ctx, mut oc) = failed_review_test_ctx("Review Technical", "active-session");
    deps.github = Some(client.clone());
    oc.url = oc_mock.uri();
    let result = run_review_check(&deps, &ctx, &oc).await;
    assert!(
        result.is_ok(),
        "run_review_check failed: {:?}",
        result.err()
    );
    oc_mock.verify().await;
}
