# src/workflow/ — Workflow Engine

Orchestrates the GitHub issue lifecycle: setup → triage → todo → review.

## WHERE TO LOOK

| File | Role | Key Exports |
|------|------|-------------|
| `mod.rs` | `Workflow` struct — runs all checks in sequence | `Workflow`, `run_all()`, `run_setup_check()`, `run_opencode_check()`, `run_triage_check()`, `run_todo_check()`, `run_review_check()`, `REQUIRED_AGENTS` |
| `checks.rs` | Free-function check implementations | `run_triage_check()`, `run_todo_check()`, `run_review_check()`, `OpencodeSessionConfig`, `start_opencode_session()` |
| `helpers.rs` | Injected dependencies + project context | `WorkflowContext`, `ProjectContext`, `WorkflowError`, `resolve_context()`, `clone_repo_if_needed()`, `fill_prompt()`, `write_project_id()` |

## CONVENTIONS

- **Deps injection**: `WorkflowContext` holds `config`, `github` (Option), `shell_deps` — tests inject mocks via closures
- **Error isolation**: each check in `run_all()` catches and logs its own errors — always returns `Ok(())`; errors are logged, never returned
- **Status machine** (hardcoded in `mod.rs`): `Triage → Todo → In Development → Review Technical → Review Product → QA → Done`
- **Branch naming**: `issue-{ISSUE_NUMBER}` (e.g. `issue-42`)
- **Issue trigger**: titles starting with `@ai ` are picked up by triage check
- **Required agents**: `git-automate-triage`, `git-automate-taskmanager`, `git-automate-developer`, `git-automate-reviewer`, `git-automate-product`, `git-automate-qa` (see `REQUIRED_AGENTS`)
- **Prompts**: `{{KEY}}` placeholder syntax filled by `fill_prompt()` with regex `\{\{\s*([A-Z_]+)\s*\}\}`
- **Repo cloning**: to `/tmp/git-automate-work/{owner}-{repo}` with `--depth 1`
- **Checks delegate to free functions**: `Workflow::run_triage_check()` calls `checks::run_triage_check()` — orchestrator wraps free-function implementations

## ANTI-PATTERNS

- **No unit tests on check functions**: `run_triage_check()`, `run_todo_check()`, `run_review_check()` have no covering unit tests — only integration tests via `wiremock`
- **`serve()` polling loop untested**: `main.rs` only tests CLI parsing, not the `tokio::select!` 30s poll loop
- **`GITHUB_TOKEN` optional**: if unset, `github` is `None` — setup/triage/todo/review checks skip gracefully
