pub mod types;

pub use types::*;

use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file: {0}")]
    ReadError(#[from] std::io::Error),
    #[error("failed to parse YAML: {0}")]
    YamlError(#[from] serde_yaml_ng::Error),
    #[error("missing environment variable: {name}")]
    MissingEnvVar { name: String },
}

pub fn load_config(path: &Path) -> Result<StubbertConfig, ConfigError> {
    let contents = std::fs::read_to_string(path)?;
    let mut value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&contents)?;
    interpolate_env_vars(&mut value)?;
    let config: StubbertConfig = serde_yaml_ng::from_value(value)?;
    Ok(config)
}

fn interpolate_env_vars(value: &mut serde_yaml_ng::Value) -> Result<(), ConfigError> {
    match value {
        serde_yaml_ng::Value::String(s) => {
            *s = replace_env_vars(s)?;
        }
        serde_yaml_ng::Value::Mapping(map) => {
            for (_key, val) in map.iter_mut() {
                interpolate_env_vars(val)?;
            }
        }
        serde_yaml_ng::Value::Sequence(seq) => {
            for val in seq.iter_mut() {
                interpolate_env_vars(val)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn replace_env_vars(s: &str) -> Result<String, ConfigError> {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut var_name = String::new();
            loop {
                match chars.next() {
                    Some('}') => break,
                    Some(ch) => var_name.push(ch),
                    None => {
                        // Unterminated ${, just emit literally
                        result.push_str("${");
                        result.push_str(&var_name);
                        return Ok(result);
                    }
                }
            }
            let val = std::env::var(&var_name).map_err(|_| ConfigError::MissingEnvVar {
                name: var_name.clone(),
            })?;
            result.push_str(&val);
        } else {
            result.push(c);
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_config(yaml: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(yaml.as_bytes()).unwrap();
        f
    }

    fn full_config_yaml() -> String {
        r#"
telegram:
  token: "tg-token-123"
  allowed_users: [111]
  unauthorized_response: "go away"

discord:
  token: "dc-token-456"
  allowed_users: [222]

claude:
  cli_path: "claude"
  timeout_secs: 300
  default_model: "sonnet"
  working_directory: "."
  env_file_path: ".env"
  allowed_tools:
    telegram: ["Bash", "Read"]
    discord: ["Read"]
  add_dirs: ["/extra"]

sessions:
  timeout_minutes: 60
  sessions_file: "sessions.json"

history:
  base_dir: "history"

logging:
  log_file: "logs/stubert.log"
  log_max_bytes: 10000000
  log_backup_count: 5
  level: "INFO"

heartbeat:
  interval_minutes: 30
  file: "HEARTBEAT.md"

health:
  port: 8484
"#
        .to_string()
    }

    #[test]
    fn loads_valid_config_with_all_fields() {
        let f = write_config(&full_config_yaml());
        let config = load_config(f.path()).unwrap();

        assert_eq!(config.telegram.token, "tg-token-123");
        assert_eq!(config.telegram.allowed_users, vec![111]);
        assert_eq!(
            config.telegram.unauthorized_response.as_deref(),
            Some("go away")
        );
        assert_eq!(config.discord.token, "dc-token-456");
        assert!(config.discord.unauthorized_response.is_none());
        assert_eq!(config.claude.cli_path, "claude");
        assert_eq!(config.claude.timeout_secs, 300);
        assert_eq!(config.claude.default_model, "sonnet");
        assert_eq!(config.claude.env_file_path, ".env");
        assert_eq!(
            config.claude.allowed_tools.get("telegram").unwrap(),
            &vec!["Bash".to_string(), "Read".to_string()]
        );
        assert_eq!(config.claude.add_dirs, vec!["/extra"]);
        assert_eq!(config.sessions.timeout_minutes, 60);
        assert_eq!(config.sessions.sessions_file, "sessions.json");
        assert_eq!(config.history.base_dir, "history");
        assert_eq!(config.logging.log_file, "logs/stubert.log");
        assert_eq!(config.logging.log_max_bytes, 10_000_000);
        assert_eq!(config.logging.log_backup_count, 5);
        assert_eq!(config.logging.level, "INFO");
        assert_eq!(config.heartbeat.interval_minutes, 30);
        assert_eq!(config.heartbeat.file, "HEARTBEAT.md");
        assert_eq!(config.health.port, 8484);
        assert!(config.scheduler.is_none());
        assert!(config.files.is_none());
        assert!(config.gateway.is_none());
    }

    #[test]
    fn interpolates_env_vars() {
        std::env::set_var("STUBERT_TEST_TG_TOKEN", "secret-tg-token");
        let yaml = full_config_yaml().replace("tg-token-123", "${STUBERT_TEST_TG_TOKEN}");
        let f = write_config(&yaml);
        let config = load_config(f.path()).unwrap();
        assert_eq!(config.telegram.token, "secret-tg-token");
        std::env::remove_var("STUBERT_TEST_TG_TOKEN");
    }

    #[test]
    fn errors_on_missing_env_var() {
        let yaml = full_config_yaml().replace("tg-token-123", "${STUBERT_TEST_NONEXISTENT_VAR}");
        let f = write_config(&yaml);
        let err = load_config(f.path()).unwrap_err();
        match err {
            ConfigError::MissingEnvVar { name } => {
                assert_eq!(name, "STUBERT_TEST_NONEXISTENT_VAR");
            }
            other => panic!("expected MissingEnvVar, got: {other}"),
        }
    }

    #[test]
    fn interpolates_env_vars_in_nested_structures() {
        std::env::set_var("STUBERT_TEST_TOOL", "Write");
        let yaml = full_config_yaml().replace(
            r#"telegram: ["Bash", "Read"]"#,
            r#"telegram: ["Bash", "${STUBERT_TEST_TOOL}"]"#,
        );
        let f = write_config(&yaml);
        let config = load_config(f.path()).unwrap();
        assert_eq!(
            config.claude.allowed_tools.get("telegram").unwrap(),
            &vec!["Bash".to_string(), "Write".to_string()]
        );
        std::env::remove_var("STUBERT_TEST_TOOL");
    }

    #[test]
    fn ignores_unknown_fields() {
        let yaml = full_config_yaml() + "\nunknown_section:\n  foo: bar\n";
        let f = write_config(&yaml);
        let config = load_config(f.path());
        assert!(config.is_ok());
    }

    #[test]
    fn errors_on_file_not_found() {
        let err = load_config(Path::new("/nonexistent/config.yaml")).unwrap_err();
        assert!(matches!(err, ConfigError::ReadError(_)));
    }

    #[test]
    fn errors_on_invalid_yaml() {
        let f = write_config("[[[invalid yaml");
        let err = load_config(f.path()).unwrap_err();
        assert!(matches!(err, ConfigError::YamlError(_)));
    }

    #[test]
    fn dotenvy_loads_env_file_for_interpolation() {
        let dir = tempfile::tempdir().unwrap();

        // Write .env file
        let env_path = dir.path().join(".env");
        std::fs::write(&env_path, "STUBERT_DOTENV_TEST_TOKEN=from-dotenv\n").unwrap();

        // Write config.yaml that references the env var
        let yaml = full_config_yaml().replace("tg-token-123", "${STUBERT_DOTENV_TEST_TOKEN}");
        let config_path = dir.path().join("config.yaml");
        std::fs::write(&config_path, &yaml).unwrap();

        // Load .env from the specific path (avoids CWD side effects in tests)
        dotenvy::from_path(&env_path).unwrap();

        let config = load_config(&config_path).unwrap();
        assert_eq!(config.telegram.token, "from-dotenv");

        std::env::remove_var("STUBERT_DOTENV_TEST_TOKEN");
    }

    #[test]
    fn errors_on_missing_required_fields() {
        let f = write_config("telegram:\n  token: x\n");
        let err = load_config(f.path()).unwrap_err();
        assert!(matches!(err, ConfigError::YamlError(_)));
    }

    #[test]
    fn emits_unterminated_env_var_literally() {
        // Unterminated ${ is passed through without error
        std::env::set_var("STUBERT_TEST_TG_TOKEN2", "real");
        let yaml = full_config_yaml().replace("tg-token-123", "before${INCOMPLETE");
        let f = write_config(&yaml);
        let config = load_config(f.path()).unwrap();
        assert_eq!(config.telegram.token, "before${INCOMPLETE");
        std::env::remove_var("STUBERT_TEST_TG_TOKEN2");
    }

    #[test]
    fn handles_multiple_env_vars_in_one_string() {
        std::env::set_var("STUBERT_TEST_PREFIX", "hello");
        std::env::set_var("STUBERT_TEST_SUFFIX", "world");
        let yaml =
            full_config_yaml().replace("tg-token-123", "${STUBERT_TEST_PREFIX}-${STUBERT_TEST_SUFFIX}");
        let f = write_config(&yaml);
        let config = load_config(f.path()).unwrap();
        assert_eq!(config.telegram.token, "hello-world");
        std::env::remove_var("STUBERT_TEST_PREFIX");
        std::env::remove_var("STUBERT_TEST_SUFFIX");
    }
}
