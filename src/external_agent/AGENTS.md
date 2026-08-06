# src/external_agent/ — External AI Agent Abstraction

Provider-agnostic trait + OpenCode HTTP implementation for managing AI agent
sessions.

## STRUCTURE

```
external_agent/
├── mod.rs           # Module declarations + re-exports
├── common/          # Provider-agnostic abstractions
│   └── mod.rs       # ExternalAgent trait, ExternalAgentError, AgentSessionStatus, SessionInfo
└── opencode/        # OpenCode-specific implementation
    ├── mod.rs       # Submodule declarations + re-exports
    ├── client.rs    # OpenCodeClient — HTTP methods, OpenCodeError, encode_basic_auth
    ├── types.rs     # Agent, SessionTime, Session, HealthResponse, AgentInfo
    └── agent.rs     # impl ExternalAgent for OpenCodeClient, extract_shell_output, tests
```

## WHERE TO LOOK

| File | Role | Key Exports |
|------|------|-------------|
| `common/mod.rs` | Provider-agnostic abstractions | `ExternalAgent` trait, `ExternalAgentError`, `AgentSessionStatus`, `SessionInfo` |
| `opencode/client.rs` | `OpenCodeClient` — all HTTP methods | `new()`, `check_health()`, `get_agents()`, `start_session()` |
| `opencode/types.rs` | Serde types for API responses | `Agent`, `AgentInfo`, `Session`, `SessionTime`, `HealthResponse` |
| `opencode/agent.rs` | Trait impl + tests | `ExternalAgent for OpenCodeClient`, `extract_shell_output`, `From<Session> for SessionInfo` |

## CONVENTIONS

- **Auth**: HTTP Basic auth — username `"opencode"`, precomputed as `Basic base64("opencode:<pw>")` via `encode_basic_auth()` helper
- **Health check**: `check_health()` returns `bool` — on any failure (network error, non-2xx, malformed JSON, missing field) returns `false`
- **`get_agents(directory)`**: when `directory` is `Some`, appends `?directory=<dir>` query param matching the SDK's `client.app.agents({ directory })`
- **`start_session()`**: POSTs to create a session, then immediately sends the prompt in a second call
- **Errors**: `OpenCodeError` enum (`Http`, `HttpStatus`, `FetchAgents`, `CreateSession`, `NoSessionData`) — `thiserror::Error`
- **`AgentInfo` projection**: `From<Agent>` strips `mode` and `built_in` fields, projecting to just `name` + `description`
- **`SessionInfo` projection**: `From<Session>` strips `version` and `time` fields, projecting to `id`, `title`, `directory`, `project_id`

## DESIGN NOTES

- **`common` does not import `opencode` types** — the trait, error, and canonical `SessionInfo` are fully provider-agnostic
- **`From<Session> for SessionInfo`** lives in `opencode/agent.rs` because it depends on the OpenCode-specific `Session` struct
