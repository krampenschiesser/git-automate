use regex::Regex;
use serde::de::Error as DeError;
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::collections::BTreeMap;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Configuration file not found: {path}")]
    FileNotFound { path: String },
    #[error("Invalid config: {message}")]
    InvalidConfig { message: String },
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("YAML parse error: {0}")]
    YamlParse(#[from] serde_yaml::Error),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OpencodeConfig {
    pub url: String,
    pub pw: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    pub repository: String,
    #[serde(
        rename = "projectId",
        default,
        deserialize_with = "deserialize_project_id"
    )]
    pub project_id: Option<String>,
    pub directory: Option<String>,
    pub opencode: Option<OpencodeConfig>,
    #[serde(rename = "issueProvider", default = "default_issue_provider")]
    pub issue_provider: String,
    #[serde(
        rename = "titlePattern",
        default = "default_title_pattern",
        deserialize_with = "deserialize_title_pattern"
    )]
    pub title_pattern: String,
    #[serde(rename = "trelloApiKey")]
    pub trello_api_key: Option<String>,
    #[serde(rename = "trelloToken")]
    pub trello_token: Option<String>,
    #[serde(rename = "trelloBoardId")]
    pub trello_board_id: Option<String>,
}

fn default_issue_provider() -> String {
    "github".to_string()
}

fn default_title_pattern() -> String {
    "@ai.*".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitAutomateConfig {
    pub projects: BTreeMap<String, ProjectConfig>,
    #[serde(default)]
    pub concurrency: Option<usize>,
}

/// Compiled regex for `${env:VAR}` patterns, cached via `OnceLock`.
fn env_var_pattern() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\$\{env:([A-Za-z_][A-Za-z0-9_]*)\}").expect("hardcoded regex literal is valid")
    })
}

pub fn substitute_env(value: &Value) -> Value {
    match value {
        Value::String(s) => {
            let re = env_var_pattern();
            let result = re.replace_all(s, |caps: &regex::Captures| {
                let key = caps.get(1).map_or("", |m| m.as_str());
                std::env::var(key).unwrap_or_default()
            });
            Value::String(result.into_owned())
        }
        Value::Sequence(seq) => {
            let new_seq: Vec<Value> = seq.iter().map(substitute_env).collect();
            Value::Sequence(new_seq)
        }
        Value::Mapping(map) => {
            let mut new_map = serde_yaml::Mapping::new();
            for (k, v) in map.iter() {
                new_map.insert(k.clone(), substitute_env(v));
            }
            Value::Mapping(new_map)
        }
        other => other.clone(),
    }
}

/// Custom deserializer for `projectId` that accepts a string, integer, or float
/// and returns the value as an `Option<String>`, or `None` for null/missing.
///
/// Equivalent to the manual coercion logic previously in `validate_project`
/// (lines 151-163 of the old implementation).
fn deserialize_project_id<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_yaml::Value::deserialize(deserializer)?;
    if value.is_null() {
        Ok(None)
    } else if let Some(s) = value.as_str() {
        Ok(Some(s.to_string()))
    } else if let Some(i) = value.as_i64() {
        Ok(Some(i.to_string()))
    } else if let Some(f) = value.as_f64() {
        Ok(Some(f.to_string()))
    } else {
        Err(DeError::custom("projectId must be a string or number"))
    }
}

/// Custom deserializer for `titlePattern` that validates the value compiles
/// as a [`Regex`], returning a deserialization error if it does not.
fn deserialize_title_pattern<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    Regex::new(&s).map_err(|e| DeError::custom(format!("invalid title_pattern '{s}': {e}")))?;
    Ok(s)
}

pub fn parse_config(file_path: &Path) -> Result<GitAutomateConfig, ConfigError> {
    let content = std::fs::read_to_string(file_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ConfigError::FileNotFound {
                path: file_path.to_string_lossy().to_string(),
            }
        } else {
            ConfigError::Io(e)
        }
    })?;
    let parsed: Value = serde_yaml::from_str(&content)?;
    let substituted = substitute_env(&parsed);
    let config: GitAutomateConfig = serde_yaml::from_value(substituted)?;
    Ok(config)
}

/// Default config file name used by [`load_config`].
pub const DEFAULT_CONFIG_FILE: &str = "git-automate.yml";

pub fn load_config() -> Result<GitAutomateConfig, ConfigError> {
    let cwd = std::env::current_dir().map_err(ConfigError::Io)?;
    let file_path = cwd.join(DEFAULT_CONFIG_FILE);
    parse_config(&file_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    // Env var names used in substitution tests (GA_TEST_* prefix).
    const ENV_HELLO: &str = "GA_TEST_HELLO";
    const ENV_UNSET_VAR: &str = "GA_TEST_UNSET_VAR";
    const ENV_NESTED: &str = "GA_TEST_NESTED";
    const ENV_ARR: &str = "GA_TEST_ARR";
    const ENV_MULTI_A: &str = "GA_TEST_MULTI_A";
    const ENV_MULTI_B: &str = "GA_TEST_MULTI_B";
    const ENV_UNDERSCORE_VAR: &str = "GA_TEST_UNDERSCORE_VAR";
    const ENV_PW: &str = "GA_TEST_PW";
    const ENV_TRELLO_KEY: &str = "GA_TEST_TRELLO_KEY";
    const ENV_TRELLO_TOKEN: &str = "GA_TEST_TRELLO_TOKEN";

    // --- substitute_env tests ---

    // Test 1: substitute_env with ${env:VAR} where VAR="hello" → "hello"
    #[test]
    fn substitute_env_replaces_existing_var() {
        unsafe {
            std::env::set_var(ENV_HELLO, "hello");
        }
        let input = Value::String(format!("${{env:{}}}", ENV_HELLO));
        let result = substitute_env(&input);
        assert_eq!(result, Value::String("hello".to_string()));
    }

    // Test 2: substitute_env with ${env:VAR} where VAR unset → ""
    #[test]
    fn substitute_env_unset_var_becomes_empty() {
        unsafe {
            std::env::remove_var(ENV_UNSET_VAR);
        }
        let input = Value::String(format!("${{env:{}}}", ENV_UNSET_VAR));
        let result = substitute_env(&input);
        assert_eq!(result, Value::String(String::new()));
    }

    // Test 3: substitute_env with no ${env:} pattern → unchanged
    #[test]
    fn substitute_env_no_pattern_unchanged() {
        let input = Value::String("just a plain string".to_string());
        let result = substitute_env(&input);
        assert_eq!(result, Value::String("just a plain string".to_string()));
    }

    // Test 4: substitute_env on mapping with nested ${env:VAR} → recursively substitutes
    #[test]
    fn substitute_env_mapping_recursive() {
        unsafe {
            std::env::set_var(ENV_NESTED, "deep_value");
        }
        let mapping =
            serde_yaml::from_str::<Value>(&format!("key: ${{env:{}}}\n", ENV_NESTED)).unwrap();
        let result = substitute_env(&mapping);
        let expected = serde_yaml::from_str::<Value>("key: deep_value\n").unwrap();
        assert_eq!(result, expected);
    }

    // Test 5: substitute_env on array with ${env:VAR} → recursively substitutes each element
    #[test]
    fn substitute_env_array_recursive() {
        unsafe {
            std::env::set_var(ENV_ARR, "arr_val");
        }
        let seq = Value::Sequence(vec![
            Value::String(format!("${{env:{}}}", ENV_ARR)),
            Value::String("literal".to_string()),
        ]);
        let result = substitute_env(&seq);
        let expected = Value::Sequence(vec![
            Value::String("arr_val".to_string()),
            Value::String("literal".to_string()),
        ]);
        assert_eq!(result, expected);
    }

    // Test 6: substitute_env with multiple ${env:VAR} in one string → all replaced
    #[test]
    fn substitute_env_multiple_in_string() {
        unsafe {
            std::env::set_var(ENV_MULTI_A, "A");
        }
        unsafe {
            std::env::set_var(ENV_MULTI_B, "B");
        }
        let input = Value::String(format!(
            "${{env:{}}} and ${{env:{}}}",
            ENV_MULTI_A, ENV_MULTI_B
        ));
        let result = substitute_env(&input);
        assert_eq!(result, Value::String("A and B".to_string()));
    }

    // Test 14: substitute_env with ${env:VAR_NAME} (underscore) → matches pattern
    #[test]
    fn substitute_env_underscore_var_name() {
        unsafe {
            std::env::set_var(ENV_UNDERSCORE_VAR, "underscored");
        }
        let input = Value::String(format!("${{env:{}}}", ENV_UNDERSCORE_VAR));
        let result = substitute_env(&input);
        assert_eq!(result, Value::String("underscored".to_string()));
    }

    // --- parse_config tests ---

    // Test 7: parse_config with valid YAML → returns GitAutomateConfig with correct projects
    #[test]
    fn parse_config_valid_yaml() {
        let yaml = r#"
projects:
  my-repo:
    repository: https://github.com/user/repo
    projectId: 42
    directory: src
    opencode:
      url: http://localhost:8081
      pw: secret123
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        assert_eq!(config.projects.len(), 1);
        let project = config.projects.get("my-repo").unwrap();
        assert_eq!(project.repository, "https://github.com/user/repo");
        assert_eq!(project.project_id.as_deref(), Some("42"));
        assert_eq!(project.directory.as_deref(), Some("src"));
        assert_eq!(
            project.opencode.as_ref().unwrap().url,
            "http://localhost:8081"
        );
        assert_eq!(project.opencode.as_ref().unwrap().pw, "secret123");
    }

    // Test 8: parse_config with missing file → ConfigError::FileNotFound
    #[test]
    fn parse_config_missing_file() {
        let result = parse_config(Path::new("/nonexistent/path/to/config.yml"));
        assert!(matches!(result, Err(ConfigError::FileNotFound { .. })));
    }

    // Test 12: parse_config with config missing opencode.url → YamlParse error
    #[test]
    fn parse_config_missing_opencode_url() {
        let yaml = r#"
projects:
  bad-repo:
    repository: https://github.com/user/repo
    opencode:
      pw: secret123
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let result = parse_config(tmp.path());
        assert!(matches!(result, Err(ConfigError::YamlParse { .. })));
    }

    // --- parse_config error tests ---

    // Test 9: parse_config with non-string repository → ConfigError::YamlParse
    #[test]
    fn validate_project_non_string_repository() {
        let yaml = r#"
projects:
  bad-repo:
    repository: 12345
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let result = parse_config(tmp.path());
        assert!(matches!(result, Err(ConfigError::YamlParse { .. })));
    }

    // Test 10: parse_config with projectId as number (e.g. 42) → coerced to String("42")
    #[test]
    fn validate_project_number_project_id_coerced() {
        let yaml = r#"
projects:
  num-repo:
    repository: https://github.com/user/repo
    projectId: 42
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        let project = config.projects.get("num-repo").unwrap();
        assert_eq!(project.project_id.as_deref(), Some("42"));
    }

    // Test 11: parse_config with projectId as string → stays as string
    #[test]
    fn validate_project_string_project_id_preserved() {
        let yaml = r#"
projects:
  str-repo:
    repository: https://github.com/user/repo
    projectId: "P-123"
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        let project = config.projects.get("str-repo").unwrap();
        assert_eq!(project.project_id.as_deref(), Some("P-123"));
    }

    // --- load_config tests ---

    // Test 13: load_config reads from cwd's git-automate.yml → returns config
    #[tokio::test]
    async fn load_config_reads_from_cwd() {
        let yaml = r#"
projects:
  cwd-repo:
    repository: https://github.com/cwd/repo
    opencode:
      url: http://localhost:8081
      pw: ${env:GA_TEST_PW}
"#;
        let tmp_dir = tempfile::tempdir().unwrap();
        let config_path = tmp_dir.path().join("git-automate.yml");
        std::fs::write(&config_path, yaml).unwrap();

        unsafe {
            std::env::set_var(ENV_PW, "testpassword");
        }

        // Change to the temp directory so load_config finds git-automate.yml
        let _guard = crate::test_utils::SET_CWD_MUTEX.lock().await;
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp_dir.path()).unwrap();
        let result = load_config();
        // Restore the original directory regardless of test outcome
        std::env::set_current_dir(&original_dir).unwrap();

        let config = result.unwrap();
        let project = config.projects.get("cwd-repo").unwrap();
        assert_eq!(project.repository, "https://github.com/cwd/repo");
        assert_eq!(project.opencode.as_ref().unwrap().pw, "testpassword");
    }

    // --- issueProvider config tests ---

    // Test 15: parse_config with issueProvider: github → issue_provider = "github"
    #[test]
    fn parse_config_with_issue_provider_github() {
        let yaml = r#"
projects:
  my-repo:
    repository: https://github.com/user/repo
    issueProvider: github
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        let project = config.projects.get("my-repo").unwrap();
        assert_eq!(project.issue_provider, "github");
    }

    // Test 16: parse_config without issueProvider → defaults to "github"
    #[test]
    fn parse_config_without_issue_provider_defaults_to_github() {
        let yaml = r#"
projects:
  my-repo:
    repository: https://github.com/user/repo
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        let project = config.projects.get("my-repo").unwrap();
        assert_eq!(project.issue_provider, "github");
    }

    // Test 17: parse_config with custom issueProvider → stored correctly
    #[test]
    fn parse_config_with_custom_issue_provider() {
        let yaml = r#"
projects:
  my-repo:
    repository: https://github.com/user/repo
    issueProvider: jira
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        let project = config.projects.get("my-repo").unwrap();
        assert_eq!(project.issue_provider, "jira");
    }

    // Test 18: ProjectConfig serde round-trip preserves issueProvider
    #[test]
    fn project_config_serde_round_trip_issue_provider() {
        let pc = ProjectConfig {
            repository: "https://github.com/user/repo".to_string(),
            project_id: None,
            directory: None,
            opencode: None,
            issue_provider: "github".to_string(),
            title_pattern: "@ai.*".to_string(),
            trello_api_key: None,
            trello_token: None,
            trello_board_id: None,
        };
        let yaml = serde_yaml::to_string(&pc).unwrap();
        let parsed: ProjectConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.issue_provider, "github");
    }

    // Test: parse_config with issueProvider: trello + trello fields + env var substitution
    #[test]
    fn parse_config_with_trello_provider_and_credentials() {
        unsafe {
            std::env::set_var(ENV_TRELLO_KEY, "secret-key");
            std::env::set_var(ENV_TRELLO_TOKEN, "secret-token");
        }
        let yaml = format!(
            "
projects:
  trello-board:
    repository: https://github.com/user/repo
    issueProvider: trello
    titlePattern: \"@ai.*\"
    trelloApiKey: ${{env:{}}}
    trelloToken: ${{env:{}}}
    trelloBoardId: BRD-123
",
            ENV_TRELLO_KEY, ENV_TRELLO_TOKEN
        );
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        let project = config.projects.get("trello-board").unwrap();
        assert_eq!(project.issue_provider, "trello");
        assert_eq!(project.trello_api_key.as_deref(), Some("secret-key"));
        assert_eq!(project.trello_token.as_deref(), Some("secret-token"));
        assert_eq!(project.trello_board_id.as_deref(), Some("BRD-123"));
    }

    // --- concurrency config tests ---

    // Test: parse_config with concurrency: 4 → config.concurrency == Some(4)
    #[test]
    fn parse_config_with_concurrency() {
        let yaml = r#"
concurrency: 4
projects:
  my-repo:
    repository: https://github.com/user/repo
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        assert_eq!(config.concurrency, Some(4));
    }

    // Test: parse_config without concurrency → defaults to None
    #[test]
    fn parse_config_without_concurrency_defaults_to_none() {
        let yaml = r#"
projects:
  my-repo:
    repository: https://github.com/user/repo
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        assert_eq!(config.concurrency, None);
    }

    // Test: parse_config with concurrency: 0 → config.concurrency == Some(0)
    #[test]
    fn parse_config_with_concurrency_zero() {
        let yaml = r#"
concurrency: 0
projects:
  my-repo:
    repository: https://github.com/user/repo
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        assert_eq!(config.concurrency, Some(0));
    }

    // Test: git_automate_config_serde_round_trip_concurrency
    #[test]
    fn git_automate_config_serde_round_trip_concurrency() {
        let config = GitAutomateConfig {
            projects: BTreeMap::new(),
            concurrency: Some(4),
        };
        let yaml = serde_yaml::to_string(&config).unwrap();
        let parsed: GitAutomateConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.concurrency, Some(4));
    }
}
