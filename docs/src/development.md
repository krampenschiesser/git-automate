# Development

## Build

```bash
cargo build                  # Debug build
cargo build --release        # Release build
cargo install --path .       # Install binary to ~/.cargo/bin
```

## Test

```bash
cargo test                   # Run all tests
cargo test --lib             # Library tests only
cargo test --test integration # Integration tests
cargo test workflow::        # Tests in the workflow module
```

### Test counts

| Category | Count |
|----------|-------|
| Library tests | 161 |
| Binary tests | 8 |
| Integration tests | 5 |
| **Total** | **174** |

## Lint

```bash
cargo clippy -- -D warnings  # Strict linting
cargo fmt --check            # Formatting check
```

## Project structure

```
.
├── src/
│   ├── lib.rs               # Library entry point + test utilities
│   ├── main.rs              # Daemon CLI, polling loop, signal handling
│   ├── config.rs            # YAML config + ${env:VAR} substitution
│   ├── shell.rs             # Shell command execution abstraction
│   ├── external_issues/
│   │   ├── mod.rs
│   │   └── github/
│   │       ├── client.rs    # GitHubClient (GraphQL + REST)
│   │       ├── types.rs     # Typed response structs
│   │       ├── project.rs   # ProjectV2Summary
│   │       ├── repo.rs      # parse_repository_url()
│   │       ├── issues.rs    # IssueInfo
│   │       └── source.rs    # GitHubIssueSource trait impl
│   ├── external_agent/
│   │   ├── mod.rs
│   │   ├── common/
│   │   │   └── mod.rs       # ExternalAgent trait
│   │   └── opencode/
│   │       ├── client.rs    # OpenCodeClient
│   │       ├── types.rs     # Serde response types
│   │       ├── agent.rs     # ExternalAgent impl
│   │       └── mod.rs
│   ├── workflow/
│   │   ├── mod.rs           # Workflow struct, steps, checks dispatch
│   │   ├── checks.rs        # run_triage_check, run_todo_check, run_review_check
│   │   └── helpers.rs       # Context resolution, prompt filling, repo cloning
│   └── assets/
│       ├── prompts/         # 6 prompt templates (triage, developer, reviewer, product, qa, taskmanager)
│       └── agents/          # 6 agent definitions (.agent.md)
├── tests/                   # Integration tests (wiremock)
├── git-automate.yml         # Example config
├── Cargo.toml
└── README.md
```

## Adding a new workflow check

1. Add a new variant to `WorkflowStep` in `src/workflow/mod.rs`.
2. Implement the check as a free function in `src/workflow/checks.rs`.
3. Add the step to `WorkflowStep::all()` and `WorkflowStep::run()`.
4. Add integration tests in `src/workflow/checks.rs` using `wiremock`.

## Adding a new prompt template

1. Create `src/assets/prompts/<name>.md` with `{{KEY}}` placeholders.
2. Add the name to `load_prompt_template()` in `src/workflow/helpers.rs`.
3. Add unit tests verifying the template loads and placeholders fill correctly.

## Running locally

```bash
# Set up env
export GITHUB_TOKEN=ghp_xxx
export OPENCODE_PW=secret

# Build and run
cargo run -- serve --config git-automate.yml

# In another terminal, check health
cargo run -- health --url http://localhost:8081 --pw secret
```

## CI

CI is configured in `.github/workflows/rust.yml`. It runs on push and pull requests:

- `cargo build`
- `cargo test`
- `cargo clippy -- -D warnings`
- `cargo fmt --check`

## Known gaps

- No dependency caching in CI.
- No cross-platform build matrix.
- No `cargo audit` security check.
- No coverage reporting (no `cargo tarpaulin` or `cargo-llvm-cov`).
- `serve()` polling loop has no unit tests.
- `run_triage_check`, `run_todo_check`, `run_review_check` have no unit tests (only integration tests via wiremock).
