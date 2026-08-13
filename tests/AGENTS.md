# Test Conventions — AGENTS.md

## OVERVIEW
Integration tests use `wiremock` for HTTP mocking + `tempfile` for disk state + `SET_CWD_MUTEX` for cwd isolation. Unit tests live inline in each `src/` module's `#[cfg(test)]` block.

## STRUCTURE
```
tests/
├── common/
│   └── mod.rs          (267)  Shared mock setup + fixtures
├── e2e_test.rs          (275)  End-to-end workflow test
└── workflow_tests.rs    (982)  Workflow integration tests
```

## WHERE TO LOOK
| Task | Location | Notes |
|---|---|---|
| Add mock helper | `common/mod.rs` | Follow `mount_*_mock` pattern |
| Add integration test | `workflow_tests.rs` or `e2e_test.rs` | Use `make_deps()` from common |
| Mock GitHub API | `common/mod.rs` `mount_github_graphql_mocks` | `body_string_contains` disambiguation |
| Mock OpenCode API | `common/mod.rs` `mount_opencode_mocks` | Health + agents endpoints |
| Test config parsing | `src/config.rs` inline tests | Uses `tempfile`, `SET_CWD_MUTEX` |
| Debug test flakiness | `src/lib.rs` `SET_CWD_MUTEX` | 14 usage locations across codebase |

## CONVENTIONS
- **`SET_CWD_MUTEX`**: Any test calling `write_project_id`, `clone_repo_if_needed`, or `load_config` mutates `cwd` — MUST acquire `SET_CWD_MUTEX.lock().await` and restore original directory afterward. This is the #1 cause of flakiness
- **Mock disambiguation**: GitHub mocks use `body_string_contains` on unique GraphQL substrings — follow this pattern for new mocks:
  - `user(login:` — owner lookup
  - `createProjectV2(input` — project creation
  - `field(name:` — status field query
  - `fields(first:` — field list
  - `updateProjectV2Field` — status update
  - `createProjectV2Field` — field creation
  - `items(first:` — project items
  - `fieldValues(first:` — field values
- **`T#` numbering**: Tests numbered in comments (e.g. `T1`, `T7`) — follow for new tests
- **`make_deps(github, with_opencode, opencode_url)`**: Builds `WorkflowContext` with a single project — use for all integration tests
- **`project_with_opencode(url)` / `project_without_opencode()`**: Builder functions for `ProjectConfig` — use instead of constructing manually
- **`wiremock`**: Both GitHub (`/graphql`) and OpenCode (`/global/health`, `/agent`, `/session`)
- **`tempfile`**: Used for on-disk config files in config tests
- **`ENV_LOCK`** (in `main.rs` tests): Mutex for env var isolation — not the same as `SET_CWD_MUTEX`

## ANTI-PATTERNS (THIS DIRECTORY)
- **Two mock layers**: Unit tests define local helpers inline; integration tests use shared `common/mod.rs`. Do NOT add helpers to one layer only — keep them in sync
- **`SET_CWD_MUTEX` re-exported**: `tests/common/mod.rs` re-exports it from `git_automate::test_utils` — the single source of truth. Do NOT define a new mutex
- **`body_string_contains` is case-sensitive**: GraphQL operation names and field names must match exactly
- **Mock ordering matters**: `wiremock` matches in registration order — more specific mocks should be mounted first
- **Do NOT use `tempfile::tempdir()` in cwd-mutating tests** — the temp dir path may be affected by `set_current_dir`. Use absolute paths
