# GitHub GraphQL/REST Client — AGENTS.md

## OVERVIEW
GitHub API client: 64 methods on `GitHubClient`, 17 `.graphql` queries embedded via `include_str!`, 4-retry HTTP transport with error handling.

## STRUCTURE
```
src/external_issues/github/
├── mod.rs              (10)  Module decls + re-exports (GitHubClient, GitHubError)
├── client.rs           (1706)  HTTP transport + 64 GraphQL/REST methods
├── types.rs            (438)  All response deserialization structs
├── repo.rs             (184)  URL parsing
├── issues.rs           (16)  Issue-specific types
├── project.rs          (14)  Project-specific types
└── queries/            (20)  17 .graphql files + schema.graphql + graphql.config.yml
```

## WHERE TO LOOK
| Task | Location | Notes |
|---|---|---|
| Add new query | `queries/<name>.graphql` + `client.rs` + `types.rs` | 4-step pattern: see below |
| Debug GraphQL errors | `client.rs` `graphql<T>()` | Parses `data` field, extracts `errors` array |
| Add response field | `types.rs` | Must match GraphQL selection set exactly |
| Resolve owner ID | `client.rs` `get_owner_id` | Tries user then org |
| Project by number | `client.rs` `get_project_by_number` | User-first, org-fallback (`if let Ok` chains) |
| List issues | `client.rs` `list_repo_issues` | REST endpoint, filters out PRs |
| Update field value | `client.rs` `update_project_item_status` / `update_project_item_session_id` | Use `body_string_contains` for mock disambiguation |

## CONVENTIONS
- **4-step query pattern**: (1) Create `queries/<name>.graphql` with named operations, (2) Add response struct to `types.rs` with `#[serde(rename)]` for camelCase, (3) Add method to `client.rs` using `include_str!("queries/<name>.graphql")` + `self.graphql::<Type>()`, (4) Add unit test in `client.rs` using `wiremock` with `body_string_contains` disambiguation
- **`include_str!`** embeds all `.graphql` files at compile time — editing a query file requires `cargo build`
- **`graphql.config.yml`**: tooling-only config (GraphQL codegen/inspector) — NOT read by application code. Points to `schema.graphql` and GitHub endpoint
- **`schema.graphql`**: 64k-line introspection dump of GitHub schema — NOT read by application code
- **`execute_with_retry`**: 4-attempt retry on 5xx/network errors with exponential backoff
- **`get_project_item_values`**: 6-variant `match` on `NodeFieldValueNode` enum — adding a field type requires updating both the enum in `types.rs` and this match
- **Mock disambiguation**: Use `body_string_contains` on unique substrings: `user(login:`, `createProjectV2(input`, `field(name:`, `fields(first:`, `updateProjectV2Field`, `createProjectV2Field`, `items(first:`, `fieldValues(first:`

## ANTI-PATTERNS (THIS DIRECTORY)
- **Not all fields are covered**: `types.rs` only defines structs for fields the app reads — if you need a new field, you must add both the GraphQL selection AND the struct field
- **`if let Ok` fallback chains** (client.rs:255, 282): user-first then org-fallback — this is the only place using Rust 1.65+ combined `if let` chains; follow the same pattern for similar logic
- **`NodeFieldValueNode` discriminated union**: every `match` arm uses `field.and_then(|f| f.name)` — if adding variants, consider extracting this repeated pattern
- **Schema drift risk**: `schema.graphql` is a static dump — if GitHub adds new types, regenerate via introspection before adding queries that use them
- **No runtime query validation**: queries are embedded at compile time; syntax errors only surface at runtime when GitHub returns a 400
