# Config API

## Types

### `GitAutomateConfig`

Top-level configuration.

```rust
pub struct GitAutomateConfig {
    pub projects: BTreeMap<String, ProjectConfig>,
    pub concurrency: Option<usize>,
}
```

| Field | Type | Description |
|-------|------|-------------|
| `projects` | `BTreeMap<String, ProjectConfig>` | Map of project names to configs |
| `concurrency` | `Option<usize>` | Max active OpenCode sessions. `None` = no limit. |

### `ProjectConfig`

Configuration for a single project.

```rust
pub struct ProjectConfig {
    pub repository: String,
    pub project_id: Option<String>,
    pub directory: Option<String>,
    pub opencode: Option<OpencodeConfig>,
    pub issue_provider: String,
    pub title_pattern: String,
    pub trello_api_key: Option<String>,
    pub trello_token: Option<String>,
    pub trello_board_id: Option<String>,
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `repository` | `String` | yes | GitHub repo URL or `owner/repo` |
| `project_id` | `Option<String>` | no | Project V2 ID. Auto-created if absent. |
| `directory` | `Option<String>` | no | Agent session working directory |
| `opencode` | `Option<OpencodeConfig>` | no | OpenCode connection config |
| `issue_provider` | `String` | no | Issue backend: `github` (default) or `trello` |
| `title_pattern` | `String` | no | Regex for triage matching. Default: `@ai.*` |
| `trello_api_key` | `Option<String>` | no | Trello API key |
| `trello_token` | `Option<String>` | no | Trello token |
| `trello_board_id` | `Option<String>` | no | Trello board ID |

### `OpencodeConfig`

OpenCode server connection details.

```rust
pub struct OpencodeConfig {
    pub url: String,
    pub pw: String,
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `url` | `String` | yes | OpenCode server base URL |
| `pw` | `String` | yes | Server password (use `${env:VAR}`) |

## Parsing

### `parse_config(path: &Path) -> Result<GitAutomateConfig, ConfigError>`

Parses a YAML config file with `${env:VAR}` substitution.

**Errors:**

| Error | Cause |
|-------|-------|
| `ConfigError::FileNotFound` | Config file does not exist |
| `ConfigError::InvalidConfig` | Missing required fields |
| `ConfigError::Io` | File read error |
| `ConfigError::YamlParse` | YAML syntax error or type mismatch |

### `load_config() -> Result<GitAutomateConfig, ConfigError>`

Loads config from `<cwd>/git-automate.yml`.

## Custom deserializers

- **`projectId`**: accepts string, integer, or float; coerces to `Option<String>`.
- **`titlePattern`**: validated as a regex at parse time; invalid patterns produce a `YamlParse` error.
