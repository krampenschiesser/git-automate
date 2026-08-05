# src/external_agent/ — OpenCode HTTP Client

Minimal HTTP client for the OpenCode server API (health, agents, sessions).

## WHERE TO LOOK

| File | Role | Key Exports |
|------|------|-------------|
| `client.rs` | `OpenCodeClient` — all HTTP methods | `new()`, `check_health()`, `get_agents()`, `start_session()` |
| `types.rs` | Serde types for API responses | `Agent`, `AgentInfo`, `Session`, `SessionTime`, `HealthResponse` |
| `mod.rs` | Module re-exports | `pub use client::OpenCodeClient`, `pub use types::*` |

## CONVENTIONS

- **Auth**: HTTP Basic auth — username `"opencode"`, precomputed as `Basic base64("opencode:<pw>")` via `encode_basic_auth()` helper
- **Health check**: `check_health()` returns `bool` — mirrors TypeScript `catch {}` semantics: any failure (network error, non-2xx, malformed JSON, missing field) returns `false`
- **`get_agents(directory)`**: when `directory` is `Some`, appends `?directory=<dir>` query param matching the SDK's `client.app.agents({ directory })`
- **`start_session()`**: POSTs to create a session, then immediately sends the prompt in a second call
- **Errors**: `OpenCodeError` enum (`Http`, `HttpStatus`, `FetchAgents`, `CreateSession`, `NoSessionData`) — `thiserror::Error`
- **`AgentInfo` projection**: `From<Agent>` strips `mode` and `built_in` fields, projecting to just `name` + `description`

## ANTI-PATTERNS

- **Only `check_health` is unit-tested**: `get_agents()` and `start_session()` have no direct unit tests — only `check_health()` is covered via `wiremock` in `src/main.rs`
- **Health check swallows all errors**: `check_health()` → `unwrap_or_default()` — returns `false` on any failure, never propagates
