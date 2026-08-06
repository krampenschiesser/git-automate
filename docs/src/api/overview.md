# API Reference Overview

This section documents the public API of `git-automate`.

## Contents

- [CLI](./cli.md) — Command-line interface reference
- [Config](./config.md) — Configuration types and parsing
- [WorkflowError](./workflow-error.md) — Error types returned by the workflow engine

## Module structure

```
git_automate::
├── config           # Config parsing and types
├── shell            # Shell execution abstraction
├── external_agent   # Agent session management
│   ├── common       # Provider-agnostic trait
│   └── opencode     # OpenCode HTTP client
├── external_issues  # GitHub API client
│   └── github       # GitHub-specific implementation
└── workflow         # Workflow orchestration
    ├── checks       # Triage, todo, review checks
    └── helpers      # Shared utilities
```

## Testing

The project ships with 174 tests (161 lib + 8 bin + 5 integration) using `wiremock` for HTTP mocking.

```bash
cargo test          # Run all tests
cargo test --lib    # Run library tests only
cargo test --test integration  # Run integration tests
```
