# OpenCode HTTP Client — AGENTS.md

## OVERVIEW
Concrete HTTP client (`OpenCodeClient`) for the **OpenCode v2** server API (all routes under `/api/*`). Manages v2 sessions over HTTP — server info/health, location/project resolution, worktree creation, session creation/prompting, and session status/completion.

## STRUCTURE
```
src/external_agent/opencode/
├── mod.rs        Module decls + re-exports
├── client.rs     HTTP client (`OpenCodeClient`) + v2 endpoint methods
├── types.rs      Serde v2 response types
└── api-spec.json Published OpenCode v2 OpenAPI spec ("opencode HttpApi", 113 paths) — reference only, NOT compiled
```

## WHERE TO LOOK
| Task | Location | Notes |
|---|---|---|
| Server info / health | `client.rs` `server_info` / `check_health` | `GET /api/info` (bare `ServerInfo`; v2 has no `/api/health`) |
| Resolve project id | `client.rs` `get_location` | `GET /api/location?location[directory]=<dir>` (deepObject) |
| Create worktree | `client.rs` `create_worktree` | `POST /api/worktree {projectID}` → `{directory}` |
| Create session | `client.rs` `create_session` | `POST /api/session {title,agent,model?,location:{directory}}` → `{data: Session}` |
| Send prompt | `client.rs` `send_prompt` | `POST /api/session/{id}/prompt {text}` → 200 admission receipt |
| Active sessions | `client.rs` `get_active_sessions` | `GET /api/session/active` → running-only map |
| Per-agent counts | `client.rs` `get_active_by_agent` | active ids → per-id `GET /api/session/{id}` → count by `Session.agent` |
| Per-model counts | `client.rs` `get_session_models` | active ids → per-id fetch → count by `Session.model` (legacy) |
| Get session / status | `client.rs` `get_session_v2` | `GET /api/session/{id}`; 404 → `None` |
| Messages | `client.rs` `get_session_messages` | `GET /api/session/{id}/message` → `{data,cursor}` |
| Agents | `client.rs` `get_agents` | `GET /api/agent?location[directory]=<dir>` → `{location,data}` |
| Auth | `client.rs` `encode_basic_auth` | base64(user:pw), username `opencode` |

## CONVENTIONS
- **Canonical client**: `OpenCodeClient` (inherent methods) is the single API for production code; the old provider-agnostic `ExternalAgent` trait was removed.
- **Basic auth unchanged in v2**: `Basic base64("opencode:<pw>")` on every request.
- **Envelope rule**: session/agent endpoints wrap payloads in `{ "data": ... }`; `GET /api/info`, `GET /api/location`, and `POST /api/worktree` return BARE objects.
- **Naming**: the method is `get_session_v2` (kept for historical reasons) — it fetches a single session via `GET /api/session/{id}`.
- **Named agents**: sessions are created with `agent:"git-automate-<role>"`; the agent must be present in OpenCode (see below). v2 has **no per-session system prompt**.
- **types.rs**: permissive serde structs — unknown/optional fields are ignored or `Option`, never `deny_unknown_fields`.

## AGENT PRESENCE (operational requirement)
The daemon sends agent ids `git-automate-{triage,taskmanager,developer,reviewer,product,qa}`. OpenCode v2 derives an agent id from the **filename minus `.md`**, so the definitions must be installed as `<id>.md`:
- project: `<repo>/.opencode/agents/git-automate-triage.md` (commit it so worktrees inherit it), or
- global: `~/.config/opencode/agents/git-automate-triage.md`, or
- the v2 `agents` map in `opencode.json`.

Bodies live at `src/assets/agents/git-automate-<role>.agent.md` (that `.agent.md` name is legacy/editor convention only — installing it verbatim yields the wrong id `git-automate-triage.agent`). The shipped agents are `mode: primary` so they can be selected as the session's agent. `doctor`/`ensure_agents_installed` installs them as `<id>.md` under `$XDG_CONFIG_HOME/opencode/agents` (default `~/.config/opencode/agents`). A session whose agent is missing is still created (HTTP 200) but ends with `outcome:"failed"`.

## ANTI-PATTERNS (THIS DIRECTORY)
- **No v1 routes**: `/global/health`, `/session`, `/session/status`, `/session/{id}/prompt_async`, `/experimental/workspace`, `/experimental/worktree` no longer exist in v2.
- **No per-session system prompt**: `system` is not a field of v2 session create or prompt; it lives in the agent definition.
- **`api-spec.json` is reference-only** — not `include_str!`'d; no rebuild needed when it changes. It is the published v2 spec.
- **No retry logic**: OpenCode HTTP calls do not retry (unlike `GitHubClient::execute_with_retry`).
- **Completion caveat**: `GET /api/session/active` is running-only — a finished *or idle* session is absent; use `outcome`/`time.idle` from `GET /api/session/{id}` to decide terminal state (see `session_completion` in `workflow/helpers.rs`).
