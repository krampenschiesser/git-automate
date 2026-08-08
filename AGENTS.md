# AGENTS.md — git-automate

Single-crate Rust daemon. Polls GitHub for `@ai`-tagged issues and drives OpenCode agent sessions through a GitHub Projects V2 board. Not a library consumers depend on — it's a standalone binary.

## Toolchain & CI

- Single crate (lib + bin), `edition = "2024"` → **requires Rust 1.85+ stable**.
- No workspace, no `rust-toolchain` file, no custom rustfmt/clippy config (defaults).
- CI (`.github/workflows/rust.yml`, runs on push + PR): **fmt → check → clippy → test**.

## Commands (all verified)

```bash
cargo fmt --check              # format check (CI gate — note --check, not bare fmt)
cargo clippy -- -D warnings    # lint; -D warnings makes warnings fail (CI gate)
cargo test                    # unit + integration tests
cargo build                   # build the binary
```

Run a single test: `cargo test <name>` (works for both unit tests in `src/` and integration tests in `tests/`).

## Module map

| File | Role |
|---|---|
| `src/main.rs` | CLI (`serve` / `health` / `doctor`) + daemon loop (30s poll, SIGINT/SIGTERM graceful exit) |
| `src/lib.rs` | Module declarations + `test_utils` (mock_shell, gh_client, SET_CWD_MUTEX) |
| `src/config.rs` | YAML config + `${env:VAR}` substitution; `deny_unknown_fields`; `projectId` accepts string/number/float; `titlePattern` validated as regex at deserialize |
| `src/shell.rs` | `sh -c` execution; exit code `-1` when the process is killed by a signal |
| `src/workflow/mod.rs` | `Workflow` orchestrator; `run_all()` runs 5 steps (Setup → OpencodeCheck → Triage → Todo → Review), each error-isolated per project |
| `src/workflow/checks.rs` | Triage/todo/review checks; `OpencodeSessionConfig`; concurrency-limit gate before session creation |
| `src/workflow/helpers.rs` | `WorkflowContext`, prompt templates + agent defs (compile-time `include_str!`), `clone_repo_if_needed`, `resolve_project_id` (numeric → global relay ID), `write_project_id` |
| `src/external_agent/opencode/` | OpenCode HTTP client (health, agents, sessions, prompt_async) |
| `src/external_issues/github/` | GitHub GraphQL/REST client; queries live in `queries/*.graphql` |
| `src/assets/` | Prompt templates (`prompts/*.md`) + agent definitions (`agents/*.agent.md`) — embedded via `include_str!` |

## Config (`git-automate.yml`)

- Default filename: `git-automate.yml` (`config::DEFAULT_CONFIG_FILE`). Resolved from `--config` arg, else `cwd`.
- **`projectId` is optional.** If it is numeric (a project *number*, not a relay ID) it is resolved to a GitHub global ID via `projectV2(number:)` and the resolved value is **persisted back** into `git-automate.yml`. Non-numeric values are treated as already-valid global IDs (no network call).
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

## Notes that are easy to miss

- Editing `src/assets/prompts/*.md` or `src/assets/agents/*.agent.md` requires a **rebuild** — they're compiled in via `include_str!`, not loaded at runtime.
- `run_all()` swallows per-project errors (logs + `"<check> failed for {project}: {e}"`); it does not propagate — so a failing project never aborts the daemon. To observe failures, check logs.
- `serve` logs `Startup runAll failed` / `Polling runAll failed` on each error; clean SIGINT/SIGTERM logs `"Received shutdown signal, exiting"`.

See `README.md` for the overview and config example, and `docs/` (mdBook) for deeper documentation.
