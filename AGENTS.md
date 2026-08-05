# PROJECT KNOWLEDGE BASE

**Generated:** 2026-08-05
**Commit:** d56f333 (main)

## OVERVIEW

Standalone Rust daemon that automates GitHub issue workflows via a polling loop — finds `@ai`-tagged issues, assigns them to project boards, and starts OpenCode agent sessions (triage → developer → review → QA).

## STRUCTURE
```
.
├── src/                 # All Rust source (7 flat files + 3 subdirs)
│   ├── lib.rs           # Library entry point + test utilities
│   ├── main.rs          # Daemon CLI (clap), polling loop, signal handling
│   ├── config.rs        # YAML config + ${env:VAR} substitution
│   ├── log.rs           # Logging abstraction
│   ├── shell.rs         # Shell command execution abstraction
│   ├── github/          # GitHub API client (GraphQL + REST via reqwest)
│   ├── opencode/        # OpenCode HTTP client
│   ├── workflow/        # Workflow engine (helpers, checks, orchestrator)
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
| GitHub API client | `src/github/` | GraphQL (Projects V2) + REST (issues/branches) via reqwest |
| OpenCode client | `src/opencode/` | Health check, agent listing, session start via HTTP |
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
| `GitHubClient` | struct | `src/github/client.rs` | 1 (workflow) | GitHub API (GraphQL + REST) |
| `ProjectConfig` | struct | `src/config.rs` | 8 | Single project config shape |
| `GitAutomateConfig` | struct | `src/config.rs` | 5 | Top-level config shape |
| `ProjectContext` | struct | `src/workflow/helpers.rs` | 5 | Resolved project state |
| `WorkflowDeps` | struct | `src/workflow/mod.rs` | 4 | Injected dependencies |
| `parse_config` | function | `src/config.rs` | 2 | YAML → typed config (+ env sub) |
| `load_config` | function | `src/config.rs` | 1 | Reads `git-automate.yml` from cwd |
| `parse_repository_url` | function | `src/github/repo.rs` | 3 | owner/repo extraction from URL |
| `run_all` | method | `src/workflow/mod.rs` | 2 | Runs all checks in sequence |

## CONVENTIONS

- **Rust edition**: 2024; `cargo` for build, `cargo test` for tests, `cargo clippy -- -D warnings` for lint, `cargo fmt` for format
- **Env var substitution**: `${env:VAR}` pattern in YAML; unresolved → empty string
- **Logging**: `[git-automate][LEVEL]` prefix — `error`→stderr, `warn`/`info`→stdout
- **Error handling**: `thiserror` for error types, `anyhow` for top-level; `try/catch` per project iteration; `String(error)` for logging
- **GraphQL**: typed response structs for each query (e.g. `NodeProjectResult`)
- **HTTP**: `reqwest::Client` for all API calls (GitHub GraphQL/REST + OpenCode HTTP)
- **Agent/prompt pairing**: assets in `src/assets/prompts/` + `src/assets/agents/` — parallel directories, embedded via `include_str!`
- **ESM imports / import.meta.url / .js extensions**: N/A — Rust uses `mod` declarations and `include_str!`

## ANTI-PATTERNS (THIS PROJECT)

- **Tests exist**: 174 tests (161 lib + 8 bin + 5 integration) — uses built-in `#[test]` + `wiremock` for integration
- **No ESLint/Prettier/Biome**: uses `cargo clippy -- -D warnings` + `cargo fmt`
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
- `bun.lock` was a stale artifact — removed; `pnpm-lock.yaml`, `package.json`, `tsconfig.json` all removed
- **CI gaps**: no dependency caching, no cross-platform matrix, no `cargo audit` security check, no `serve()` polling-loop tests
