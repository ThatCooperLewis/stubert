use stubert::gateway::claude_cli::{call_claude, ClaudeCallParams, ClaudeError};

fn make_live_params(prompt: &str) -> ClaudeCallParams {
    ClaudeCallParams {
        prompt: prompt.to_string(),
        session_id: uuid::Uuid::new_v4().to_string(),
        is_new_session: true,
        allowed_tools: None,
        add_dirs: None,
        model: None,
        append_system_prompt: None,
        env_file_path: ".env".to_string(),
        timeout_secs: 120,
        working_directory: ".".to_string(),
        cli_path: "claude".to_string(),
    }
}

#[tokio::test]
#[ignore]
async fn real_cli_call_parses_json() {
    let params = make_live_params("Respond with exactly the word PONG and nothing else.");

    let resp = call_claude(&params).await.expect("CLI call should succeed");
    assert!(
        resp.result.contains("PONG"),
        "expected PONG in response, got: {}",
        resp.result
    );
    assert!(!resp.session_id.is_empty());
    assert!(resp.cost_usd > 0.0);
    assert!(resp.input_tokens > 0);
    assert!(resp.output_tokens > 0);
}

#[tokio::test]
#[ignore]
async fn real_cli_timeout() {
    let mut params = make_live_params(
        "Write a 10000-word essay about the history of mathematics. Take your time and be thorough.",
    );
    params.timeout_secs = 1;

    let err = call_claude(&params).await.expect_err("should timeout");
    assert!(
        matches!(err, ClaudeError::Timeout { .. }),
        "expected Timeout error, got: {err}"
    );
}

#[tokio::test]
#[ignore]
async fn real_cli_model_resolution() {
    let mut params = make_live_params("Respond with exactly OK.");
    params.model = Some("claude-sonnet-4-6".to_string());

    let resp = call_claude(&params).await.expect("CLI call with model should succeed");
    assert!(!resp.result.is_empty());
}
