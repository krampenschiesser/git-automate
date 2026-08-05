# src/external_issues/ — GitHub API Client

GraphQL + REST client for GitHub Projects V2 and issues via `reqwest`.

## WHERE TO LOOK

| File | Role | Key Exports |
|------|------|-------------|
| `client.rs` | `GitHubClient` — all API methods | `graphql()`, `create_project()`, `get_project_fields()`, `get_project_status_field()`, `list_project_items()`, `update_project_item_status()` |
| `types.rs` | Typed response structs per GraphQL query | `NodeProjectResult`, `StatusFieldInfo`, `ProjectItem`, `IssueInfo`, `ProjectFieldInfo`, `GitHubError` |
| `mod.rs` | Submodule re-exports | `pub use client::GitHubClient`; domain types |
| `repo.rs` | Repository URL parser | `parse_repository_url() -> ParsedRepo` |
| `project.rs` | Project V2 field types | `ProjectV2Summary` |
| `issues.rs` | Issue-related REST helpers | Issue type definitions |

## CONVENTIONS

- **GraphQL**: `graphql<T: DeserializeOwned>()` generic method — define a `*Result` struct per query; inline `r#"..."#` query strings with `$variable` syntax
- **REST**: `rest_get()` / `rest_post()` private helpers for non-GraphQL endpoints (issues REST)
- **Auth**: `Authorization: Bearer {token}` + `User-Agent: git-automate` + `Accept: application/vnd.github+json`
- **Errors**: `GitHubError` enum (`HttpStatus`, `GraphQLError`, `OwnerNotFound`, `ProjectNotFound`, `GraphQLError`) — all implement `thiserror::Error`
- **Response shaping**: `list_project_items` filters out items with `null` content; `get_project_item_values` returns `BTreeMap<String, Option<String>>`

## ANTI-PATTERNS

- **No covering unit tests**: `GitHubClient` methods (`list_repo_issues`, `get_project_status_field`, `add_project_field`, etc.) have no unit tests — only integration tests in `tests/integration.rs` via `wiremock`
- **`catch {}` not used here**: unlike `check_opencode_health`, GraphQL errors are propagated as `Err(GitHubError::GraphQLError(...))`
