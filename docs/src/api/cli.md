# CLI Reference

## Commands

### `serve`

Run the git-automate daemon. Polls GitHub and OpenCode every 30 seconds.

```bash
git-automate serve [OPTIONS]
```

**Options:**

| Option | Default | Description |
|--------|---------|-------------|
| `--config <path>` | `git-automate.yml` | Path to the configuration file |

**Environment variables:**

| Variable | Required | Description |
|----------|----------|-------------|
| `GITHUB_TOKEN` | yes | GitHub API token |

**Exit codes:**

| Code | Meaning |
|------|---------|
| 0 | Daemon exited cleanly (signal received) |
| 1 | Config error or missing `GITHUB_TOKEN` |

### `health`

Check OpenCode server health.

```bash
git-automate health --url <url> --pw <password>
```

**Options:**

| Option | Description |
|--------|-------------|
| `--url <url>` | OpenCode server base URL (e.g. `http://localhost:8081`) |
| `--pw <password>` | OpenCode server password |

**Exit codes:**

| Code | Meaning |
|------|---------|
| 0 | Server is healthy |
| 1 | Server is not healthy or unreachable |

## Examples

```bash
# Run daemon with default config
GITHUB_TOKEN=ghp_xxx OPENCODE_PW=secret git-automate serve

# Run daemon with custom config path
GITHUB_TOKEN=ghp_xxx git-automate serve --config /etc/git-automate.yml

# Check OpenCode health
git-automate health --url http://localhost:8081 --pw secret
```
