use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use stubert::adapters::{AdapterError, IncomingMessage, MessageHandler, PlatformAdapter};
use stubert::config::types::*;
use stubert::gateway::claude_cli::{ClaudeCallParams, ClaudeError, ClaudeResponse};
use stubert::gateway::core::ClaudeCaller;

// ---- TestClaudeCaller ----

#[allow(dead_code)]
pub struct CapturedCall {
    pub prompt: String,
    pub session_id: String,
    pub is_new_session: bool,
    pub model: Option<String>,
    pub append_system_prompt: Option<String>,
}

pub struct TestClaudeCaller {
    responses: std::sync::Mutex<Vec<Result<ClaudeResponse, ClaudeError>>>,
    pub calls: Arc<std::sync::Mutex<Vec<CapturedCall>>>,
    delay: Option<std::time::Duration>,
}

impl TestClaudeCaller {
    pub fn always_success(text: &str) -> Self {
        let text = text.to_string();
        Self {
            responses: std::sync::Mutex::new(vec![Ok(success_response(&text))]),
            calls: Arc::new(std::sync::Mutex::new(Vec::new())),
            delay: None,
        }
    }

    /// Same as always_success but adds a delay before returning, giving
    /// concurrent tasks (e.g. the typing indicator loop) time to execute.
    pub fn with_delay(text: &str, delay: std::time::Duration) -> Self {
        let text = text.to_string();
        Self {
            responses: std::sync::Mutex::new(vec![Ok(success_response(&text))]),
            calls: Arc::new(std::sync::Mutex::new(Vec::new())),
            delay: Some(delay),
        }
    }

    #[allow(dead_code)]
    pub fn with_responses(responses: Vec<Result<ClaudeResponse, ClaudeError>>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
            calls: Arc::new(std::sync::Mutex::new(Vec::new())),
            delay: None,
        }
    }
}

#[async_trait]
impl ClaudeCaller for TestClaudeCaller {
    async fn call(&self, params: &ClaudeCallParams) -> Result<ClaudeResponse, ClaudeError> {
        self.calls.lock().unwrap().push(CapturedCall {
            prompt: params.prompt.clone(),
            session_id: params.session_id.clone(),
            is_new_session: params.is_new_session,
            model: params.model.clone(),
            append_system_prompt: params.append_system_prompt.clone(),
        });

        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }

        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            Ok(success_response("Hello from test!"))
        } else if responses.len() == 1 {
            // For single-element (always_success pattern), don't drain it
            match &responses[0] {
                Ok(r) => Ok(ClaudeResponse {
                    result: r.result.clone(),
                    session_id: r.session_id.clone(),
                    cost_usd: r.cost_usd,
                    duration_ms: r.duration_ms,
                    input_tokens: r.input_tokens,
                    output_tokens: r.output_tokens,
                }),
                Err(e) => Err(clone_error(e)),
            }
        } else {
            responses.remove(0)
        }
    }
}

fn clone_error(e: &ClaudeError) -> ClaudeError {
    match e {
        ClaudeError::ExitError { code, stderr } => ClaudeError::ExitError {
            code: *code,
            stderr: stderr.clone(),
        },
        ClaudeError::ParseError(s) => ClaudeError::ParseError(s.clone()),
        ClaudeError::CliFailure(s) => ClaudeError::CliFailure(s.clone()),
        ClaudeError::Timeout { timeout_secs } => ClaudeError::Timeout {
            timeout_secs: *timeout_secs,
        },
        _ => ClaudeError::CliFailure("cloned error".to_string()),
    }
}

// ---- TestAdapter ----

pub struct TestAdapter {
    started: std::sync::Mutex<bool>,
    handler: Arc<std::sync::Mutex<Option<MessageHandler>>>,
    pub sent_messages: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    pub typing_calls: Arc<std::sync::Mutex<Vec<String>>>,
}

impl TestAdapter {
    pub fn new() -> Self {
        Self {
            started: std::sync::Mutex::new(false),
            handler: Arc::new(std::sync::Mutex::new(None)),
            sent_messages: Arc::new(std::sync::Mutex::new(Vec::new())),
            typing_calls: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    pub fn handler_slot(&self) -> Arc<std::sync::Mutex<Option<MessageHandler>>> {
        self.handler.clone()
    }

    #[allow(dead_code)]
    pub fn is_started(&self) -> bool {
        *self.started.lock().unwrap()
    }
}

#[async_trait]
impl PlatformAdapter for TestAdapter {
    async fn start(&mut self) -> Result<(), AdapterError> {
        *self.started.lock().unwrap() = true;
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), AdapterError> {
        *self.started.lock().unwrap() = false;
        Ok(())
    }

    async fn send_message(&self, chat_id: &str, text: &str) -> Result<(), AdapterError> {
        self.sent_messages
            .lock()
            .unwrap()
            .push((chat_id.to_string(), text.to_string()));
        Ok(())
    }

    async fn send_typing(&self, chat_id: &str) -> Result<(), AdapterError> {
        self.typing_calls
            .lock()
            .unwrap()
            .push(chat_id.to_string());
        Ok(())
    }

    fn set_message_handler(&mut self, handler: MessageHandler) {
        *self.handler.lock().unwrap() = Some(handler);
    }
}

// ---- Helper Functions ----

pub fn make_test_config(working_dir: &Path) -> StubbertConfig {
    use std::collections::HashMap;

    StubbertConfig {
        telegram: TelegramConfig {
            token: "tg-token".to_string(),
            allowed_users: vec![],
            unauthorized_response: None,
        },
        discord: DiscordConfig {
            token: "dc-token".to_string(),
            allowed_users: vec![],
            unauthorized_response: None,
        },
        claude: ClaudeConfig {
            cli_path: "claude".to_string(),
            timeout_secs: 300,
            default_model: "claude-sonnet-4-6".to_string(),
            working_directory: working_dir.to_str().unwrap().to_string(),
            env_file_path: ".env".to_string(),
            allowed_tools: HashMap::new(),
            add_dirs: vec![],
            platform_readmes: HashMap::new(),
        },
        sessions: SessionConfig {
            timeout_minutes: 60,
            sessions_file: "sessions.json".to_string(),
        },
        history: HistoryConfig {
            base_dir: "history".to_string(),
        },
        logging: LoggingConfig {
            log_file: "stubert.log".to_string(),
            log_max_bytes: 10_000_000,
            log_backup_count: 5,
            level: "info".to_string(),
        },
        heartbeat: HeartbeatConfig {
            interval_minutes: 5,
            file: "HEARTBEAT.md".to_string(),
            allowed_tools: vec![],
            log_file: None,
            log_max_bytes: None,
            log_backup_count: None,
        },
        health: HealthConfig { port: 0 },
        scheduler: None,
        files: None,
        gateway: None,
        bluebubbles: None,
    }
}

pub fn make_incoming(platform: &str, chat_id: &str, text: &str) -> IncomingMessage {
    IncomingMessage {
        platform: platform.to_string(),
        user_id: "user1".to_string(),
        username: None,
        chat_id: chat_id.to_string(),
        text: Some(text.to_string()),
        image_paths: vec![],
        audio_paths: vec![],
        file_paths: vec![],
        file_names: vec![],
    }
}

pub fn make_incoming_empty(platform: &str, chat_id: &str) -> IncomingMessage {
    IncomingMessage {
        platform: platform.to_string(),
        user_id: "user1".to_string(),
        username: None,
        chat_id: chat_id.to_string(),
        text: None,
        image_paths: vec![],
        audio_paths: vec![],
        file_paths: vec![],
        file_names: vec![],
    }
}

pub fn success_response(text: &str) -> ClaudeResponse {
    ClaudeResponse {
        result: text.to_string(),
        session_id: "test-session-id".to_string(),
        cost_usd: 0.01,
        duration_ms: 500,
        input_tokens: 50,
        output_tokens: 25,
    }
}

pub async fn wait_for_messages(
    sent: &Arc<std::sync::Mutex<Vec<(String, String)>>>,
    count: usize,
) {
    for _ in 0..500 {
        if sent.lock().unwrap().len() >= count {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!(
        "timed out waiting for {} messages, got {}",
        count,
        sent.lock().unwrap().len()
    );
}
