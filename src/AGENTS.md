# src/ — Source Code

All Rust source is flat in this directory. Submodules are in `src/external_issues/`, `src/external_agent/`, `src/workflow/`, and `src/assets/`.

## WHERE TO LOOK

| File/Dir | Role | Key Exports |
|----------|------|-------------|
| `lib.rs` | Library entry point, test utilities | `test_utils::SET_CWD_MUTEX` |
| `main.rs` | Daemon CLI (clap), polling loop, signal handling, tracing init | `main()`, `Cli`, `serve()`, `check_health()`, `init_tracing()`, `GitAutomateFormatLayer` |
| `config.rs` | YAML config + `${env:VAR}` substitution | `ProjectConfig`, `GitAutomateConfig`, `parse_config()`, `load_config()` |
| `shell.rs` | Shell command execution abstraction | `ShellFn`, `ShellOutput` |
| `external_issues/` | External issue sources — abstraction + GitHub impl | `ExternalIssue`, `ExternalIssueError`, `ExternalIssueSource` (common); `GitHubClient`, `GitHubIssueSource`, `GitHubError` (github) |
| `external_agent/` | OpenCode HTTP client | `OpenCodeClient`, `Agent`, `AgentInfo`, `Session`, `PromptPart` |
| `workflow/` | Workflow engine | `Workflow` struct, `run_triage_check()`, `run_todo_check()`, `run_review_check()`, `WorkflowContext`, `ProjectContext` |
| `assets/` | Embedded prompt/agent templates | 6 prompts + 6 `.agent.md` files (via `include_str!`) |

## CONVENTIONS

- **Error handling**: `thiserror` for error types, `anyhow` for top-level error handling; `try/catch` per project iteration in the workflow loop
- **Logging**: via `tracing` crate; levels: `info`, `warn`, `error`; format `[git-automate][LEVEL] message`; ERROR to stderr, others to stdout
- **`deps` pattern**: `WorkflowContext` and `ProjectContext` hold injected dependencies (config, github, shell)
- **GraphQL**: typed response interfaces for each query — define `QueryXxxResult` per query
- **REST/HTTP**: `reqwest::Client` for API calls to GitHub and OpenCode
- **Assets embedded**: `include_str!("../assets/prompts/*.md")` — no runtime file access for prompts
- **Environment variables**: `${env:VAR}` → empty string if unset (not left as `${env:VAR}`)
- **Branch naming**: `issue-{ISSUE_NUMBER}` (e.g., `issue-42`)
- **Issue trigger**: titles starting with `@ai ` are picked up by triage check
- **Polling**: 30s interval, standalone daemon (no `session.idle` hook)

## ANTI-PATTERNS

- **`catch {}` without binding**: used intentionally in `check_opencode_health` to swallow network errors and return `false`
- **`GITHUB_TOKEN` optional**: if unset, `github` is `None` — checks fail gracefully
- **No linting**: no Clippy config beyond `-- -D warnings`
- **Check functions untested**: `run_triage_check`, `run_todo_check`, `run_review_check` have no unit tests — only integration tests
- **`serve()` polling loop untested**: `main.rs` only tests CLI parsing (8 tests), not the 30s poll loop
