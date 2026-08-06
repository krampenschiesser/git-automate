//! Trello issue source — implements [`ExternalIssueSource`] for Trello boards.
//!
//! Maps Trello concepts to the canonical [`ExternalIssue`] shape:
//! - Trello *list* name → `ExternalIssue.status` (workflow status)
//! - Trello *card* name → `ExternalIssue.title`
//! - Trello *card* desc → `ExternalIssue.content`
//! - Trello *card* id → `ExternalIssue.external_id`
//!
//! Trello has no native session or sub-task concepts, so `session_id`
//! is always empty and `sub_task_external_ids` is always `vec![]`.

use std::pin::Pin;

use regex::Regex;

use super::client::TrelloClient;
use crate::external_issues::common::{ExternalIssue, ExternalIssueError, ExternalIssueSource};

/// Issues source backed by Trello boards.
///
/// Fetches cards from a single Trello board, treating each list's name as a
/// workflow status. Only cards whose titles match `title_pattern` are returned
/// (matching the `@ai.*` convention used by the GitHub source).
pub struct TrelloIssueSource {
    /// Trello REST API client (reqwest-based).
    client: TrelloClient,
    /// The Trello board ID to monitor.
    board_id: String,
    /// Compiled regex for filtering card titles (e.g. `@ai.*`).
    title_pattern: Regex,
}

impl TrelloIssueSource {
    /// Create a new Trello issue source.
    ///
    /// * `client` — A configured [`TrelloClient`].
    /// * `board_id` — The Trello board to monitor for issues.
    /// * `title_pattern` — Regex string to match card titles (e.g. `"@ai.*"`).
    ///
    /// # Errors
    /// Returns `TrelloError::Other` if `title_pattern` is not a valid regex.
    pub fn new(
        client: TrelloClient,
        board_id: String,
        title_pattern: &str,
    ) -> Result<Self, super::client::TrelloError> {
        let regex = Regex::new(title_pattern).map_err(|e| {
            super::client::TrelloError::Other(format!(
                "invalid title_pattern '{}': {}",
                title_pattern, e
            ))
        })?;
        Ok(Self {
            client,
            board_id,
            title_pattern: regex,
        })
    }
}

impl ExternalIssueSource for TrelloIssueSource {
    fn fetch_issues_by_state<'a>(
        &'a self,
        state: &'a str,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<Vec<ExternalIssue>, ExternalIssueError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            // ── 1. List all lists on the board ──────────────
            let lists = self.client.list_lists(&self.board_id).await?;

            // ── 2. Find the list whose name matches the requested state ──
            let target_list = lists.iter().find(|l| l.name == state && !l.closed);

            let Some(target_list) = target_list else {
                // No list matching the requested state — return empty.
                tracing::info!(
                    "Trello board {}: no list found for state '{}'",
                    self.board_id,
                    state
                );
                return Ok(vec![]);
            };

            // ── 3. List all cards on the board ──────────────
            let cards = self.client.list_cards(&self.board_id).await?;

            // ── 4. Filter: not closed + in target list + title matches pattern ──
            let matching: Vec<&super::types::TrelloCard> = cards
                .iter()
                .filter(|c| !c.closed && c.id_list == target_list.id)
                .filter(|c| self.title_pattern.is_match(&c.name))
                .collect();

            if matching.is_empty() {
                tracing::info!(
                    "Trello board {}: no matching cards in list '{}' ({})",
                    self.board_id,
                    target_list.name,
                    state
                );
            }

            // ── 5. Map to canonical ExternalIssue ────────────
            let result: Vec<ExternalIssue> = matching
                .iter()
                .map(|c| ExternalIssue {
                    session_id: String::new(),
                    status: state.to_string(),
                    title: c.name.clone(),
                    content: c.desc.clone(),
                    external_id: c.id.clone(),
                    sub_task_external_ids: vec![],
                })
                .collect();

            Ok(result)
        })
    }
}

// ─── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Build a TrelloIssueSource pointing at a mock server with default pattern `@ai.*`.
    fn tl_source(mock: &MockServer) -> TrelloIssueSource {
        let client = TrelloClient::new_with_base_url(
            "test-key".to_string(),
            "test-token".to_string(),
            mock.uri(),
        )
        .unwrap();
        TrelloIssueSource::new(client, "brd1".to_string(), "@ai.*").unwrap()
    }

    // ── Constructor tests ──

    #[test]
    fn new_valid_regex_succeeds() {
        let client = TrelloClient::new("key".to_string(), "token".to_string()).unwrap();
        let result = TrelloIssueSource::new(client, "brd1".to_string(), "@ai.*");
        assert!(result.is_ok());
    }

    #[test]
    fn new_invalid_regex_returns_error() {
        let client = TrelloClient::new("key".to_string(), "token".to_string()).unwrap();
        let result = TrelloIssueSource::new(client, "brd1".to_string(), "[invalid");
        match result {
            Err(e) => assert!(e.to_string().contains("invalid title_pattern")),
            Ok(_) => panic!("expected error from invalid regex, got Ok"),
        }
    }

    // ── Wiremock tests ──

    #[tokio::test]
    async fn returns_matching_issues() {
        let mock = MockServer::start().await;
        let source = tl_source(&mock);

        // Board has two lists: "Triage" and "Todo"
        let lists_json = json!([
            {"id": "lst-triage", "name": "Triage", "closed": false, "idBoard": "brd1"},
            {"id": "lst-todo", "name": "Todo", "closed": false, "idBoard": "brd1"}
        ]);

        // Card #1: in "Triage" list, @ai prefix → should match
        // Card #2: in "Triage" list, no @ai prefix → should be filtered
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

        let result = source.fetch_issues_by_state("Triage").await;
        assert!(result.is_ok(), "fetch should succeed: {:?}", result.err());
        let issues = result.unwrap();

        // Only card #1 (@ai Fix bug) should be returned
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].external_id, "card1");
        assert_eq!(issues[0].title, "@ai Fix bug");
        assert_eq!(issues[0].content, "bug body");
        assert_eq!(issues[0].status, "Triage");
        assert!(issues[0].session_id.is_empty());
        assert!(issues[0].sub_task_external_ids.is_empty());
    }

    #[tokio::test]
    async fn filters_other_lists() {
        let mock = MockServer::start().await;
        let source = tl_source(&mock);

        let lists_json = json!([
            {"id": "lst-triage", "name": "Triage", "closed": false, "idBoard": "brd1"},
            {"id": "lst-todo", "name": "Todo", "closed": false, "idBoard": "brd1"}
        ]);

        // Card is in "Todo" list, but we're fetching "Triage" → should not match
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

        let result = source.fetch_issues_by_state("Triage").await.unwrap();
        assert!(result.is_empty(), "expected no issues, got {:?}", result);
    }

    #[tokio::test]
    async fn filters_by_title_pattern() {
        let mock = MockServer::start().await;
        let source = tl_source(&mock);

        let lists_json = json!([
            {"id": "lst-triage", "name": "Triage", "closed": false, "idBoard": "brd1"}
        ]);

        // Card in "Triage" list but title doesn't match "@ai.*" → filtered out
        let cards_json = json!([
            {
                "id": "card1",
                "name": "Regular task",
                "desc": "not an ai task",
                "idList": "lst-triage",
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

        let result = source.fetch_issues_by_state("Triage").await.unwrap();
        assert!(result.is_empty(), "expected no issues, got {:?}", result);
    }

    #[tokio::test]
    async fn empty_when_no_matching_list() {
        let mock = MockServer::start().await;
        let source = tl_source(&mock);

        // Board has "Todo" and "Done" lists, but no "Triage" list
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

        // Fetching "Triage" when no "Triage" list exists → empty
        let result = source.fetch_issues_by_state("Triage").await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn filters_closed_cards() {
        let mock = MockServer::start().await;
        let source = tl_source(&mock);

        let lists_json = json!([
            {"id": "lst-triage", "name": "Triage", "closed": false, "idBoard": "brd1"}
        ]);

        // Card #1: open, matches → returned
        // Card #2: closed, matches pattern but should be filtered
        let cards_json = json!([
            {
                "id": "card1",
                "name": "@ai Fix bug",
                "desc": "bug body",
                "idList": "lst-triage",
                "idBoard": "brd1",
                "idShort": 1,
                "closed": false,
                "due": null,
                "dueComplete": false
            },
            {
                "id": "card2",
                "name": "@ai Archive me",
                "desc": "archived",
                "idList": "lst-triage",
                "idBoard": "brd1",
                "idShort": 2,
                "closed": true,
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

        let result = source.fetch_issues_by_state("Triage").await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].external_id, "card1");
    }

    #[tokio::test]
    async fn api_error_propagates_as_external_issue_error() {
        let mock = MockServer::start().await;
        let source = tl_source(&mock);

        Mock::given(method("GET"))
            .and(path("/boards/brd1/lists"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&mock)
            .await;

        let result = source.fetch_issues_by_state("Triage").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("401"));
    }
}
