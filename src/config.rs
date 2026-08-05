use regex::Regex;
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
pub struct ProjectConfig {
    pub repository: String,
    #[serde(rename = "projectId")]
    pub project_id: Option<String>,
    pub directory: Option<String>,
    pub opencode: Option<OpencodeConfig>,
    #[serde(rename = "issueProvider", default = "default_issue_provider")]
    pub issue_provider: Option<String>,
}

fn default_issue_provider() -> Option<String> {
    Some("github".to_string())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitAutomateConfig {
    pub projects: BTreeMap<String, ProjectConfig>,
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

fn validate_project(data: &Value, name: &str) -> Result<ProjectConfig, ConfigError> {
    let mapping = data
        .as_mapping()
        .ok_or_else(|| ConfigError::InvalidConfig {
            message: format!("project '{}' must be a mapping", name),
        })?;

    let repository = mapping
        .get("repository")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ConfigError::InvalidConfig {
            message: format!("project '{}' requires a string 'repository' field", name),
        })?
        .to_string();

    let mut config = ProjectConfig {
        repository,
        project_id: None,
        directory: None,
        opencode: None,
        issue_provider: Some("github".to_string()),
    };

    if let Some(provider) = mapping.get("issueProvider")
        && let Some(s) = provider.as_str()
    {
        config.issue_provider = Some(s.to_string());
    }

    if let Some(raw_id) = mapping.get("projectId") {
        if let Some(s) = raw_id.as_str() {
            config.project_id = Some(s.to_string());
        } else if let Some(i) = raw_id.as_i64() {
            config.project_id = Some(i.to_string());
        } else if let Some(u) = raw_id.as_u64() {
            config.project_id = Some(u.to_string());
        } else if let Some(f) = raw_id.as_f64() {
            config.project_id = Some(f.to_string());
        } else {
            return Err(ConfigError::InvalidConfig {
                message: format!("project '{}' 'projectId' must be a string or number", name),
            });
        }
    }

    if let Some(dir) = mapping.get("directory") {
        if let Some(s) = dir.as_str() {
            config.directory = Some(s.to_string());
        } else {
            return Err(ConfigError::InvalidConfig {
                message: format!("project '{}' 'directory' must be a string", name),
            });
        }
    }

    if let Some(oc) = mapping.get("opencode") {
        let oc_mapping = oc.as_mapping().ok_or_else(|| ConfigError::InvalidConfig {
            message: format!("project '{}' 'opencode' must be a mapping", name),
        })?;

        let url = oc_mapping
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ConfigError::InvalidConfig {
                message: format!("project '{}' 'opencode.url' must be a string", name),
            })?
            .to_string();

        let pw = oc_mapping
            .get("pw")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ConfigError::InvalidConfig {
                message: format!("project '{}' 'opencode.pw' must be a string", name),
            })?
            .to_string();

        config.opencode = Some(OpencodeConfig { url, pw });
    }

    Ok(config)
}

fn validate_config(data: &Value) -> Result<GitAutomateConfig, ConfigError> {
    let mapping = data
        .as_mapping()
        .ok_or_else(|| ConfigError::InvalidConfig {
            message: "expected a top-level mapping".to_string(),
        })?;

    let projects_map = match mapping.get("projects") {
        Some(Value::Mapping(m)) => m,
        _ => {
            return Err(ConfigError::InvalidConfig {
                message: "'projects' must be a mapping".to_string(),
            });
        }
    };

    let mut projects = BTreeMap::new();
    for (name, raw) in projects_map.iter() {
        let name_str = name.as_str().ok_or_else(|| ConfigError::InvalidConfig {
            message: "project name must be a string".to_string(),
        })?;
        let project = validate_project(raw, name_str)?;
        projects.insert(name_str.to_string(), project);
    }

    Ok(GitAutomateConfig { projects })
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
    validate_config(&substituted)
}

pub fn load_config() -> Result<GitAutomateConfig, ConfigError> {
    let cwd = std::env::current_dir().map_err(ConfigError::Io)?;
    let file_path = cwd.join("git-automate.yml");
    parse_config(&file_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    // --- substitute_env tests ---

    // Test 1: substitute_env with ${env:VAR} where VAR="hello" → "hello"
    #[test]
    fn substitute_env_replaces_existing_var() {
        unsafe {
            std::env::set_var("GA_TEST_HELLO", "hello");
        }
        let input = Value::String("${env:GA_TEST_HELLO}".to_string());
        let result = substitute_env(&input);
        assert_eq!(result, Value::String("hello".to_string()));
    }

    // Test 2: substitute_env with ${env:VAR} where VAR unset → ""
    #[test]
    fn substitute_env_unset_var_becomes_empty() {
        unsafe {
            std::env::remove_var("GA_TEST_UNSET_VAR");
        }
        let input = Value::String("${env:GA_TEST_UNSET_VAR}".to_string());
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
            std::env::set_var("GA_TEST_NESTED", "deep_value");
        }
        let mapping = serde_yaml::from_str::<Value>("key: ${env:GA_TEST_NESTED}\n").unwrap();
        let result = substitute_env(&mapping);
        let expected = serde_yaml::from_str::<Value>("key: deep_value\n").unwrap();
        assert_eq!(result, expected);
    }

    // Test 5: substitute_env on array with ${env:VAR} → recursively substitutes each element
    #[test]
    fn substitute_env_array_recursive() {
        unsafe {
            std::env::set_var("GA_TEST_ARR", "arr_val");
        }
        let seq = Value::Sequence(vec![
            Value::String("${env:GA_TEST_ARR}".to_string()),
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
            std::env::set_var("GA_TEST_MULTI_A", "A");
        }
        unsafe {
            std::env::set_var("GA_TEST_MULTI_B", "B");
        }
        let input = Value::String("${env:GA_TEST_MULTI_A} and ${env:GA_TEST_MULTI_B}".to_string());
        let result = substitute_env(&input);
        assert_eq!(result, Value::String("A and B".to_string()));
    }

    // Test 14: substitute_env with ${env:VAR_NAME} (underscore) → matches pattern
    #[test]
    fn substitute_env_underscore_var_name() {
        unsafe {
            std::env::set_var("GA_TEST_UNDERSCORE_VAR", "underscored");
        }
        let input = Value::String("${env:GA_TEST_UNDERSCORE_VAR}".to_string());
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

    // Test 12: parse_config with config missing opencode.url → InvalidConfig error
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
        assert!(matches!(result, Err(ConfigError::InvalidConfig { .. })));
    }

    // --- validate_project tests (via parse_config) ---

    // Test 9: validate_project with non-string repository → ConfigError::InvalidConfig
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
        assert!(matches!(result, Err(ConfigError::InvalidConfig { .. })));
    }

    // Test 10: validate_project with projectId as number (e.g. 42) → coerced to String("42")
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

    // Test 11: validate_project with projectId as string → stays as string
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
    #[test]
    fn load_config_reads_from_cwd() {
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
            std::env::set_var("GA_TEST_PW", "testpassword");
        }

        // Change to the temp directory so load_config finds git-automate.yml
        let _guard = crate::test_utils::SET_CWD_MUTEX.lock().unwrap();
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

    // Test 15: parse_config with issueProvider: github → issue_provider = Some("github")
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
        assert_eq!(project.issue_provider.as_deref(), Some("github"));
    }

    // Test 16: parse_config without issueProvider → defaults to Some("github")
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
        assert_eq!(project.issue_provider.as_deref(), Some("github"));
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
        assert_eq!(project.issue_provider.as_deref(), Some("jira"));
    }

    // Test 18: ProjectConfig serde round-trip preserves issueProvider
    #[test]
    fn project_config_serde_round_trip_issue_provider() {
        let pc = ProjectConfig {
            repository: "https://github.com/user/repo".to_string(),
            project_id: None,
            directory: None,
            opencode: None,
            issue_provider: Some("github".to_string()),
        };
        let yaml = serde_yaml::to_string(&pc).unwrap();
        let parsed: ProjectConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.issue_provider.as_deref(), Some("github"));
    }
}
