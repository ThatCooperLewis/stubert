use std::time::Duration;
use thiserror::Error;

#[derive(Debug)]
pub struct ClaudeCallParams {
    pub prompt: String,
    pub session_id: String,
    pub is_new_session: bool,
    pub allowed_tools: Option<Vec<String>>,
    pub add_dirs: Option<Vec<String>>,
    pub model: Option<String>,
    pub append_system_prompt: Option<String>,
    pub env_file_path: String,
    pub timeout_secs: u64,
    pub working_directory: String,
    pub cli_path: String,
}

#[derive(Debug)]
pub struct ClaudeResponse {
    pub result: String,
    pub session_id: String,
    pub cost_usd: f64,
    pub duration_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Error)]
pub enum ClaudeError {
    #[error("CLI exited with code {code}: {stderr}")]
    ExitError { code: i32, stderr: String },
    #[error("failed to parse CLI output: {0}")]
    ParseError(String),
    #[error("CLI returned failure: {0}")]
    CliFailure(String),
    #[error("CLI timed out after {timeout_secs}s")]
    Timeout { timeout_secs: u64 },
    #[error("failed to spawn CLI process: {0}")]
    SpawnError(std::io::Error),
    #[error("CLI process I/O error: {0}")]
    ProcessError(std::io::Error),
}

pub fn resolve_model(alias: &str) -> String {
    match alias {
        "sonnet" => "claude-sonnet-4-6".to_string(),
        "opus" => "claude-opus-4-6".to_string(),
        "haiku" => "claude-haiku-4-5-20251001".to_string(),
        other => other.to_string(),
    }
}

pub fn display_model(model_id: &str) -> String {
    match model_id {
        "claude-sonnet-4-6" => "Sonnet 4.6".to_string(),
        "claude-opus-4-6" => "Opus 4.6".to_string(),
        "claude-haiku-4-5-20251001" => "Haiku 4.5".to_string(),
        other => other.to_string(),
    }
}

pub fn build_args(params: &ClaudeCallParams) -> Vec<String> {
    let mut args = vec![
        "-p".to_string(),
        params.prompt.clone(),
        "--output-format".to_string(),
        "json".to_string(),
    ];

    if params.is_new_session {
        args.push("--session-id".to_string());
    } else {
        args.push("--resume".to_string());
    }
    args.push(params.session_id.clone());

    if let Some(ref tools) = params.allowed_tools {
        args.push("--allowedTools".to_string());
        args.extend(tools.iter().cloned());
    }

    if let Some(ref dirs) = params.add_dirs {
        for dir in dirs {
            args.push("--add-dir".to_string());
            args.push(dir.clone());
        }
    }

    if let Some(ref model) = params.model {
        args.push("--model".to_string());
        args.push(model.clone());
    }

    if let Some(ref prompt) = params.append_system_prompt {
        args.push("--append-system-prompt".to_string());
        args.push(prompt.clone());
    }

    args
}

pub async fn call_claude(params: &ClaudeCallParams) -> Result<ClaudeResponse, ClaudeError> {
    let args = build_args(params);

    // Prepend .venv/bin to PATH so skill scripts using #!/usr/bin/env python3
    // resolve to the venv's Python interpreter.
    let venv_bin = std::path::Path::new(&params.working_directory).join(".venv/bin");
    let path_env = match std::env::var("PATH") {
        Ok(existing) => format!("{}:{existing}", venv_bin.display()),
        Err(_) => venv_bin.display().to_string(),
    };

    let mut cmd = tokio::process::Command::new(&params.cli_path);
    cmd.args(&args)
        .current_dir(&params.working_directory)
        .env("PATH", &path_env)
        .env("CLAUDE_ENV_FILE", &params.env_file_path)
        .env("SHELL", "/bin/bash")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let child = cmd.spawn().map_err(ClaudeError::SpawnError)?;

    let timeout = Duration::from_secs(params.timeout_secs);

    // wait_with_output reads stdout/stderr concurrently with waiting,
    // avoiding deadlock if output exceeds the OS pipe buffer (~64KB).
    // kill_on_drop ensures the child is killed if the timeout fires and
    // the future (owning the child) is dropped.
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            if !output.status.success() {
                let code = output.status.code().unwrap_or(-1);
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                return Err(ClaudeError::ExitError { code, stderr });
            }
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            parse_response(&stdout)
        }
        Ok(Err(e)) => Err(ClaudeError::ProcessError(e)),
        Err(_) => Err(ClaudeError::Timeout {
            timeout_secs: params.timeout_secs,
        }),
    }
}

fn parse_response(stdout: &str) -> Result<ClaudeResponse, ClaudeError> {
    let json: serde_json::Value =
        serde_json::from_str(stdout).map_err(|e| ClaudeError::ParseError(e.to_string()))?;

    let subtype = json
        .get("subtype")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if subtype != "success" {
        let msg = json
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Err(ClaudeError::CliFailure(msg.to_string()));
    }

    let result = json
        .get("result")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ClaudeError::ParseError("missing 'result' field".to_string()))?
        .to_string();

    let session_id = json
        .get("session_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ClaudeError::ParseError("missing 'session_id' field".to_string()))?
        .to_string();

    let cost_usd = json
        .get("cost_usd")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    let duration_ms = json
        .get("duration_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let usage = json.get("usage");
    let input_tokens = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    Ok(ClaudeResponse {
        result,
        session_id,
        cost_usd,
        duration_ms,
        input_tokens,
        output_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use tempfile::TempDir;

    fn make_params() -> ClaudeCallParams {
        ClaudeCallParams {
            prompt: "hello".to_string(),
            session_id: "sess-1".to_string(),
            is_new_session: true,
            allowed_tools: None,
            add_dirs: None,
            model: None,
            append_system_prompt: None,
            env_file_path: ".env".to_string(),
            timeout_secs: 30,
            working_directory: ".".to_string(),
            cli_path: "claude".to_string(),
        }
    }

    fn make_mock_script(dir: &Path, name: &str, content: &str) -> String {
        let path = dir.join(name);
        // Write to a temp file first, then atomically rename. This avoids
        // ETXTBSY ("Text file busy") errors that occur when exec races with
        // a file that was recently opened for writing.
        let tmp_path = dir.join(format!("{name}.tmp"));
        let mut f = std::fs::File::create(&tmp_path).unwrap();
        write!(f, "#!/usr/bin/env bash\n{content}").unwrap();
        f.sync_all().unwrap();
        drop(f);
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&tmp_path, &path).unwrap();
        path.to_str().unwrap().to_string()
    }

    fn success_json() -> String {
        serde_json::json!({
            "type": "result",
            "subtype": "success",
            "result": "Hello! How can I help?",
            "session_id": "abc-123",
            "cost_usd": 0.05,
            "duration_ms": 1500,
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50
            }
        })
        .to_string()
    }

    // -- Model aliasing tests --

    mod test_resolve_model {
        use super::super::*;

        #[test]
        fn resolves_sonnet() {
            assert_eq!(resolve_model("sonnet"), "claude-sonnet-4-6");
        }

        #[test]
        fn resolves_opus() {
            assert_eq!(resolve_model("opus"), "claude-opus-4-6");
        }

        #[test]
        fn resolves_haiku() {
            assert_eq!(resolve_model("haiku"), "claude-haiku-4-5-20251001");
        }

        #[test]
        fn passes_through_custom_model() {
            assert_eq!(resolve_model("custom-model"), "custom-model");
        }
    }

    mod test_display_model {
        use super::super::*;

        #[test]
        fn displays_sonnet() {
            assert_eq!(display_model("claude-sonnet-4-6"), "Sonnet 4.6");
        }

        #[test]
        fn displays_opus() {
            assert_eq!(display_model("claude-opus-4-6"), "Opus 4.6");
        }

        #[test]
        fn displays_haiku() {
            assert_eq!(display_model("claude-haiku-4-5-20251001"), "Haiku 4.5");
        }

        #[test]
        fn passes_through_unknown() {
            assert_eq!(display_model("unknown"), "unknown");
        }
    }

    // -- Argument assembly tests --

    mod test_build_args {
        use super::super::*;
        use super::make_params;

        #[test]
        fn includes_base_args() {
            let params = make_params();
            let args = build_args(&params);
            assert_eq!(args[0], "-p");
            assert_eq!(args[1], "hello");
            assert_eq!(args[2], "--output-format");
            assert_eq!(args[3], "json");
        }

        #[test]
        fn new_session_uses_session_id_flag() {
            let params = make_params();
            let args = build_args(&params);
            assert_eq!(args[4], "--session-id");
            assert_eq!(args[5], "sess-1");
        }

        #[test]
        fn resume_uses_resume_flag() {
            let mut params = make_params();
            params.is_new_session = false;
            let args = build_args(&params);
            assert_eq!(args[4], "--resume");
            assert_eq!(args[5], "sess-1");
        }

        #[test]
        fn includes_allowed_tools() {
            let mut params = make_params();
            params.allowed_tools = Some(vec!["Bash".to_string(), "Read".to_string()]);
            let args = build_args(&params);
            let tools_idx = args.iter().position(|a| a == "--allowedTools").unwrap();
            assert_eq!(args[tools_idx + 1], "Bash");
            assert_eq!(args[tools_idx + 2], "Read");
        }

        #[test]
        fn includes_add_dirs() {
            let mut params = make_params();
            params.add_dirs = Some(vec!["/a".to_string(), "/b".to_string()]);
            let args = build_args(&params);
            let positions: Vec<_> = args
                .iter()
                .enumerate()
                .filter(|(_, a)| a.as_str() == "--add-dir")
                .map(|(i, _)| i)
                .collect();
            assert_eq!(positions.len(), 2);
            assert_eq!(args[positions[0] + 1], "/a");
            assert_eq!(args[positions[1] + 1], "/b");
        }

        #[test]
        fn includes_model() {
            let mut params = make_params();
            params.model = Some("sonnet".to_string());
            let args = build_args(&params);
            let idx = args.iter().position(|a| a == "--model").unwrap();
            assert_eq!(args[idx + 1], "sonnet");
        }

        #[test]
        fn includes_append_system_prompt() {
            let mut params = make_params();
            params.append_system_prompt = Some("You are on telegram.".to_string());
            let args = build_args(&params);
            let idx = args.iter().position(|a| a == "--append-system-prompt").unwrap();
            assert_eq!(args[idx + 1], "You are on telegram.");
        }

        #[test]
        fn no_optional_args_when_none() {
            let params = make_params();
            let args = build_args(&params);
            assert_eq!(args.len(), 6); // -p, prompt, --output-format, json, --session-id, sess-1
            assert!(!args.contains(&"--allowedTools".to_string()));
            assert!(!args.contains(&"--add-dir".to_string()));
            assert!(!args.contains(&"--model".to_string()));
            assert!(!args.contains(&"--append-system-prompt".to_string()));
        }
    }

    // -- call_claude tests (mock scripts) --

    mod test_call_claude {
        use super::*;

        #[tokio::test]
        async fn parses_successful_response() {
            let dir = TempDir::new().unwrap();
            let script = make_mock_script(
                dir.path(),
                "claude",
                &format!("echo '{}'", success_json()),
            );
            let mut params = make_params();
            params.cli_path = script;
            params.working_directory = dir.path().to_str().unwrap().to_string();

            let resp = call_claude(&params).await.unwrap();
            assert_eq!(resp.result, "Hello! How can I help?");
            assert_eq!(resp.session_id, "abc-123");
            assert_eq!(resp.cost_usd, 0.05);
            assert_eq!(resp.duration_ms, 1500);
            assert_eq!(resp.input_tokens, 100);
            assert_eq!(resp.output_tokens, 50);
        }

        #[tokio::test]
        async fn returns_exit_error_on_nonzero_exit() {
            let dir = TempDir::new().unwrap();
            let script = make_mock_script(
                dir.path(),
                "claude",
                "echo 'something went wrong' >&2\nexit 1",
            );
            let mut params = make_params();
            params.cli_path = script;
            params.working_directory = dir.path().to_str().unwrap().to_string();

            let err = call_claude(&params).await.unwrap_err();
            match err {
                ClaudeError::ExitError { code, stderr } => {
                    assert_eq!(code, 1);
                    assert!(stderr.contains("something went wrong"));
                }
                other => panic!("expected ExitError, got: {other}"),
            }
        }

        #[tokio::test]
        async fn returns_parse_error_on_invalid_json() {
            let dir = TempDir::new().unwrap();
            let script = make_mock_script(dir.path(), "claude", "echo 'not json'");
            let mut params = make_params();
            params.cli_path = script;
            params.working_directory = dir.path().to_str().unwrap().to_string();

            let err = call_claude(&params).await.unwrap_err();
            assert!(matches!(err, ClaudeError::ParseError(_)));
        }

        #[tokio::test]
        async fn returns_cli_failure_on_non_success_subtype() {
            let dir = TempDir::new().unwrap();
            let json = serde_json::json!({
                "type": "result",
                "subtype": "error",
                "result": "rate limited"
            });
            let script =
                make_mock_script(dir.path(), "claude", &format!("echo '{json}'"));
            let mut params = make_params();
            params.cli_path = script;
            params.working_directory = dir.path().to_str().unwrap().to_string();

            let err = call_claude(&params).await.unwrap_err();
            match err {
                ClaudeError::CliFailure(msg) => assert!(msg.contains("rate limited")),
                other => panic!("expected CliFailure, got: {other}"),
            }
        }

        #[tokio::test]
        async fn returns_timeout_on_slow_process() {
            let dir = TempDir::new().unwrap();
            let script = make_mock_script(dir.path(), "claude", "sleep 30");
            let mut params = make_params();
            params.cli_path = script;
            params.timeout_secs = 1;
            params.working_directory = dir.path().to_str().unwrap().to_string();

            let err = call_claude(&params).await.unwrap_err();
            match err {
                ClaudeError::Timeout { timeout_secs } => assert_eq!(timeout_secs, 1),
                other => panic!("expected Timeout, got: {other}"),
            }
        }

        #[tokio::test]
        async fn sets_env_vars_on_subprocess() {
            let dir = TempDir::new().unwrap();
            let script = make_mock_script(
                dir.path(),
                "claude",
                &format!(
                    r#"
if [ "$CLAUDE_ENV_FILE" = "custom.env" ] && [ "$SHELL" = "/bin/bash" ]; then
  echo '{}'
else
  echo "CLAUDE_ENV_FILE=$CLAUDE_ENV_FILE SHELL=$SHELL" >&2
  exit 1
fi
"#,
                    success_json()
                ),
            );
            let mut params = make_params();
            params.cli_path = script;
            params.env_file_path = "custom.env".to_string();
            params.working_directory = dir.path().to_str().unwrap().to_string();

            let resp = call_claude(&params).await.unwrap();
            assert_eq!(resp.session_id, "abc-123");
        }

        #[tokio::test]
        async fn returns_parse_error_on_empty_stdout() {
            let dir = TempDir::new().unwrap();
            let script = make_mock_script(dir.path(), "claude", "exit 0");
            let mut params = make_params();
            params.cli_path = script;
            params.working_directory = dir.path().to_str().unwrap().to_string();

            let err = call_claude(&params).await.unwrap_err();
            assert!(matches!(err, ClaudeError::ParseError(_)));
        }

        #[tokio::test]
        async fn returns_spawn_error_for_missing_binary() {
            let mut params = make_params();
            params.cli_path = "/nonexistent/claude".to_string();

            let err = call_claude(&params).await.unwrap_err();
            assert!(matches!(err, ClaudeError::SpawnError(_)));
        }
    }
}
