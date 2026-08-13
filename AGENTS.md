# PROJECT KNOWLEDGE BASE

**Generated:** 2026-08-13
**Commit:** `6b4e3e2` (feat/github-token-config)
**Branch:** `feat/github-token-config`

## OVERVIEW
Single-crate Rust daemon (`edition = "2024"`, req. Rust 1.85+). Polls GitHub for `@ai`-tagged issues and drives OpenCode agent sessions through a GitHub Projects V2 board. Standalone binary — not a consumer library.

## STRUCTURE
```
{root}/
├── src/
│   ├── main.rs          (831)  CLI (serve/health/doctor) + daemon loop
│   ├── lib.rs           (67)   Module decls + test_utils
│   ├── config.rs        (740)  YAML config + ${env:VAR} substitution
│   ├── shell.rs         (167)  sh -c execution wrapper
│   ├── assets/          (12)   Embedded via include_str!
│   │   ├── agents/      (6)    *.agent.md (triage/taskmanager/dev/reviewer/product/qa)
│   │   └── prompts/     (6)    *.md prompt templates
│   ├── workflow/        (3, 5710 lines)   See AGENTS.md
│   ├── external_agent/  (6 files)
│   │   ├── common/      (1)    ExternalAgent trait (provider-agnostic)
│   │   └── opencode/    (4)    OpenCode HTTP client — See AGENTS.md
│   └── external_issues/ (8 files)
│       └── github/      (7 + 20 queries)  See AGENTS.md
├── tests/               (3 files)          See AGENTS.md
├── docs/                (mdBook)
├── Cargo.toml
├── git-automate.yml     (sample config)
└── AGENTS.md
```

## WHERE TO LOOK
| Task | Location | Notes |
|---|---|---|
| Add CLI subcommand | `src/main.rs` | Clap derive, `#[tokio::main]`, SIGINT/SIGTERM |
| Change config schema | `src/config.rs` | `${env:VAR}` subst, regex validation, `deny_unknown_fields` |
| Modify workflow steps | `src/workflow/` | `mod.rs` runs 5 steps; errors swallowed |
| Add OpenCode API call | `src/external_agent/opencode/client.rs` | HTTP client, `include_str!` queries |
| Add GitHub GraphQL query | `src/external_issues/github/queries/` + `client.rs` + `types.rs` | 4-step pattern: .graphql → struct → method → test |
| Edit agent prompts | `src/assets/prompts/*.md` | Requires `cargo build` (include_str!) |
| Edit agent definitions | `src/assets/agents/*.agent.md` | Copied to `~/.opencode/agents/` by `doctor` |
| Add integration test | `tests/` | Helpers in `tests/common/mod.rs`, use `SET_CWD_MUTEX` |
| Debug test flakiness | `src/lib.rs` | `SET_CWD_MUTEX` guards cwd mutations |
| Update CI pipeline | `.github/workflows/rust.yml` | fmt → check → clippy → test |

## CODE MAP
| Symbol | Type | Location | Role |
|---|---|---|---|
| `Workflow` | struct | `workflow/mod.rs` | Orchestrator; `run_all()` runs 5 error-isolated steps |
| `WorkflowStep` | enum | `workflow/mod.rs` | Setup→OpencodeCheck→Triage→Todo→Review |
| `WorkflowStatus` | enum | `workflow/mod.rs` | 7 statuses: Triage→Todo→In Dev→Review Tech→Review Prod→QA→Done |
| `AgentName` | enum | `workflow/mod.rs` | 6 agents: triage, taskmanager, developer, reviewer, product, qa |
| `WorkflowError` | enum | `workflow/helpers.rs` | Error type; per-project errors never propagate |
| `WorkflowContext` | struct | `workflow/helpers.rs` | Config + GitHub client + shell fn |
| `OpencodeSessionConfig` | struct | `workflow/checks.rs` | Session creation config + concurrency gate |
| `ExternalAgent` | trait | `external_agent/common/mod.rs` | Provider-agnostic: `create_session`, `get_session`, `list_agents` |
| `OpenCodeClient` | struct | `external_agent/opencode/client.rs` | Concrete `ExternalAgent` impl via HTTP |
| `GitHubClient` | struct | `external_issues/github/client.rs` | GraphQL/REST; 17 `include_str!` queries |
| `GitAutomateConfig` | struct | `config.rs` | Top-level YAML config with `${env:VAR}` |
| `ShellFn` | type alias | `shell.rs` | `Arc<dyn Fn(String) -> ...>` |
| `SET_CWD_MUTEX` | static | `lib.rs` | Serializes cwd-mutating tests |

## COMMANDS
```bash
cargo fmt --check              # CI gate (--check, not bare fmt)
cargo clippy -- -D warnings    # CI gate (-D warnings)
cargo test                    # unit + integration tests
cargo build                   # build binary
cargo test <name>             # single test (unit or integration)
git-automate serve --config git-automate.yml   # daemon, 30s poll
git-automate health --url <u> --pw <p>          # probe OpenCode server
git-automate doctor --config git-automate.yml   # install missing agents
```

## Config (`git-automate.yml`)

- Default filename: `git-automate.yml` (`config::DEFAULT_CONFIG_FILE`). Resolved from `--config` arg, else `cwd`.
- **`projectId` is optional.** If it is numeric (a project *number*, not a relay ID) it is **resolved to a GitHub global ID at runtime** via `projectV2(number:)` — the original numeric value is **kept** in `git-automate.yml` (not replaced). Non-numeric values are treated as already-valid global IDs (no network call).
- **`${env:VAR}`** interpolation runs on every string field (recursing into maps/sequences). Unset vars become empty strings.
- **`.env`** is loaded via `dotenv()` at startup; vars already in the environment take precedence over the file.
- `concurrency` (top-level) caps active OpenCode sessions — when the limit is reached session creation is skipped with a warning.

## Required environment

- `GITHUB_TOKEN` — **fail-fast in `serve`** (exits if missing/empty); **optional in `doctor`** (warns, skips GitHub checks). Referenced in `git-automate.yml` via `${env:GITHUB_TOKEN}` for authenticated clones.
- `OPENCODE_PW` — OpenCode server password; referenced in config via `${env:OPENCODE_PW}`.

## CLI

```
git-automate serve --config git-automate.yml      # daemon, polls every 30s
git-automate health --url <url> --pw <pw>          # probe OpenCode server health
git-automate doctor --config git-automate.yml      # one-shot setup; copies missing agents to ~/.opencode/agents/
```

## Agent & workflow model

- **6 required OpenCode agents** (embedded + copied by `doctor`): `git-automate-triage`, `git-automate-taskmanager`, `git-automate-developer`, `git-automate-reviewer`, `git-automate-product`, `git-automate-qa`.
- **7 statuses**: `Triage → Todo → In Development → Review Technical → Review Product → QA → Done`.
- Project setup is idempotent: missing status options and the `sessionId` text field are added as needed; an existing project with all options + field is a no-op.
- Repos are shallow-cloned to `/tmp/git-automate-work/{owner}-{repo}`; existing clones are skipped.

## Test conventions

- **Mocking**: `wiremock` for both GitHub GraphQL (`/graphql`) and OpenCode (`/global/health`, `/agent`, `/session`, …). `tempfile` for on-disk config.
- **`SET_CWD_MUTEX`** (`lib.rs::test_utils`): any test that calls `write_project_id`, `clone_repo_if_needed`, or `load_config` mutates `cwd` — it **must** acquire this lock and restore the original directory afterward. This is the most common cause of test flakiness.
- **Mock disambiguation**: GitHub mocks use `body_string_contains` on unique substrings (`user(login:`, `createProjectV2(input`, `field(name:`, `fields(first:`, `updateProjectV2Field`, `createProjectV2Field`) — follow the same pattern for new GraphQL tests.
- **Shared fixtures**: integration tests live in `tests/` with helpers in `tests/common/mod.rs` (`mount_github_graphql_mocks`, `mount_opencode_mocks`, `make_deps`, `gh_client`, …).
- Tests numbered `T#` in comments (e.g. `T1`, `T7`).

## ANTI-PATTERNS (THIS PROJECT)
- `run_all()` swallows per-project errors (logs + `"<check> failed for {project}: {e}"`); return value is always `Ok(())` — check logs, not return value
- Editing `src/assets/prompts/*.md` or `src/assets/agents/*.agent.md` requires a **rebuild** — `include_str!` is compile-time
- `SET_CWD_MUTEX` must be acquired before any cwd-mutating test (`write_project_id`, `clone_repo_if_needed`, `load_config`); restore original dir afterward
- `GITHUB_TOKEN` fail-fast in `serve`; optional in `doctor` (warns, skips GitHub checks)
- Shell exit code `-1` means both signal-kill and spawn failure (distinguish via non-empty stderr)
- `${env:VAR}` for unset vars becomes empty string (no error); `.env` loaded via `dotenv()`, env takes precedence
- `projectId` numeric → resolved to global ID at runtime via `projectV2(number:)`; original value kept in config
- `deny_unknown_fields` — unknown YAML keys cause parse errors; `titlePattern` validated as regex at deserialize
- Mock disambiguation: GitHub mocks use `body_string_contains` on unique substrings (e.g. `user(login:`, `createProjectV2(input`, `field(name:`)
- `concurrency` cap skips session creation with a warning (not an error) when limit reached
- GitHub client `None` → each check logs `warn` and returns `Ok(())` (intentional, e.g. `doctor` mode)

## Subdirectory AGENTS
| Path | Scope |
|---|---|
| `src/workflow/AGENTS.md` | Workflow engine: `Workflow`, `WorkflowStep`, `run_all()`, error isolation, 5-step pipeline |
| `src/external_issues/github/AGENTS.md` | GitHub GraphQL/REST client: `.graphql` files, `include_str!`, 4-step query pattern, `types.rs` |
| `src/external_agent/opencode/AGENTS.md` | OpenCode HTTP client: `ExternalAgent` trait impl, sessions, agents, basic auth |
| `tests/AGENTS.md` | Integration test conventions: `SET_CWD_MUTEX`, mock helpers, wiremock patterns, T# numbering |

## Notes that are easy to miss

- `serve` logs `Startup runAll failed` / `Polling runAll failed` on each error; clean SIGINT/SIGTERM logs `"Received shutdown signal, exiting"`.
- Repos are shallow-cloned to `/tmp/git-automate-work/{owner}-{repo}`; existing clones are skipped.
- `config.rs` is the foundation — it depends on no other module. All other modules depend on it.
- No unified top-level error enum; each module defines its own. `main.rs` uses `Box<dyn std::error::Error>` as catch-all.

See `README.md` for the overview and config example, and `docs/` (mdBook) for deeper documentation. Subdirectory AGENTS.md files provide module-specific details.
