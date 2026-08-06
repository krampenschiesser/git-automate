# Getting Started

## Prerequisites

- **Rust toolchain** (edition 2024): `rustup` recommended
- **GitHub token** with `repo` and `project` scopes
- **OpenCode server** running and reachable
- **`GITHUB_TOKEN`** and **`OPENCODE_PW`** environment variables

## Installation

```bash
cargo install --path .
```

Or build locally:

```bash
cargo build --release
```

## Configuration

Create a `git-automate.yml` file in your working directory:

```yaml
concurrency: 4
projects:
  my-project:
    repository: https://github.com/owner/repo
    projectId: 1                          # optional; created automatically if absent
    titlePattern: '@ai.*'                 # optional; default is '@ai.*'
    directory: /path/to/repo              # optional; working dir for agent sessions
    issueProvider: github                 # optional; default is 'github'
    opencode:
      url: http://localhost:8081
      pw: ${env:OPENCODE_PW}
```

### Environment variable substitution

The config parser supports `${env:VAR}` syntax. Unresolved variables become empty strings:

```yaml
opencode:
  pw: ${env:OPENCODE_PW}
```

### Required environment variables

| Variable | Required | Description |
|----------|----------|-------------|
| `GITHUB_TOKEN` | yes (for `serve`) | GitHub API token with `repo` + `project` scopes |
| `OPENCODE_PW` | yes | Password for the OpenCode server (referenced via `${env:OPENCODE_PW}`) |

## Running the daemon

```bash
# Set required env vars
export GITHUB_TOKEN=ghp_xxx
export OPENCODE_PW=secret

# Run the daemon (polls every 30s)
git-automate serve --config git-automate.yml

# Check OpenCode server health
git-automate health --url http://localhost:8081 --pw secret
```

## First run

On the first run against a new repository, the daemon will:

1. Create a GitHub Project V2 board (if `projectId` is absent).
2. Add the seven workflow status options (`Triage`, `Todo`, `In Development`, `Review Technical`, `Review Product`, `QA`, `Done`).
3. Add a custom `sessionId` text field.
4. The project ID is written back to the config file automatically.

## Logging

The daemon uses the `tracing` crate. Log format:

```
[git-automate][INFO] Starting git-automate daemon
[git-automate][WARN] OpenCode server at http://localhost:8081 is not healthy
[git-automate][ERROR] Triage check failed for my-project: ...
```

- `ERROR` → stderr
- `WARN` / `INFO` → stdout
