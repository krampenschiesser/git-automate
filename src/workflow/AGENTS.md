# Workflow Engine — AGENTS.md

## OVERVIEW
Core orchestration: `Workflow` runs 5 error-isolated steps (Setup→OpencodeCheck→Triage→Todo→Review) across configured projects. Each step calls GitHub + OpenCode clients via `WorkflowContext`.

## STRUCTURE
```
src/workflow/
├── mod.rs          (1425)  Workflow orchestrator, enums, step dispatch
├── checks.rs       (2281)  3 check functions + start_opencode_session
└── helpers.rs      (2003)  Shared utilities, types, prompt/agent loading
```

## WHERE TO LOOK
| Task | Location | Notes |
|---|---|---|
| Add workflow step | `mod.rs` `WorkflowStep` enum + `run_all()` | Each step isolated, non-concurrency errors swallowed |
| Add new status | `mod.rs` `WorkflowStatus` enum + `helpers.rs` `ensure_status_options` | 7 statuses: Triage → Todo → In Dev → Review Tech → Review Prod → QA → Done |
| Modify check logic | `checks.rs` `run_triage_check` / `run_todo_check` / `run_review_check` | Production code ~300 lines; tests are ~1800 lines |
| Resolve project/repo | `helpers.rs` `resolve_context` | Hub function: parse repo, resolve ID, ensure fields |
| Load prompt templates | `helpers.rs` `load_prompt_template` | `include_str!` — **requires rebuild**. Agent instructions come from OpenCode agent definitions, not templates. |
| Add GitHub call | `external_issues/github/client.rs` + `types.rs` | 4-step pattern: .graphql → struct → method → test |
| Start OpenCode session | `checks.rs` `start_opencode_session` | Concurrency gate (per-agent + `default`, legacy model keys) → `GET /api/location` → `POST /api/worktree` → `POST /api/session` (agent + location) → `POST /api/session/{id}/prompt` |

## CONVENTIONS
- **Error isolation**: `run_all()` runs each step in a loop; per-project errors are logged as `"<check> failed for {project}: {e}"` and swallowed — **except** `WorkflowError::ConcurrencyExceeded`, which the `run_*_check` wrappers re-raise so `run_all()` breaks the cycle (returns `Ok(())` overall)
- **`async {}` block pattern**: triage/todo/review checks use inline `async { ... }.await` to combine `?`-based resolution with error catching (mod.rs:231, 257, 283) — do not refactor to named function without preserving semantics
- **`resolve_context` is the hub**: Called 3x (triage, todo, review); single entry point for project setup, ID resolution, and field enforcement
- **`REVIEW_STATES` array** (`checks.rs`): 3-element array covering review states — updated separately from `WorkflowStatus` enum
- **`OpencodeSessionConfig`**: Built per-project from `ProjectConfig.opencode.url` + `pw` + `concurrency`; per-agent concurrency limit (agent key, else `default`) checked before session creation via `get_active_by_agent()`; legacy keys containing `/` checked via `get_session_models()`
- **Duplicate functions**: `ensure_status_options` and `ensure_session_id_field` exist in BOTH `mod.rs` (thin wrappers with `github.as_ref()` check) and `helpers.rs` (testable in isolation) — do not remove wrappers without updating tests

## ANTI-PATTERNS (THIS DIRECTORY)
- **Production vs test ratio**: `checks.rs` is 80% tests, `helpers.rs` is 68% tests — don't assume file size reflects production complexity
- **`start_opencode_session`** must be the ONLY function that creates sessions; it resolves the project, creates a worktree, creates the session in the worktree directory, and sends the prompt. Non-concurrency errors map to `WorkflowError::Other`. `OpencodeSessionConfig` enforces the per-agent concurrency gate (`get_active_by_agent()`, `default` fallback) plus legacy per-model keys (`get_session_models()`), returning `WorkflowError::ConcurrencyExceeded` at capacity. Sessions use named OpenCode agents (`git-automate-<role>`) and receive only the user prompt — OpenCode v2 has no per-session system prompt.
- **Completion detection**: a session is still running while it appears in `GET /api/session/active`; otherwise `session_completion(...)` decides from `outcome`/`time.idle` (terminal) or neither (waiting), and 404 means not-found. `failed`/`interrupted` sessions are reset WITHOUT parsing output. A poll error skips the whole check (`Ok(())`) — never substitute an empty active map (that made every in-flight session look complete).
- **`WorkflowStatus` enum** has a custom `as_str()` method — adding a variant requires updating the string mapping, NOT derive macros
- **`write_project_id`** mutates both the in-memory `GitAutomateConfig` AND the YAML file on disk — round-trip via `serde_yaml::Value` preserves formatting
- **`resolve_project_id`**: numeric IDs resolved at runtime via `projectV2(number:)`; original value kept in config (not replaced)
- **Nested loops in `run_review_check`**: iterates 3 review states × N project items — most computationally dense function in the codebase
