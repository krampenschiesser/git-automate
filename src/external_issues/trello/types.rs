//! Trello API response types.
//!
//! Serde-compatible deserialisation structs for Trello REST API JSON responses.
//! Field names are renamed to `snake_case` to match Rust conventions while
//! deserialising Trello's `camelCase` / `lowercase` JSON keys.

use serde::Deserialize;

/// A Trello board.
///
/// Returned by `GET /boards/{id}` and `GET /members/me/boards`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TrelloBoard {
    /// Unique board ID (e.g. `"5abbe4b7ddc1b351ef961414"`).
    pub id: String,
    /// Board name.
    pub name: String,
    /// Whether the board is closed / archived.
    #[serde(default)]
    pub closed: bool,
    /// Board description (may be empty).
    #[serde(default)]
    pub desc: String,
    /// Board URL.
    #[serde(default, rename = "url")]
    pub url: String,
}

/// A Trello list (a column on a board, e.g. "Triage", "Todo", "Done").
///
/// Returned by `GET /boards/{id}/lists`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TrelloList {
    /// Unique list ID.
    pub id: String,
    /// List name — maps to workflow status (e.g. "Triage").
    pub name: String,
    /// Whether the list is closed / archived.
    #[serde(default)]
    pub closed: bool,
    /// List position within the board (numeric or string like "top").
    #[serde(default, rename = "pos")]
    pub pos: Option<serde_json::Value>,
    /// Board ID this list belongs to.
    #[serde(default, rename = "idBoard")]
    pub id_board: String,
}

/// A Trello card (an issue / task within a list).
///
/// Returned by `GET /boards/{id}/cards`, `GET /cards/{id}`, and
/// `POST /cards` (response).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TrelloCard {
    /// Unique card ID (long hex string).
    pub id: String,
    /// Card title / name.
    pub name: String,
    /// Card description / body.
    #[serde(default)]
    pub desc: String,
    /// ID of the list this card belongs to (maps to workflow status).
    #[serde(rename = "idList")]
    pub id_list: String,
    /// ID of the board this card belongs to.
    #[serde(rename = "idBoard")]
    pub id_board: String,
    /// Short numeric ID (integer as number).
    #[serde(rename = "idShort")]
    pub id_short: u32,
    /// Whether the card is closed / archived.
    #[serde(default)]
    pub closed: bool,
    /// Due date (ISO 8601 string, may be null).
    #[serde(default)]
    pub due: Option<String>,
    /// Whether the due date is marked complete.
    #[serde(default, rename = "dueComplete")]
    pub due_complete: bool,
}

// ─── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_list_with_all_fields() {
        let json = r#"{
                "id": "5abbe4b7ddc1b351ef961414",
                "name": "Triage",
                "closed": false,
                "pos": 16384,
                "idBoard": "5abbe4b7ddc1b351ef961415"
            }"#;
        let list: TrelloList = serde_json::from_str(json).unwrap();
        assert_eq!(list.id, "5abbe4b7ddc1b351ef961414");
        assert_eq!(list.name, "Triage");
        assert!(!list.closed);
        assert_eq!(list.id_board, "5abbe4b7ddc1b351ef961415");
    }

    #[test]
    fn deserialize_list_with_string_pos() {
        let json = r#"{
                "id": "lst1",
                "name": "Todo",
                "closed": true,
                "pos": "top",
                "idBoard": "brd1"
            }"#;
        let list: TrelloList = serde_json::from_str(json).unwrap();
        assert!(list.closed);
    }

    #[test]
    fn deserialize_card_with_all_fields() {
        let json = r#"{
                "id": "60c72b2f9f1b2c006f8e4a1c",
                "name": "@ai Fix bug",
                "desc": "Detailed description",
                "idList": "5abbe4b7ddc1b351ef961414",
                "idBoard": "5abbe4b7ddc1b351ef961415",
                "idShort": 42,
                "closed": false,
                "due": "2023-12-31T23:59:59.999Z",
                "dueComplete": false
            }"#;
        let card: TrelloCard = serde_json::from_str(json).unwrap();
        assert_eq!(card.id, "60c72b2f9f1b2c006f8e4a1c");
        assert_eq!(card.name, "@ai Fix bug");
        assert_eq!(card.desc, "Detailed description");
        assert_eq!(card.id_list, "5abbe4b7ddc1b351ef961414");
        assert_eq!(card.id_board, "5abbe4b7ddc1b351ef961415");
        assert_eq!(card.id_short, 42);
        assert!(!card.closed);
        assert_eq!(card.due, Some("2023-12-31T23:59:59.999Z".to_string()));
        assert!(!card.due_complete);
    }

    #[test]
    fn deserialize_card_with_null_due() {
        let json = r#"{
                "id": "card1",
                "name": "Task",
                "desc": "",
                "idList": "lst1",
                "idBoard": "brd1",
                "idShort": 1,
                "closed": false,
                "due": null,
                "dueComplete": false
            }"#;
        let card: TrelloCard = serde_json::from_str(json).unwrap();
        assert_eq!(card.desc, "");
        assert!(card.due.is_none());
    }

    #[test]
    fn deserialize_board() {
        let json = r#"{
                "id": "5abbe4b7ddc1b351ef961414",
                "name": "My Board",
                "closed": false,
                "desc": "Board description",
                "url": "https://trello.com/b/abc123/my-board"
            }"#;
        let board: TrelloBoard = serde_json::from_str(json).unwrap();
        assert_eq!(board.id, "5abbe4b7ddc1b351ef961414");
        assert_eq!(board.name, "My Board");
        assert_eq!(board.desc, "Board description");
        assert_eq!(board.url, "https://trello.com/b/abc123/my-board");
    }
}
