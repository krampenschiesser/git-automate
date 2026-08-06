# PROJECT KNOWLEDGE BASE

**Generated:** 2026-08-05
**Commit:** 3bcfa39 (main)

use the `sks-rust-basic` skill for writing code, tests or reviewing those.

## OVERVIEW

Standalone Rust daemon that automates GitHub issue workflows via a polling loop — finds `@ai`-tagged issues, assigns them to project boards, and starts OpenCode agent sessions (triage → developer → review → QA).

## STRUCTURE
```
.
├── src/                 # All Rust source (4 flat files + 5 subdirs)
│   ├── lib.rs           # Library entry point + test utilities
│   ├── main.rs          # Daemon CLI (clap), polling loop, signal handling
│   ├── config.rs        # YAML config + ${env:VAR} substitution
│   ├── shell.rs         # Shell command execution abstraction
│   ├── external_issues/ # GitHub API client (GraphQL + REST via reqwest)
│   ├── external_agent/  # OpenCode HTTP client
│   ├── workflow/        # Workflow engine (helpers, checks, orchestrator)
│   ├── issues/          # External issue source abstraction (trait + GitHub impl)
│   └── assets/          # Embedded prompt/agent templates (include_str!)
├── tests/               # Integration tests (wiremock-based)
├── git-automate.yml     # Runtime config — GitHub projects + OpenCode server creds
├── Cargo.toml           # Rust build config
├── Cargo.lock           # Rust lockfile
├── opencode.json        # Agent directory config for OpenCode
├── README.md            # Project documentation
└── .github/workflows/rust.yml  # CI configuration
```

## WHERE TO LOOK

| Task | Location | Notes |
|------|----------|-------|
| Daemon entry point | `src/main.rs` | CLI with clap, 30s polling loop, signal handling |
| Config parsing | `src/config.rs` | YAML parser with `${env:VAR}` substitution |
| Shell abstraction | `src/shell.rs` | `ShellFn`, `ShellOutput` type aliases |
| GitHub API client | `src/external_issues/` | GraphQL (Projects V2) + REST (issues/branches) via reqwest |
| OpenCode client | `src/external_agent/` | Health check, agent listing, session start via HTTP |
| Issue source abstraction | `src/issues/` | `ExternalIssueSource` trait + `GitHubIssueSource` impl |
| Workflow orchestration | `src/workflow/mod.rs` | `Workflow` struct — setup/check/triage/todo/review |
| Individual checks | `src/workflow/checks.rs` | `run_triage_check`, `run_todo_check`, `run_review_check` |
| Shared helpers | `src/workflow/helpers.rs` | Prompt loading, repo cloning, field resolution |
| Library exports | `src/lib.rs` | Module declarations + test utilities |
| Embedded assets | `src/assets/` | 6 prompts + 6 agent definitions (via `include_str!`) |

## CODE MAP

| Symbol | Type | Location | Refs | Role |
|--------|------|----------|------|------|
| `serve` | function | `src/main.rs` | 0 | Daemon entry — starts polling loop |
| `check_health` | function | `src/main.rs` | 0 | Health subcommand |
| `Workflow` | struct | `src/workflow/mod.rs` | 1 (main) | Orchestrates all checks |
| `GitHubClient` | struct | `src/external_issues/client.rs` | 16 | GitHub API (GraphQL + REST) |
| `GitHubIssueSource` | struct | `src/issues/github.rs` | 1 | GitHub impl of `ExternalIssueSource` |
| `ExternalIssueSource` | trait | `src/issues/mod.rs` | 1 | Abstraction over issue backends |
| `ExternalIssue` | struct | `src/issues/mod.rs` | 5 | Canonical issue representation |
| `OpenCodeClient` | struct | `src/external_agent/client.rs` | 2 | OpenCode HTTP API client |
| `ProjectConfig` | struct | `src/config.rs` | 8 | Single project config shape |
| `GitAutomateConfig` | struct | `src/config.rs` | 5 | Top-level config shape |
| `ProjectContext` | struct | `src/workflow/helpers.rs` | 5 | Resolved project state |
| `WorkflowContext` | struct | `src/workflow/helpers.rs` | 4 | Injected dependencies |
| `parse_config` | function | `src/config.rs` | 2 | YAML → typed config (+ env sub) |
| `load_config` | function | `src/config.rs` | 1 | Reads `git-automate.yml` from cwd |
| `parse_repository_url` | function | `src/external_issues/repo.rs` | 3 | owner/repo extraction from URL |
| `run_all` | method | `src/workflow/mod.rs` | 2 | Runs all checks in sequence |
| `fill_prompt` | function | `src/workflow/helpers.rs` | 3 | Fills `{{KEY}}` placeholders in templates |
| `REQUIRED_AGENTS` | const | `src/workflow/mod.rs` | 1 | Required OpenCode agent names |

## CONVENTIONS

- **Rust edition**: 2024; `cargo` for build, `cargo test` for tests, `cargo clippy -- -D warnings` for lint, `cargo fmt` for format
- **Env var substitution**: `${env:VAR}` pattern in YAML; unresolved → empty string
- **Logging**: `[git-automate][LEVEL]` prefix — `error`→stderr, `warn`/`info`→stdout
- **Error handling**: `thiserror` for error types, `anyhow` for top-level; `try/catch` per project iteration; `String(error)` for logging
- **GraphQL**: typed response structs for each query (e.g. `NodeProjectResult`)
- **HTTP**: `reqwest::Client` for all API calls (GitHub GraphQL/REST + OpenCode HTTP)
- **Agent/prompt pairing**: assets in `src/assets/prompts/` + `src/assets/agents/` — parallel directories, embedded via `include_str!`
- **ESM imports / import.meta.url / .js extensions**: N/A — Rust uses `mod` declarations and `include_str!`
- **Config**: `issueProvider` field in `git-automate.yml` selects the issue backend (currently `github`); `${env:VAR}` for secrets

## ANTI-PATTERNS (THIS PROJECT)

- **Tests exist**: 174 tests (161 lib + 8 bin + 5 integration) — uses built-in `#[test]` + `wiremock` for integration
- **No linting tools from JS ecosystem**: uses `cargo clippy -- -D warnings` + `cargo fmt`
- **`GITHUB_TOKEN` optional**: if unset, `github` is `None` — checks fail gracefully
- **`catch {}` without binding**: `check_opencode_health` swallows network errors — returns `false` intentionally
- **Assets embedded**: `include_str!` at compile time — no runtime file access for prompts/agents
- **No unit tests on check functions**: `run_triage_check`, `run_todo_check`, `run_review_check` have no covering unit tests — only integration tests via `wiremock`
- **No coverage reporting**: no `cargo tarpaulin`, `cargo-llvm-cov`, or `grcov` configured

## UNIQUE STYLES

- **Agent files**: `src/assets/agents/git-automate-<role>.agent.md` — OpenCode agent definitions with YAML frontmatter (`name`, `description`, `mode: subagent`), embedded via `include_str!`
- **Prompt templates**: `src/assets/prompts/<role>.md` — `{{KEY}}` placeholder syntax, filled by `fill_prompt()` with regex `\{\{\s*([A-Z_]+)\s*\}\}`
- **Workflow status machine** (hardcoded in `src/workflow/mod.rs`):
  `Triage → Todo → In Development → Review Technical → Review Product → QA → Done`
- **Branch naming**: `issue-{ISSUE_NUMBER}` (e.g. `issue-42`)
- **Issue trigger**: titles starting with `@ai ` are picked up by triage check

## COMMANDS
```bash
cargo build          # Build the binary
cargo test           # Run all tests
cargo clippy -- -D warnings  # Lint
cargo fmt --check    # Check formatting
cargo run -- serve --config git-automate.yml  # Run the daemon
```

## NOTES

- CI/CD: `.github/workflows/rust.yml`
- `target/` is gitignored; `.codegraph`, `.omo`, `.idea` also ignored
- OpenCode server uses HTTP Basic auth with username `"opencode"` and password from `${env:OPENCODE_PW}`
- Polling interval: 30s (standalone daemon, no `session.idle` hook)
- Repos cloned to `/tmp/git-automate-work/{owner}-{repo}` with `--depth 1`
- No barrel exports (`lib.rs`); every module uses direct file paths via `mod` declarations
- `bun.lock` was a stale artifact — removed; `pnpm-lock.yaml`, `package.json` all removed
- **Issue source abstraction**: `src/issues/` provides a trait-based abstraction (`ExternalIssueSource`) — GitHub is the only implementation; `issueProvider` config field controls which backend is used
- **CI gaps**: no dependency caching, no cross-platform matrix, no `cargo audit` security check, no `serve()` polling-loop tests
