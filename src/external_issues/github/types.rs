//! Domain types and GraphQL/REST response structs for the GitHub API client.
//! - REST response shapes: derived from octokit response.data usage

use serde::Deserialize;

// ─── Domain types ───────────────────────────────────────────

// Re-exported from `project.rs` — moved to match AGENTS.md intent.
pub use super::project::ProjectV2Summary;

/// A field on a Project V2 board.
///
/// `dataType` is serialized as `dataType` in the GraphQL response;
/// serde rename maps the Rust snake_case `data_type`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Hash)]
pub struct ProjectFieldInfo {
    pub id: String,
    pub name: String,
    #[serde(rename = "dataType")]
    pub data_type: String,
}

/// A single selectable option within a `StatusFieldInfo`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Hash)]
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

// Re-exported from `issues.rs` — moved to match AGENTS.md intent.
pub use super::issues::IssueInfo;

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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct CreateProjectV2Result {
    #[serde(rename = "createProjectV2")]
    pub create_project_v2: CreateProjectV2Inner,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct CreateProjectV2Inner {
    pub id: String,
}

/// Response for `get_project` query.
///
/// `node` is `null` when the project doesn't exist.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct NodeProjectResult {
    pub node: Option<NodeProjectInner>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct NodeProjectInner {
    pub id: String,
    pub number: i64,
    pub title: String,
}

/// Response for `get_owner_id` query.
/// Exactly one of `user` or `organization` will have an `id`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct NodeOwnerResult {
    pub user: Option<NodeIdOnly>,
    pub organization: Option<NodeIdOnly>,
}

/// Response for `get_project_by_number` query.
///
/// `user.project_v2` and `organization.project_v2` are both `Option` —
/// exactly one will be `Some` when the project exists under the given owner.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct ProjectNumberResult {
    pub user: Option<NumberProjectHolder>,
    pub organization: Option<NumberProjectHolder>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct NumberProjectHolder {
    #[serde(rename = "projectV2")]
    pub project_v2: Option<IdHolder>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct NodeIdOnly {
    pub id: String,
}

/// Response for `get_project_fields` query.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct NodeFieldsResult {
    pub node: Option<NodeFieldsInner>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct NodeFieldsInner {
    pub fields: NodeFieldsList,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct NodeFieldsList {
    pub nodes: Vec<ProjectFieldInfo>,
}

/// Response for `add_project_field` mutation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct CreateFieldResult {
    #[serde(rename = "createProjectV2Field")]
    pub create_project_v2_field: CreateFieldInner,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct CreateFieldInner {
    #[serde(rename = "projectField")]
    pub project_field: IdHolder,
}

/// Response for `get_project_status_field` query.
/// `field` is `null` if the field doesn't exist or isn't a single-select field.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct StatusFieldResult {
    pub node: Option<StatusFieldNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct StatusFieldNode {
    pub field: Option<StatusFieldDetail>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct StatusFieldDetail {
    pub id: String,
    pub options: Vec<StatusOption>,
}

/// Response for `add_project_status_options` mutation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct UpdateFieldConfigResult {
    #[serde(rename = "updateProjectV2FieldConfiguration")]
    pub update_project_v2_field_configuration: UpdateFieldConfigInner,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct UpdateFieldConfigInner {
    #[serde(rename = "projectV2Field")]
    pub project_v2_field: IdHolder,
}

/// Response for `update_project_item_status` / `update_project_item_session_id` mutations.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct UpdateItemFieldValueResult {
    #[serde(rename = "updateProjectV2ItemFieldValue")]
    pub update_project_v2_item_field_value: UpdateItemFieldValueInner,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct UpdateItemFieldValueInner {
    #[serde(rename = "projectV2Item")]
    pub project_v2_item: IdHolder,
}

/// Response for `add_issue_to_project` mutation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct AddItemResult {
    #[serde(rename = "addProjectV2ItemById")]
    pub add_project_v2_item_by_id: AddItemInner,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct AddItemInner {
    pub item: IdHolder,
}

/// Response for `list_project_items` query.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct ListProjectItemsResult {
    pub node: Option<ListProjectItemsNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct ListProjectItemsNode {
    pub items: ListProjectItemsList,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct ListProjectItemsList {
    pub nodes: Vec<ListProjectItemNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct ListProjectItemNode {
    pub id: String,
    pub content: Option<ListProjectItemContent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct ListProjectItemContent {
    #[serde(rename = "__typename")]
    pub typename: String,
    pub id: String,
    pub number: i64,
}

/// Response for `get_project_item_values` query.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct NodeFieldValuesResult {
    pub node: Option<NodeFieldValuesNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct NodeFieldValuesNode {
    #[serde(rename = "fieldValues")]
    pub field_values: NodeFieldValuesList,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct NodeFieldValuesList {
    pub nodes: Vec<NodeFieldValueNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct RestRepo {
    #[serde(rename = "default_branch")]
    pub default_branch: String,
}

/// Response from `GET /repos/{owner}/{repo}/commits/{ref}` — used for `sha`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct RestCommit {
    pub sha: String,
}

/// Response from `GET /repos/{owner}/{repo}/issues/{number}` — used for `node_id`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct RestIssueNode {
    #[serde(rename = "node_id")]
    pub node_id: String,
}

/// Helper struct for responses that just need an `id` field.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct IdHolder {
    pub id: String,
}

// ─── Issue hierarchy (GraphQL) ────────────────────────────────

/// A single issue node with its parent issue number (if any).
///
/// Used by [`GitHubClient::list_issues_with_parents`] to build the
/// parent → children (sub-task) map.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct IssueWithParent {
    pub id: String,
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    /// The number of the parent issue, if this is a sub-issue.
    pub parent_number: Option<i64>,
}

/// GraphQL response for `list_issues_with_parents`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct IssuesWithParentsResult {
    pub repository: IssuesWithParentsRepo,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct IssuesWithParentsRepo {
    pub issues: IssuesWithParentsList,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct IssuesWithParentsList {
    pub nodes: Vec<IssueWithParentNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct IssueWithParentNode {
    pub id: String,
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    #[serde(rename = "parentIssue")]
    pub parent_issue: Option<IssueParentRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct IssueParentRef {
    pub number: i64,
}

/// Deserialise from [`IssueWithParentNode`] (which has the nested
/// `parent_issue` field) into the flat [`IssueWithParent`].
impl From<IssueWithParentNode> for IssueWithParent {
    fn from(node: IssueWithParentNode) -> Self {
        IssueWithParent {
            id: node.id,
            number: node.number,
            title: node.title,
            body: node.body,
            state: node.state,
            parent_number: node.parent_issue.map(|p| p.number),
        }
    }
}
