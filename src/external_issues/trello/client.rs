//! Trello REST API client.
//!
//! Authentication uses an API key and token passed as query parameters
//! (`?key=...&token=...`) on every request, per the Trello REST API specification.
//! Base URL: `https://api.trello.com/1/`.

use reqwest::Client;
use serde::de::DeserializeOwned;
use serde_json::Value;
use thiserror::Error;

use super::types::{TrelloBoard, TrelloCard, TrelloList};

// ─── Error type ───────────────────────────────────────────────

/// Errors that can occur while communicating with the Trello REST API.
#[derive(Debug, Error)]
pub enum TrelloError {
    #[error("Trello API key is required")]
    EmptyApiKey,
    #[error("Trello API token is required")]
    EmptyToken,
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("HTTP status {0}")]
    HttpStatus(u16),
    #[error("Failed to parse Trello response: {0}")]
    Parse(String),
    #[error("Board not found: {0}")]
    BoardNotFound(String),
    #[error("List not found: {0}")]
    ListNotFound(String),
    #[error("Card not found: {0}")]
    CardNotFound(String),
    #[error("{0}")]
    Other(String),
}

impl From<serde_json::Error> for TrelloError {
    fn from(e: serde_json::Error) -> Self {
        TrelloError::Parse(e.to_string())
    }
}

// ─── Client ───────────────────────────────────────────────────

/// A minimal HTTP client for the Trello REST API.
///
/// Uses API key + token authentication via query parameters. All requests
/// go to `{base_url}/{endpoint}` with `?key={api_key}&token={api_token}`.
///
/// The base URL defaults to `https://api.trello.com/1` but can be overridden
/// via [`TrelloClient::new_with_base_url`] for testing.
#[derive(Clone)]
pub struct TrelloClient {
    client: Client,
    api_key: String,
    api_token: String,
    base_url: String,
}

impl TrelloClient {
    /// Create a new client targeting the real Trello API.
    ///
    /// # Errors
    /// Returns [`TrelloError::EmptyApiKey`] if `api_key` is empty.
    /// Returns [`TrelloError::EmptyToken`] if `api_token` is empty.
    pub fn new(api_key: String, api_token: String) -> Result<Self, TrelloError> {
        if api_key.is_empty() {
            return Err(TrelloError::EmptyApiKey);
        }
        if api_token.is_empty() {
            return Err(TrelloError::EmptyToken);
        }
        Ok(Self {
            client: Client::new(),
            api_key,
            api_token,
            base_url: "https://api.trello.com/1".to_string(),
        })
    }

    /// Create a client targeting a custom base URL (for testing/integration).
    ///
    /// # Errors
    /// Returns [`TrelloError::EmptyApiKey`] if `api_key` is empty.
    /// Returns [`TrelloError::EmptyToken`] if `api_token` is empty.
    pub fn new_with_base_url(
        api_key: String,
        api_token: String,
        base_url: String,
    ) -> Result<Self, TrelloError> {
        if api_key.is_empty() {
            return Err(TrelloError::EmptyApiKey);
        }
        if api_token.is_empty() {
            return Err(TrelloError::EmptyToken);
        }
        Ok(Self {
            client: Client::new(),
            api_key,
            api_token,
            base_url,
        })
    }

    // ── Core transport ──────────────────────────────────────────

    /// Build the authentication query parameters (`key`, `token`).
    fn auth_params(&self) -> Vec<(&str, &str)> {
        vec![("key", &self.api_key), ("token", &self.api_token)]
    }

    /// GET `{base_url}/{path}` with auth params, deserialised into `T`.
    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, TrelloError> {
        let response = self
            .client
            .get(format!("{}/{}", self.base_url, path))
            .query(&self.auth_params())
            .send()
            .await?;
        self.handle_response(response).await
    }

    /// POST `{base_url}/{path}` with auth + extra query params, deserialised into `T`.
    async fn post_json<T: DeserializeOwned>(
        &self,
        path: &str,
        extra_params: &[(&str, &str)],
    ) -> Result<T, TrelloError> {
        let response = self
            .client
            .post(format!("{}/{}", self.base_url, path))
            .query(&self.auth_params())
            .query(extra_params)
            .send()
            .await?;
        self.handle_response(response).await
    }

    /// PUT `{base_url}/{path}` with auth + extra query params, deserialised into `T`.
    async fn put_json<T: DeserializeOwned>(
        &self,
        path: &str,
        extra_params: &[(&str, &str)],
    ) -> Result<T, TrelloError> {
        let response = self
            .client
            .put(format!("{}/{}", self.base_url, path))
            .query(&self.auth_params())
            .query(extra_params)
            .send()
            .await?;
        self.handle_response(response).await
    }

    /// Check response status and parse JSON body.
    async fn handle_response<T: DeserializeOwned>(
        &self,
        response: reqwest::Response,
    ) -> Result<T, TrelloError> {
        let status = response.status();
        if !status.is_success() {
            // Try to extract error message from body
            let text = response.text().await.unwrap_or_default();
            if text.is_empty() {
                return Err(TrelloError::HttpStatus(status.as_u16()));
            }
            return Err(TrelloError::Other(format!(
                "HTTP {}: {}",
                status.as_u16(),
                text
            )));
        }
        response.json::<T>().await.map_err(TrelloError::from)
    }

    // ── Boards ─────────────────────────────────────────────────

    /// `GET /members/me/boards` — list boards for the authenticated user.
    pub async fn list_boards(&self) -> Result<Vec<TrelloBoard>, TrelloError> {
        self.get_json("members/me/boards").await
    }

    /// `GET /boards/{id}` — get a single board.
    pub async fn get_board(&self, board_id: &str) -> Result<TrelloBoard, TrelloError> {
        self.get_json(&format!("boards/{}", board_id)).await
    }

    // ── Lists ──────────────────────────────────────────────────

    /// `GET /boards/{id}/lists` — list all lists on a board.
    pub async fn list_lists(&self, board_id: &str) -> Result<Vec<TrelloList>, TrelloError> {
        self.get_json(&format!("boards/{}/lists", board_id)).await
    }

    // ── Cards ──────────────────────────────────────────────────

    /// `GET /boards/{id}/cards` — list all cards on a board.
    pub async fn list_cards(&self, board_id: &str) -> Result<Vec<TrelloCard>, TrelloError> {
        self.get_json(&format!("boards/{}/cards", board_id)).await
    }

    /// `GET /cards/{id}` — get a single card by ID.
    pub async fn get_card(&self, card_id: &str) -> Result<TrelloCard, TrelloError> {
        self.get_json(&format!("cards/{}", card_id)).await
    }

    /// `POST /cards` — create a new card.
    ///
    /// * `id_list` — The list to add the card to (required).
    /// * `name` — Card title (required).
    /// * `desc` — Card description (optional, may be empty).
    pub async fn create_card(
        &self,
        id_list: &str,
        name: &str,
        desc: &str,
    ) -> Result<TrelloCard, TrelloError> {
        self.post_json(
            "cards",
            &[("idList", id_list), ("name", name), ("desc", desc)],
        )
        .await
    }

    /// `PUT /cards/{id}` — move a card to a different list.
    pub async fn move_card_to_list(
        &self,
        card_id: &str,
        id_list: &str,
    ) -> Result<TrelloCard, TrelloError> {
        self.put_json(&format!("cards/{}", card_id), &[("idList", id_list)])
            .await
    }

    /// `POST /cards/{id}/actions/comments` — add a comment to a card.
    pub async fn add_comment_to_card(&self, card_id: &str, text: &str) -> Result<(), TrelloError> {
        let _: Value = self
            .post_json(
                &format!("cards/{}/actions/comments", card_id),
                &[("text", text)],
            )
            .await?;
        Ok(())
    }
}

// ─── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Build a TrelloClient pointing at a mock server with test credentials.
    fn tl_client(mock: &MockServer) -> TrelloClient {
        TrelloClient::new_with_base_url(
            "test-key".to_string(),
            "test-token".to_string(),
            mock.uri(),
        )
        .unwrap()
    }

    // ── Constructor tests ──

    #[test]
    fn new_valid_key_and_token_succeeds() {
        let result = TrelloClient::new("key".to_string(), "token".to_string());
        assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
    }

    #[test]
    fn new_empty_api_key_returns_error() {
        let result = TrelloClient::new(String::new(), "token".to_string());
        assert!(matches!(result, Err(TrelloError::EmptyApiKey)));
    }

    #[test]
    fn new_empty_token_returns_error() {
        let result = TrelloClient::new("key".to_string(), String::new());
        assert!(matches!(result, Err(TrelloError::EmptyToken)));
    }

    #[test]
    fn new_with_base_url_uses_custom_url() {
        let client = TrelloClient::new_with_base_url(
            "key".to_string(),
            "token".to_string(),
            "http://localhost:9999".to_string(),
        )
        .unwrap();
        assert_eq!(client.base_url, "http://localhost:9999");
    }

    #[test]
    fn new_with_base_url_empty_key_returns_error() {
        let result = TrelloClient::new_with_base_url(
            String::new(),
            "token".to_string(),
            "http://localhost:9999".to_string(),
        );
        assert!(matches!(result, Err(TrelloError::EmptyApiKey)));
    }

    // ── list_lists wiremock tests ──

    #[tokio::test]
    async fn list_lists_makes_get_request_and_parses_response() {
        let mock = MockServer::start().await;
        let client = tl_client(&mock);

        let boards_lists_json = json!([
            {
                "id": "lst1",
                "name": "Triage",
                "closed": false,
                "idBoard": "brd1",
                "pos": 16384
            },
            {
                "id": "lst2",
                "name": "Todo",
                "closed": false,
                "idBoard": "brd1",
                "pos": 32768
            }
        ]);

        Mock::given(method("GET"))
            .and(path("/boards/brd1/lists"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&boards_lists_json))
            .mount(&mock)
            .await;

        let lists = client.list_lists("brd1").await.unwrap();
        assert_eq!(lists.len(), 2);
        assert_eq!(lists[0].id, "lst1");
        assert_eq!(lists[0].name, "Triage");
        assert_eq!(lists[1].name, "Todo");
    }

    #[tokio::test]
    async fn list_lists_includes_auth_params() {
        let mock = MockServer::start().await;
        let client = tl_client(&mock);

        Mock::given(method("GET"))
            .and(path("/boards/brd1/lists"))
            .and(query_param("key", "test-key"))
            .and(query_param("token", "test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&mock)
            .await;

        let lists = client.list_lists("brd1").await.unwrap();
        assert!(lists.is_empty());
    }

    // ── list_cards wiremock tests ──

    #[tokio::test]
    async fn list_cards_makes_get_request_and_parses_response() {
        let mock = MockServer::start().await;
        let client = tl_client(&mock);

        let cards_json = json!([
            {
                "id": "card1",
                "name": "@ai Fix bug",
                "desc": "bug body",
                "idList": "lst1",
                "idBoard": "brd1",
                "idShort": 42,
                "closed": false,
                "due": null,
                "dueComplete": false
            }
        ]);

        Mock::given(method("GET"))
            .and(path("/boards/brd1/cards"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&cards_json))
            .mount(&mock)
            .await;

        let cards = client.list_cards("brd1").await.unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].id, "card1");
        assert_eq!(cards[0].name, "@ai Fix bug");
        assert_eq!(cards[0].desc, "bug body");
        assert_eq!(cards[0].id_list, "lst1");
    }

    #[tokio::test]
    async fn list_cards_empty_response() {
        let mock = MockServer::start().await;
        let client = tl_client(&mock);

        Mock::given(method("GET"))
            .and(path("/boards/brd1/cards"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&mock)
            .await;

        let cards = client.list_cards("brd1").await.unwrap();
        assert!(cards.is_empty());
    }

    // ── Error handling tests ──

    #[tokio::test]
    async fn get_card_http_404_returns_error() {
        let mock = MockServer::start().await;
        let client = tl_client(&mock);

        Mock::given(method("GET"))
            .and(path("/cards/nonexistent"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock)
            .await;

        let result = client.get_card("nonexistent").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("404"));
    }
}
