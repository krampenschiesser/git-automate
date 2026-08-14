# OpenCode HTTP Client — AGENTS.md

## OVERVIEW
Concrete implementation of the `ExternalAgent` trait. Manages OpenCode server sessions via HTTP — health checks, agent listing, session creation, and session status.

## STRUCTURE
```
src/external_agent/opencode/
├── mod.rs      (13)  Module decls + re-exports (OpenCodeClient, OpenCodeError)
├── client.rs   (653)  HTTP client + ExternalAgent impl
├── agent.rs    (1079) Agent definitions + prompt handling
├── api-spec.json OpenApi specification of opencode
└── types.rs    (66)  Serde response types
```

## WHERE TO LOOK
| Task | Location | Notes |
|---|---|---|
| Add API endpoint call | `client.rs` | HTTP via `reqwest`, JSON serde |
| Add new agent | `agent.rs` | Agent listing + prompt templates |
| Modify auth | `client.rs` `encode_basic_auth` | base64(user:pw) |
| Session lifecycle | `client.rs` `create_session` / `get_session` | POST /session → GET /session/{id} |
| Health check | `client.rs` `check_health` | GET /global/health |
| Agent list | `agent.rs` `list_agents` | GET /agent |

## CONVENTIONS
- **Provider-agnostic trait**: `ExternalAgent` defined in `common/mod.rs`; `OpenCodeClient` implements it here. The trait abstracts `create_session`, `get_session`, `list_agents`, `check_health`
- **Basic auth**: `encode_basic_auth(user, pw)` → `base64` → `Basic <encoded>` header. Uses `base64` crate (not `http` crate)
- **`agent.rs`** handles agent definitions and prompt mapping (`AgentName` → file/template), separate from HTTP transport in `client.rs`
- **`types.rs`**: serde structs mirror OpenCode JSON responses (`Agent`, `AgentInfo`, `HealthResponse`, `Session`, `SessionTime`)
- **Re-exports** (`mod.rs`): `pub use client::{OpenCodeClient, OpenCodeError, encode_basic_auth}` and `pub use types::{Agent, AgentInfo, HealthResponse, Session, SessionTime}`

## ANTI-PATTERNS (THIS DIRECTORY)
- **Agent definitions loaded separately**: `agent.rs` manages agent prompts/templates via `include_str!` from `src/assets/agents/` — editing these requires rebuild
- **No retry logic**: Unlike `GitHubClient::execute_with_retry`, OpenCode HTTP calls do not retry — add retry in `start_session_http` if needed
- **Auth is per-client**: `OpenCodeClient` holds the base64-encoded auth header; no token refresh mechanism (OpenCode sessions are ephemeral)
- **`agent.rs` is 1079 lines but only 66 lines of types**: Most of the file is agent prompt/handler logic, not HTTP transport — see `src/workflow/agents/` AGENTS.md for prompt details
