//! graphql_client generated query type definitions.
//!
//! Each struct corresponds to a `.graphql` file in `queries/` and uses the
//! `#[derive(GraphQLQuery)]` macro from `graphql_client`. The generated types
//! (`Variables` + `ResponseData`) are used by the methods in `client.rs`.

use graphql_client::GraphQLQuery;

// Scalar type alias needed across query modules.
pub type Date = String;

// ─── Query: get_owner_id ──────────────────────────────────────────

#[derive(GraphQLQuery, Debug, Clone, PartialEq)]
#[graphql(
    schema_path = "src/external_issues/github/schema.graphql",
    query_path = "src/external_issues/github/queries/get_owner_id.graphql"
)]
pub struct GetOwnerId;

// ─── Mutation: create_project ─────────────────────────────────────

#[derive(GraphQLQuery, Debug, Clone, PartialEq)]
#[graphql(
    schema_path = "src/external_issues/github/schema.graphql",
    query_path = "src/external_issues/github/queries/create_project.graphql"
)]
pub struct CreateProject;

// ─── Query: get_project ───────────────────────────────────────────

#[derive(GraphQLQuery, Debug, Clone, PartialEq)]
#[graphql(
    schema_path = "src/external_issues/github/schema.graphql",
    query_path = "src/external_issues/github/queries/get_project.graphql"
)]
pub struct GetProject;

// ─── Query: get_project_fields ────────────────────────────────────

#[derive(GraphQLQuery, Debug, Clone, PartialEq)]
#[graphql(
    schema_path = "src/external_issues/github/schema.graphql",
    query_path = "src/external_issues/github/queries/get_project_fields.graphql"
)]
pub struct GetProjectFields;

// ─── Mutation: add_project_field ──────────────────────────────────

#[derive(GraphQLQuery, Debug, Clone, PartialEq)]
#[graphql(
    schema_path = "src/external_issues/github/schema.graphql",
    query_path = "src/external_issues/github/queries/add_project_field.graphql"
)]
pub struct AddProjectField;

// ─── Query: get_project_status_field ──────────────────────────────

#[derive(GraphQLQuery, Debug, Clone, PartialEq)]
#[graphql(
    schema_path = "src/external_issues/github/schema.graphql",
    query_path = "src/external_issues/github/queries/get_project_status_field.graphql"
)]
pub struct GetProjectStatusField;

// ─── Mutation: add_project_status_options ─────────────────────────

#[derive(GraphQLQuery, Debug, Clone, PartialEq)]
#[graphql(
    schema_path = "src/external_issues/github/schema.graphql",
    query_path = "src/external_issues/github/queries/add_project_status_options.graphql"
)]
pub struct AddProjectStatusOptions;

// ─── Query: list_project_items ────────────────────────────────────

#[derive(GraphQLQuery, Debug, Clone, PartialEq)]
#[graphql(
    schema_path = "src/external_issues/github/schema.graphql",
    query_path = "src/external_issues/github/queries/list_project_items.graphql"
)]
pub struct ListProjectItems;

// ─── Mutation: update_project_item_status ─────────────────────────

#[derive(GraphQLQuery, Debug, Clone, PartialEq)]
#[graphql(
    schema_path = "src/external_issues/github/schema.graphql",
    query_path = "src/external_issues/github/queries/update_project_item_status.graphql",
    skip_serializing_none
)]
pub struct UpdateProjectItemStatus;

// ─── Mutation: add_issue_to_project ───────────────────────────────

#[derive(GraphQLQuery, Debug, Clone, PartialEq)]
#[graphql(
    schema_path = "src/external_issues/github/schema.graphql",
    query_path = "src/external_issues/github/queries/add_issue_to_project.graphql"
)]
pub struct AddIssueToProject;

// ─── Query: get_project_item_values ───────────────────────────────

#[derive(GraphQLQuery, Debug, Clone, PartialEq)]
#[graphql(
    schema_path = "src/external_issues/github/schema.graphql",
    query_path = "src/external_issues/github/queries/get_project_item_values.graphql"
)]
pub struct GetProjectItemValues;

// ─── Query: list_issues_with_parents ──────────────────────────────

#[derive(GraphQLQuery, Debug, Clone, PartialEq)]
#[graphql(
    schema_path = "src/external_issues/github/schema.graphql",
    query_path = "src/external_issues/github/queries/list_issues_with_parents.graphql"
)]
pub struct ListIssuesWithParents;
