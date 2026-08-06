# Configuration

## Top-level structure

```yaml
concurrency: 4                              # optional; max active OpenCode sessions
projects:
  <project-name>:
    repository: https://github.com/owner/repo
    projectId: 1                            # optional; auto-created if absent
    titlePattern: '@ai.*'                   # optional; default '@ai.*'
    directory: /path/to/repo                # optional; agent session working directory
    issueProvider: github                   # optional; default 'github'
    opencode:
      url: http://localhost:8081
      pw: ${env:OPENCODE_PW}
```

## Field reference

### `concurrency`

Maximum number of active OpenCode agent sessions across all projects. When the active count reaches this limit, new session creation is skipped with a warning. Defaults to no limit.

### `projects`

A map of project names to `ProjectConfig`. The key is an arbitrary identifier used in logs.

### `ProjectConfig`

| Field | Required | Type | Description |
|-------|----------|------|-------------|
| `repository` | yes | `string` | GitHub repo URL or `owner/repo` shorthand |
| `projectId` | no | `string\|number` | GitHub Project V2 ID. Auto-created if missing. |
| `titlePattern` | no | `string` | Regex matching issue titles for triage. Default: `@ai.*` |
| `directory` | no | `string` | Working directory for OpenCode agent sessions. Falls back to cloned repo path. |
| `issueProvider` | no | `string` | Issue backend. Currently `github` (default) or `trello`. |
| `opencode.url` | yes | `string` | OpenCode server base URL |
| `opencode.pw` | yes | `string` | OpenCode server password. Use `${env:VAR}` for secrets. |
| `trelloApiKey` | no | `string` | Trello API key (required when `issueProvider: trello`) |
| `trelloToken` | no | `string` | Trello token |
| `trelloBoardId` | no | `string` | Trello board ID |

## Environment variable substitution

The YAML parser replaces `${env:VAR_NAME}` patterns before deserialization. The regex is:

```
\$\{env:([A-Za-z_][A-Za-z0-9_]*)\}
```

- Resolved variables are substituted with their value.
- Unresolved variables become empty strings.
- Substitution is recursive across strings, mappings, and sequences.

## Config file discovery

- `git-automate serve --config <path>` reads from the given path.
- `git-automate health` does not read a config file.

## Project ID persistence

When `projectId` is absent, the daemon creates a new Project V2 on GitHub and writes the ID back to the config file on disk. Subsequent runs reuse the stored ID.
