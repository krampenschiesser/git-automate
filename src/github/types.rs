//! Domain types and GraphQL/REST response structs for the GitHub API client.
//!
//! Mirrors the TypeScript interfaces from `src/github.ts`:
//! - Domain types: lines 14-44, 340-403
//! - GraphQL response structs: lines 78-144
//! - REST response shapes: derived from octokit response.data usage

use serde::Deserialize;

// ─── Domain types ───────────────────────────────────────────

/// Summary of a GitHub Project V2, returned by `get_project`.
///
/// `number` is kept as a `String` to match the TypeScript interface
/// (`github.ts:16`) where `String(result.node.number)` is used.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ProjectV2Summary {
    pub id: String,
    pub number: String,
    pub title: String,
}

/// A field on a Project V2 board.
///
/// `dataType` is serialized as `dataType` in the GraphQL response;
/// serde rename maps the Rust snake_case `data_type`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ProjectFieldInfo {
    pub id: String,
    pub name: String,
    #[serde(rename = "dataType")]
    pub data_type: String,
}

/// A single selectable option within a `StatusFieldInfo`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct StatusOption {
    pub id: String,
    pub name: String,
}

/// The project's "Status" single-select field and its available options.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct StatusFieldInfo {
    pub id: String,
    pub options: Vec<StatusOption>,
}

/// A GitHub issue (or PR) as returned by REST endpoints.
///
/// `id` corresponds to `node_id` in REST responses and `id` in GraphQL.
/// `body` is `None` when the issue has no body or the API returns `null`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct IssueInfo {
    pub id: String,
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
}

/// Parsed owner/repo extracted from a repository URL or shorthand.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedRepo {
    pub owner: String,
    pub repo: String,
}

/// A single item listed in a Project V2 board.
///
/// `content` may be `null` for items that are not issues/PRs; such items
/// are filtered out by `list_project_items`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectItem {
    pub id: String,
    pub content_node_id: String,
    pub content_type: String,
    pub content_number: i64,
}

// ─── GraphQL response structs ────────────────────────────────

/// Response for `create_project` mutation.
#[derive(Debug, Deserialize)]
pub struct CreateProjectV2Result {
    #[serde(rename = "createProjectV2")]
    pub create_project_v2: CreateProjectV2Inner,
}

#[derive(Debug, Deserialize)]
pub struct CreateProjectV2Inner {
    pub id: String,
}

/// Response for `get_project` query.
///
/// `node` is `null` when the project doesn't exist.
#[derive(Debug, Deserialize)]
pub struct NodeProjectResult {
    pub node: Option<NodeProjectInner>,
}

#[derive(Debug, Deserialize)]
pub struct NodeProjectInner {
    pub id: String,
    pub number: i64,
    pub title: String,
}

/// Response for `get_owner_id` query.
/// Exactly one of `user` or `organization` will have an `id`.
#[derive(Debug, Deserialize)]
pub struct NodeOwnerResult {
    pub user: Option<NodeIdOnly>,
    pub organization: Option<NodeIdOnly>,
}

#[derive(Debug, Deserialize)]
pub struct NodeIdOnly {
    pub id: String,
}

/// Response for `get_project_fields` query.
#[derive(Debug, Deserialize)]
pub struct NodeFieldsResult {
    pub node: Option<NodeFieldsInner>,
}

#[derive(Debug, Deserialize)]
pub struct NodeFieldsInner {
    pub fields: NodeFieldsList,
}

#[derive(Debug, Deserialize)]
pub struct NodeFieldsList {
    pub nodes: Vec<ProjectFieldInfo>,
}

/// Response for `add_project_field` mutation.
#[derive(Debug, Deserialize)]
pub struct CreateFieldResult {
    #[serde(rename = "createProjectV2Field")]
    pub create_project_v2_field: CreateFieldInner,
}

#[derive(Debug, Deserialize)]
pub struct CreateFieldInner {
    #[serde(rename = "projectField")]
    pub project_field: IdHolder,
}

/// Response for `get_project_status_field` query.
/// `field` is `null` if the field doesn't exist or isn't a single-select field.
#[derive(Debug, Deserialize)]
pub struct StatusFieldResult {
    pub node: Option<StatusFieldNode>,
}

#[derive(Debug, Deserialize)]
pub struct StatusFieldNode {
    pub field: Option<StatusFieldDetail>,
}

#[derive(Debug, Deserialize)]
pub struct StatusFieldDetail {
    pub id: String,
    pub options: Vec<StatusOption>,
}

/// Response for `add_project_status_options` mutation.
#[derive(Debug, Deserialize)]
pub struct UpdateFieldConfigResult {
    #[serde(rename = "updateProjectV2FieldConfiguration")]
    pub update_project_v2_field_configuration: UpdateFieldConfigInner,
}

#[derive(Debug, Deserialize)]
pub struct UpdateFieldConfigInner {
    #[serde(rename = "projectV2Field")]
    pub project_v2_field: IdHolder,
}

/// Response for `update_project_item_status` / `update_project_item_session_id` mutations.
#[derive(Debug, Deserialize)]
pub struct UpdateItemFieldValueResult {
    #[serde(rename = "updateProjectV2ItemFieldValue")]
    pub update_project_v2_item_field_value: UpdateItemFieldValueInner,
}

#[derive(Debug, Deserialize)]
pub struct UpdateItemFieldValueInner {
    #[serde(rename = "projectV2Item")]
    pub project_v2_item: IdHolder,
}

/// Response for `add_issue_to_project` mutation.
#[derive(Debug, Deserialize)]
pub struct AddItemResult {
    #[serde(rename = "addProjectV2ItemById")]
    pub add_project_v2_item_by_id: AddItemInner,
}

#[derive(Debug, Deserialize)]
pub struct AddItemInner {
    pub item: IdHolder,
}

/// Response for `list_project_items` query.
#[derive(Debug, Deserialize)]
pub struct ListProjectItemsResult {
    pub node: Option<ListProjectItemsNode>,
}

#[derive(Debug, Deserialize)]
pub struct ListProjectItemsNode {
    pub items: ListProjectItemsList,
}

#[derive(Debug, Deserialize)]
pub struct ListProjectItemsList {
    pub nodes: Vec<ListProjectItemNode>,
}

#[derive(Debug, Deserialize)]
pub struct ListProjectItemNode {
    pub id: String,
    pub content: Option<ListProjectItemContent>,
}

#[derive(Debug, Deserialize)]
pub struct ListProjectItemContent {
    #[serde(rename = "__typename")]
    pub typename: String,
    pub id: String,
    pub number: i64,
}

/// Response for `get_project_item_values` query.
#[derive(Debug, Deserialize)]
pub struct NodeFieldValuesResult {
    pub node: Option<NodeFieldValuesNode>,
}

#[derive(Debug, Deserialize)]
pub struct NodeFieldValuesNode {
    #[serde(rename = "fieldValues")]
    pub field_values: NodeFieldValuesList,
}

#[derive(Debug, Deserialize)]
pub struct NodeFieldValuesList {
    pub nodes: Vec<NodeFieldValueNode>,
}

#[derive(Debug, Deserialize)]
pub struct NodeFieldValueNode {
    pub name: Option<String>,
    pub text: Option<String>,
    pub option: Option<String>,
}

// ─── REST response structs ───────────────────────────────────

/// Fields extracted from a REST issue object.
///
/// `pull_request` being `None` means the item is a pure issue
/// (not a PR). Both an absent key and `null` deserialize to `None`.
#[derive(Debug, Deserialize)]
pub struct RestIssue {
    #[serde(rename = "node_id")]
    pub node_id: String,
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
}

/// Response from `GET /repos/{owner}/{repo}` — used for `default_branch`.
#[derive(Debug, Deserialize)]
pub struct RestRepo {
    #[serde(rename = "default_branch")]
    pub default_branch: String,
}

/// Response from `GET /repos/{owner}/{repo}/commits/{ref}` — used for `sha`.
#[derive(Debug, Deserialize)]
pub struct RestCommit {
    pub sha: String,
}

/// Response from `GET /repos/{owner}/{repo}/issues/{number}` — used for `node_id`.
#[derive(Debug, Deserialize)]
pub struct RestIssueNode {
    #[serde(rename = "node_id")]
    pub node_id: String,
}

/// Helper struct for responses that just need an `id` field.
#[derive(Debug, Deserialize)]
pub struct IdHolder {
    pub id: String,
}
