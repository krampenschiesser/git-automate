# Architecture Overview

## High-level diagram

```
┌─────────────────────────────────────────────────────────────┐
│                        git-automate                         │
│                                                             │
│  ┌──────────┐    ┌──────────────┐    ┌──────────────────┐  │
│  │  main.rs │───▶│   Workflow   │───▶│  workflow/       │  │
│  │  (CLI)   │    │   (polling)  │    │  checks.rs       │  │
│  └──────────┘    └──────────────┘    └──────────────────┘  │
│                            │                  │             │
│                            ▼                  ▼             │
│                    ┌──────────────┐    ┌──────────────────┐  │
│                    │  config.rs   │    │  helpers.rs      │  │
│                    │ (YAML + env) │    │  (prompts, clone)│  │
│                    └──────────────┘    └──────────────────┘  │
│                                                             │
│  ┌──────────────────────┐    ┌──────────────────────────┐   │
│  │ external_issues/     │    │ external_agent/          │   │
│  │  github/client.rs    │    │  opencode/client.rs      │   │
│  │  (GraphQL + REST)    │    │  (HTTP Basic auth)       │   │
│  └──────────────────────┘    └──────────────────────────┘   │
│                                                             │
│  ┌──────────────────────────────────────────────────────┐   │
│  │  assets/prompts/  (6 templates with {{KEY}} placeholders) │
│  │  assets/agents/   (6 agent definitions)              │   │
│  └──────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────┘
          │                              │
          ▼                              ▼
   ┌─────────────┐              ┌─────────────────┐
   │  GitHub API │              │  OpenCode Server │
   │  (GraphQL)  │              │  (HTTP REST)     │
   └─────────────┘              └─────────────────┘
```

## Module map

| Module | File | Role |
|--------|------|------|
| `main` | `src/main.rs` | CLI (clap), polling loop, signal handling, tracing init |
| `config` | `src/config.rs` | YAML parsing, `${env:VAR}` substitution, type validation |
| `shell` | `src/shell.rs` | Shell command execution (`sh -c`), `ShellFn` type |
| `external_issues` | `src/external_issues/` | GitHub API client (GraphQL + REST) |
| `external_agent` | `src/external_agent/` | Provider-agnostic `ExternalAgent` trait + OpenCode impl |
| `workflow` | `src/workflow/` | Workflow orchestrator, three check functions, helpers |
| `assets/prompts` | `src/assets/prompts/` | 6 embedded prompt templates (`include_str!`) |
| `assets/agents` | `src/assets/agents/` | 6 embedded agent definitions (`include_str!`) |

## Key types

- **`WorkflowContext`** — injected dependencies: config, optional GitHub client, shell function
- **`ProjectContext`** — fully resolved project state: name, config, owner, repo, project ID
- **`OpencodeSessionConfig`** — OpenCode connection details + optional directory
- **`WorkflowError`** — error enum for workflow operations
- **`GitHubError`** — error enum for GitHub API operations
- **`OpenCodeError`** — error enum for OpenCode HTTP operations

## Design patterns

- **Dependency injection**: `WorkflowContext` holds all external dependencies, making tests straightforward.
- **Error isolation**: each `run_*_check` wraps per-project work in `try/catch`; errors are logged, never propagated.
- **Idempotency**: checks skip items that already have a session ID stored in the project.
- **Asset embedding**: prompts and agent definitions use `include_str!` — no runtime file access needed.
- **Provider abstractions**: `ExternalAgent` and `ExternalIssueSource` traits allow swapping backends.
