//! TrelloIssueSource integration tests via mock Trello API.
//!
//! Tests 11 and 12 from the original `integration.rs`:
//! - fetch_issues_by_state with title pattern filtering
//! - empty result when no matching list exists

mod common;

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use git_automate::external_issues::common::ExternalIssueSource;
use git_automate::external_issues::trello::source::TrelloIssueSource;

use common::tl_client;

// ─── Test 11: TrelloIssueSource fetch by state via mock Trello API ───
//
// End-to-end test of the Trello issue source using wiremock to mock the
// Trello REST API. Mirrors the GitHub issue source integration tests
// (tests 6 & 7) but for Trello's REST endpoints:
//   GET /boards/{id}/lists → list of lists (status columns)
//   GET /boards/{id}/cards → list of cards (issues)
//
// Verifies: list mapping → state filtering → title pattern filtering →
// ExternalIssue field mapping.

#[tokio::test]
async fn test_trello_issue_source_fetch_by_state() {
    let mock = MockServer::start().await;

    // Board has two lists: "Triage" and "Todo"
    let lists_json = json!([
        {"id": "lst-triage", "name": "Triage", "closed": false, "idBoard": "brd1"},
        {"id": "lst-todo", "name": "Todo", "closed": false, "idBoard": "brd1"}
    ]);

    // Cards: one @ai in Triage (should match), one non-@ai in Triage (filtered),
    // one @ai in Todo (different list, filtered)
    let cards_json = json!([
        {
            "id": "card1",
            "name": "@ai Fix bug",
            "desc": "bug body",
            "idList": "lst-triage",
            "idBoard": "brd1",
            "idShort": 42,
            "closed": false,
            "due": null,
            "dueComplete": false
        },
        {
            "id": "card2",
            "name": "Regular task",
            "desc": "not an ai task",
            "idList": "lst-triage",
            "idBoard": "brd1",
            "idShort": 43,
            "closed": false,
            "due": null,
            "dueComplete": false
        },
        {
            "id": "card3",
            "name": "@ai Todo task",
            "desc": "todo body",
            "idList": "lst-todo",
            "idBoard": "brd1",
            "idShort": 44,
            "closed": false,
            "due": null,
            "dueComplete": false
        }
    ]);

    Mock::given(method("GET"))
        .and(path("/boards/brd1/lists"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&lists_json))
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/boards/brd1/cards"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&cards_json))
        .mount(&mock)
        .await;

    let client = tl_client(&mock);
    let source = TrelloIssueSource::new(client, "brd1".to_string(), "@ai.*").unwrap();

    let result = source.fetch_issues_by_state("Triage").await;
    assert!(result.is_ok(), "fetch should succeed: {:?}", result.err());
    let issues = result.unwrap();

    // Only card1 (@ai Fix bug in Triage list) should be returned
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].external_id, "card1");
    assert_eq!(issues[0].title, "@ai Fix bug");
    assert_eq!(issues[0].content, "bug body");
    assert_eq!(issues[0].status, "Triage");
    assert!(issues[0].session_id.is_empty());
    assert!(issues[0].sub_task_external_ids.is_empty());
}

// ─── Test 12: TrelloIssueSource filters by state (empty when no matching list) ───

#[tokio::test]
async fn test_trello_issue_source_filters_by_state() {
    let mock = MockServer::start().await;

    // Board has only "Todo" and "Done" lists — no "Triage"
    let lists_json = json!([
        {"id": "lst-todo", "name": "Todo", "closed": false, "idBoard": "brd1"},
        {"id": "lst-done", "name": "Done", "closed": false, "idBoard": "brd1"}
    ]);

    let cards_json = json!([
        {
            "id": "card1",
            "name": "@ai Fix bug",
            "desc": "bug body",
            "idList": "lst-todo",
            "idBoard": "brd1",
            "idShort": 1,
            "closed": false,
            "due": null,
            "dueComplete": false
        }
    ]);

    Mock::given(method("GET"))
        .and(path("/boards/brd1/lists"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&lists_json))
        .mount(&mock)
        .await;

    Mock::given(method("GET"))
        .and(path("/boards/brd1/cards"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&cards_json))
        .mount(&mock)
        .await;

    let client = tl_client(&mock);
    let source = TrelloIssueSource::new(client, "brd1".to_string(), "@ai.*").unwrap();

    // Fetching "Triage" when no "Triage" list exists → empty
    let result = source.fetch_issues_by_state("Triage").await.unwrap();
    assert!(result.is_empty());
}
