use regex::Regex;
use serde::de::Error as DeError;
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::collections::HashMap;
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
    pub cwd: String,
    pub project: String,
    #[serde(default)]
    pub concurrency: HashMap<String, usize>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitSection {
    pub repository: String,
    #[serde(
        rename = "projectId",
        default,
        deserialize_with = "deserialize_project_id"
    )]
    pub project_id: Option<String>,
    #[serde(deserialize_with = "deserialize_directory")]
    pub directory: String,
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
    #[serde(default)]
    pub token: Option<String>,
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
    pub git: GitSection,
    pub opencode: Option<OpencodeConfig>,
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

/// Rejects an empty/whitespace `directory` (catches an unset `${env:VAR}`
/// which substitutes to `""`).
fn deserialize_directory<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    if s.trim().is_empty() {
        return Err(DeError::custom("git.directory must be a non-empty path"));
    }
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

/// Resolve the config file path: prefer `GIT_AUTOMATE_CONFIG` env var,
/// fall back to [`DEFAULT_CONFIG_FILE`] in the current directory.
pub fn resolve_config_path() -> Result<std::path::PathBuf, ConfigError> {
    if let Ok(path_str) = std::env::var("GIT_AUTOMATE_CONFIG") {
        return Ok(std::path::PathBuf::from(path_str));
    }
    let cwd = std::env::current_dir().map_err(ConfigError::Io)?;
    Ok(cwd.join(DEFAULT_CONFIG_FILE))
}

pub fn load_config() -> Result<GitAutomateConfig, ConfigError> {
    let file_path = resolve_config_path()?;
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
    const ENV_GH_TOKEN: &str = "GA_TEST_GH_TOKEN";
    const ENV_GH_TOKEN_ALL: &str = "GA_TEST_GH_TOKEN_ALL";
    const ENV_GH_TOKEN_UNSET: &str = "GA_TEST_GH_TOKEN_UNSET";
    const ENV_OPENCODE_URL_ALL: &str = "GA_TEST_OPENCODE_URL_ALL";
    const ENV_PW_ALL: &str = "GA_TEST_PW_ALL";
    const ENV_PW_PER_PROJECT: &str = "GA_TEST_PW_PER_PROJECT";

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

    // Test 7: parse_config with valid YAML → returns GitAutomateConfig with correct git section
    #[test]
    fn parse_config_valid_yaml() {
        let yaml = r#"
git:
  repository: https://github.com/user/repo
  projectId: 42
  directory: src
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        assert_eq!(config.git.repository, "https://github.com/user/repo");
        assert_eq!(config.git.project_id.as_deref(), Some("42"));
        assert_eq!(config.git.directory, "src");
    }

    // Test: parse_config without directory → YamlParse (missing field)
    #[test]
    fn parse_config_missing_directory_fails() {
        let yaml = "git:\n  repository: https://github.com/user/repo\n";
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();
        let result = parse_config(tmp.path());
        assert!(matches!(result, Err(ConfigError::YamlParse { .. })));
    }

    // Test: parse_config with empty directory → YamlParse (non-empty validation)
    #[test]
    fn parse_config_empty_directory_fails() {
        let yaml = "git:\n  repository: https://github.com/user/repo\n  directory: \"\"\n";
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();
        let result = parse_config(tmp.path());
        assert!(matches!(result, Err(ConfigError::YamlParse { .. })));
    }

    // Test 8: parse_config with missing file → ConfigError::FileNotFound
    #[test]
    fn parse_config_missing_file() {
        let result = parse_config(Path::new("/nonexistent/path/to/config.yml"));
        assert!(matches!(result, Err(ConfigError::FileNotFound { .. })));
    }

    // Test 12: parse_config with global opencode missing url → YamlParse error
    #[test]
    fn parse_config_missing_opencode_url() {
        let yaml = r#"
opencode:
  pw: secret123
  cwd: /test-work
  project: test-project
git:
  repository: https://github.com/user/repo
  directory: /test-dir
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
    fn validate_git_non_string_repository() {
        let yaml = r#"
git:
  repository: 12345
  directory: /test-dir
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let result = parse_config(tmp.path());
        assert!(matches!(result, Err(ConfigError::YamlParse { .. })));
    }

    // Test 10: parse_config with projectId as number (e.g. 42) → coerced to String("42")
    #[test]
    fn validate_git_number_project_id_coerced() {
        let yaml = r#"
git:
  repository: https://github.com/user/repo
  projectId: 42
  directory: /test-dir
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        assert_eq!(config.git.project_id.as_deref(), Some("42"));
    }

    // Test 11: parse_config with projectId as string → stays as string
    #[test]
    fn validate_git_string_project_id_preserved() {
        let yaml = r#"
git:
  repository: https://github.com/user/repo
  projectId: "P-123"
  directory: /test-dir
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        assert_eq!(config.git.project_id.as_deref(), Some("P-123"));
    }

    // --- load_config tests ---

    // Test 13: load_config reads from cwd's git-automate.yml → returns config
    #[tokio::test]
    async fn load_config_reads_from_cwd() {
        let yaml = r#"
opencode:
  url: http://localhost:8081
  pw: ${env:GA_TEST_PW}
  cwd: /test-work
  project: test-project
git:
  repository: https://github.com/cwd/repo
  directory: /test-dir
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
        assert_eq!(config.git.repository, "https://github.com/cwd/repo");
        assert_eq!(config.opencode.as_ref().unwrap().pw, "testpassword");
    }

    // --- issueProvider config tests ---

    // Test 15: parse_config with issueProvider: github → issue_provider = "github"
    #[test]
    fn parse_config_with_issue_provider_github() {
        let yaml = r#"
git:
  repository: https://github.com/user/repo
  issueProvider: github
  directory: /test-dir
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        assert_eq!(config.git.issue_provider, "github");
    }

    // Test 16: parse_config without issueProvider → defaults to "github"
    #[test]
    fn parse_config_without_issue_provider_defaults_to_github() {
        let yaml = r#"
git:
  repository: https://github.com/user/repo
  directory: /test-dir
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        assert_eq!(config.git.issue_provider, "github");
    }

    // Test 17: parse_config with custom issueProvider → stored correctly
    #[test]
    fn parse_config_with_custom_issue_provider() {
        let yaml = r#"
git:
  repository: https://github.com/user/repo
  issueProvider: jira
  directory: /test-dir
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        assert_eq!(config.git.issue_provider, "jira");
    }

    // Test 18: GitSection serde round-trip preserves issueProvider
    #[test]
    fn git_section_serde_round_trip_issue_provider() {
        let gs = GitSection {
            repository: "https://github.com/user/repo".to_string(),
            project_id: None,
            directory: "/test-work".to_string(),
            issue_provider: "github".to_string(),
            title_pattern: "@ai.*".to_string(),
            trello_api_key: None,
            trello_token: None,
            trello_board_id: None,
            token: None,
        };
        let yaml = serde_yaml::to_string(&gs).unwrap();
        let parsed: GitSection = serde_yaml::from_str(&yaml).unwrap();
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
git:
  repository: https://github.com/user/repo
  issueProvider: trello
  titlePattern: \"@ai.*\"
  trelloApiKey: ${{env:{}}}
  trelloToken: ${{env:{}}}
  trelloBoardId: BRD-123
  directory: /test-dir
",
            ENV_TRELLO_KEY, ENV_TRELLO_TOKEN
        );
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        assert_eq!(config.git.issue_provider, "trello");
        assert_eq!(config.git.trello_api_key.as_deref(), Some("secret-key"));
        assert_eq!(config.git.trello_token.as_deref(), Some("secret-token"));
        assert_eq!(config.git.trello_board_id.as_deref(), Some("BRD-123"));
    }

    // --- concurrency config tests ---

    // Test: parse_config with opencode.concurrency → hashmap with entries
    #[test]
    fn parse_config_opencode_concurrency_hashmap() {
        let yaml = r#"
opencode:
  url: http://localhost:8081
  pw: secret
  cwd: /test-work
  project: test-project
  concurrency:
    myprovider/slow: 2
    myprovider/fast: 4
git:
  repository: https://github.com/user/repo
  directory: /test-dir
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        let oc = config.opencode.as_ref().unwrap();
        assert_eq!(oc.concurrency.get("myprovider/slow"), Some(&2));
        assert_eq!(oc.concurrency.get("myprovider/fast"), Some(&4));
    }

    // Test: parse_config without opencode.concurrency → empty HashMap
    #[test]
    fn parse_config_opencode_concurrency_defaults_empty() {
        let yaml = r#"
opencode:
  url: http://localhost:8081
  pw: secret
  cwd: /test-work
  project: test-project
git:
  repository: https://github.com/user/repo
  directory: /test-dir
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        let oc = config.opencode.as_ref().unwrap();
        assert!(oc.concurrency.is_empty());
    }

    // Test: git_automate_config_serde_round_trip_with_opencode_concurrency
    #[test]
    fn git_automate_config_serde_round_trip_concurrency() {
        let mut concurrency = HashMap::new();
        concurrency.insert("myprovider/fast".to_string(), 4);
        let config = GitAutomateConfig {
            git: GitSection {
                repository: String::new(),
                project_id: None,
                directory: "/test-work".to_string(),
                issue_provider: "github".to_string(),
                title_pattern: "@ai.*".to_string(),
                trello_api_key: None,
                trello_token: None,
                trello_board_id: None,
                token: Some("ghp_testtoken123456789".to_string()),
            },
            opencode: Some(OpencodeConfig {
                url: "http://localhost:8081".to_string(),
                pw: "pw".to_string(),
                cwd: "/test-work".to_string(),
                project: "test-project".to_string(),
                concurrency,
            }),
        };
        let yaml = serde_yaml::to_string(&config).unwrap();
        let parsed: GitAutomateConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(
            parsed
                .opencode
                .as_ref()
                .unwrap()
                .concurrency
                .get("myprovider/fast"),
            Some(&4)
        );
        assert_eq!(parsed.git.token.as_deref(), Some("ghp_testtoken123456789"));
    }

    // Test: parse_config_opencode_concurrency_hashmap
    #[test]
    fn parse_config_opencode_concurrency_hashmap_multi_entries() {
        let yaml = r#"
opencode:
  url: http://localhost:8081
  pw: secret
  cwd: /test-work
  project: test-project
  concurrency:
    model/a: 1
    model/b: 2
    model/c: 3
git:
  repository: https://github.com/user/repo
  directory: /test-dir
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        let oc = config.opencode.as_ref().unwrap();
        assert_eq!(oc.concurrency.len(), 3);
        assert_eq!(oc.concurrency.get("model/a"), Some(&1));
        assert_eq!(oc.concurrency.get("model/b"), Some(&2));
        assert_eq!(oc.concurrency.get("model/c"), Some(&3));
    }

    // --- github_token substitution tests ---

    // Test: parse_config with git.token: ${env:VAR} → git.token is substituted
    #[test]
    fn parse_config_substitutes_github_token() {
        unsafe {
            std::env::set_var(ENV_GH_TOKEN, "ghp_from_env_substitution");
        }
        let yaml = format!(
            "
git:
  repository: https://github.com/user/repo
  directory: /test-dir
  token: ${{env:{}}}
",
            ENV_GH_TOKEN
        );
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        assert_eq!(
            config.git.token.as_deref(),
            Some("ghp_from_env_substitution")
        );
    }

    // Test: parse_config without git.token → git.token is None
    #[test]
    fn parse_config_without_github_token_is_none() {
        let yaml = r#"
git:
  repository: https://github.com/user/repo
  directory: /test-dir
"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        assert_eq!(config.git.token, None);
    }

    // Test: parse_config with git.token: ${env:VAR} where VAR unset → git.token is Some("")
    #[test]
    fn parse_config_github_token_unset_var_becomes_empty_string() {
        unsafe {
            std::env::remove_var(ENV_GH_TOKEN_UNSET);
        }
        let yaml = format!(
            "
git:
  repository: https://github.com/user/repo
  directory: /test-dir
  token: ${{env:{}}}
",
            ENV_GH_TOKEN_UNSET
        );
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        assert_eq!(config.git.token.as_deref(), Some(""));
    }

    // Test: parse_config with ALL env vars substituted simultaneously
    // (OPENCODE_URL, OPENCODE_PW, GITHUB_TOKEN) — end-to-end substitution
    #[test]
    fn parse_config_substitutes_all_env_vars_together() {
        unsafe {
            std::env::set_var(ENV_OPENCODE_URL_ALL, "http://localhost:8081");
            std::env::set_var(ENV_PW_ALL, "secret123");
            std::env::set_var(ENV_GH_TOKEN_ALL, "ghp_all_vars_work");
        }
        let yaml = format!(
            "
opencode:
  url: ${{env:{}}}
  pw: ${{env:{}}}
  cwd: /test-work
  project: test-project
git:
  repository: https://github.com/user/repo
  directory: /test-dir
  token: ${{env:{}}}
",
            ENV_OPENCODE_URL_ALL, ENV_PW_ALL, ENV_GH_TOKEN_ALL
        );
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        assert_eq!(config.git.token.as_deref(), Some("ghp_all_vars_work"));
        assert_eq!(
            config.opencode.as_ref().unwrap().url,
            "http://localhost:8081"
        );
        assert_eq!(config.opencode.as_ref().unwrap().pw, "secret123");
    }

    // Test: parse_config with global opencode.pw using ${env:VAR}
    // alongside git.token — both substituted independently
    #[test]
    fn parse_config_substitutes_global_opencode_pw() {
        unsafe {
            std::env::set_var(ENV_PW_PER_PROJECT, "global_level_pw");
        }
        let yaml = format!(
            "
opencode:
  url: http://localhost:8081
  pw: ${{env:{}}}
  cwd: /test-work
  project: test-project
git:
  repository: https://github.com/user/repo
  directory: /test-dir
",
            ENV_PW_PER_PROJECT
        );
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(yaml.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let config = parse_config(tmp.path()).unwrap();
        assert_eq!(config.opencode.as_ref().unwrap().pw, "global_level_pw");
    }
}
