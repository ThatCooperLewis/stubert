use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::adapters::{IncomingMessage, PlatformAdapter};
use crate::config::types::StubbertConfig;
use crate::gateway::claude_cli::{display_model, resolve_model, ClaudeCallParams};
use crate::gateway::core::ClaudeCaller;
use crate::gateway::history::HistoryWriter;
use crate::gateway::session::SessionManager;
use crate::gateway::skills::SkillRegistry;

// ---- Constants ----

const KNOWN_COMMANDS: &[&str] = &[
    "new", "context", "restart", "models", "skill", "history", "status", "heartbeat", "help",
];

const NEW_SESSION_GREETING: &str = "A new session has began, please greet the user.";
const NEW_SESSION_CONFIRMATION: &str = "New session started · model: ";
const NEW_SESSION_GREETING_FAILED: &str = "Session started but greeting failed.";
const CONTEXT_PROMPT: &str = "Report your current context usage: how many tokens used out of the total context window, as a percentage and raw numbers. Be brief.";
const NO_ACTIVE_SESSION: &str = "No active session.";
const RESTARTING: &str = "Restarting...";
const UNKNOWN_MODEL: &str = "Unknown model. Available: sonnet, opus, haiku";
const UNKNOWN_SKILL: &str = "Unknown skill. Use /skill to list available skills.";
const HISTORY_USAGE: &str = "Usage: /history <search term>";
const HEARTBEAT_ALREADY_RUNNING: &str = "A heartbeat is already in progress.";
const HEARTBEAT_UNAVAILABLE: &str = "Heartbeat system not available.";

const MODEL_ALIASES: &[&str] = &["sonnet", "opus", "haiku"];

// ---- Traits ----

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait HeartbeatTrigger: Send + Sync {
    async fn trigger(&self) -> Result<String, String>;
    fn is_running(&self) -> bool;
    fn last_execution(&self) -> Option<std::time::Instant>;
}

// ---- Command Parsing ----

pub fn parse_command(text: &str) -> Option<(&'static str, &str)> {
    if !text.starts_with('/') {
        return None;
    }

    let (token, args) = match text.find(char::is_whitespace) {
        Some(idx) => (&text[..idx], text[idx..].trim()),
        None => (text, ""),
    };

    // Strip leading /
    let cmd = &token[1..];

    // Strip @botname suffix (Telegram group mentions)
    let cmd = match cmd.find('@') {
        Some(idx) => &cmd[..idx],
        None => cmd,
    };

    // Case-insensitive match against known commands
    let cmd_lower = cmd.to_lowercase();
    KNOWN_COMMANDS
        .iter()
        .find(|&&known| known == cmd_lower)
        .map(|&known| (known, args))
}

// ---- Simple Command Handlers ----

fn cmd_help() -> String {
    [
        "/new — Start a fresh session",
        "/context — Check context window usage",
        "/restart — Restart the bot",
        "/models [alias] — List or switch models",
        "/skill [name] [args] — List or invoke a skill",
        "/history <query> — Search conversation history",
        "/status — Show bot status",
        "/heartbeat — Trigger a heartbeat check",
        "/help — Show this help message",
    ]
    .join("\n")
}

async fn cmd_models(
    args: &str,
    platform: &str,
    chat_id: &str,
    session_manager: &Arc<Mutex<SessionManager>>,
) -> String {
    if args.is_empty() {
        let sm = session_manager.lock().await;
        let session_key = SessionManager::conversation_key(platform, chat_id);
        let current_model = sm
            .get(&session_key)
            .map(|s| s.model.as_str())
            .unwrap_or("claude-sonnet-4-6");

        let models = [
            ("sonnet", "claude-sonnet-4-6"),
            ("opus", "claude-opus-4-6"),
            ("haiku", "claude-haiku-4-5-20251001"),
        ];

        let lines: Vec<String> = models
            .iter()
            .map(|(alias, id)| {
                let display = display_model(id);
                if *id == current_model {
                    format!("* {alias} — {display}")
                } else {
                    format!("  {alias} — {display}")
                }
            })
            .collect();

        lines.join("\n")
    } else {
        let alias = args.trim().to_lowercase();
        if !MODEL_ALIASES.contains(&alias.as_str()) {
            return UNKNOWN_MODEL.to_string();
        }

        let full_model = resolve_model(&alias);
        let display = display_model(&full_model);

        let mut sm = session_manager.lock().await;
        sm.get_or_create(platform, chat_id).model = full_model;
        if let Err(e) = sm.save() {
            tracing::warn!(error = %e, "failed to save sessions after model switch");
        }

        format!("Switched to {display}")
    }
}

fn cmd_history(args: &str, platform: &str, history_writer: &HistoryWriter) -> String {
    if args.is_empty() {
        return HISTORY_USAGE.to_string();
    }

    let results = history_writer.search(platform, args, 20);
    if results.is_empty() {
        return format!("No results for \"{args}\"");
    }

    let mut output = Vec::new();
    for result in &results {
        output.push(format!(
            "**{}** (line {})",
            result.date, result.line_number
        ));
        for line in &result.context {
            if !line.is_empty() {
                output.push(line.clone());
            }
        }
        output.push(String::new());
    }

    output.join("\n").trim_end().to_string()
}

async fn cmd_status(
    platform: &str,
    chat_id: &str,
    session_manager: &Arc<Mutex<SessionManager>>,
    start_time: Option<Instant>,
) -> String {
    let sm = session_manager.lock().await;
    let session_key = SessionManager::conversation_key(platform, chat_id);

    let uptime = start_time
        .map(|st| {
            let elapsed = st.elapsed();
            let hours = elapsed.as_secs() / 3600;
            let minutes = (elapsed.as_secs() % 3600) / 60;
            format!("{hours}h {minutes}m")
        })
        .unwrap_or_else(|| "unknown".to_string());

    let active_sessions = sm.active_session_count();
    let processing_count = sm.processing_sessions().len();

    let model = sm
        .get(&session_key)
        .map(|s| display_model(&s.model))
        .unwrap_or_else(|| display_model("claude-sonnet-4-6"));

    format!(
        "Uptime: {uptime}\nActive sessions: {active_sessions}\nIn-flight: {processing_count}\nModel: {model}"
    )
}

async fn cmd_restart(platform: &str, chat_id: &str, config: &StubbertConfig) -> String {
    let origin = serde_json::json!({
        "platform": platform,
        "chat_id": chat_id,
    });

    let file_path =
        std::path::Path::new(&config.claude.working_directory).join("restart_origin.json");

    if let Err(e) = std::fs::write(&file_path, origin.to_string()) {
        tracing::warn!(error = %e, "failed to write restart_origin.json");
    }

    RESTARTING.to_string()
}

async fn cmd_heartbeat(heartbeat_trigger: &Option<Arc<dyn HeartbeatTrigger>>) -> String {
    match heartbeat_trigger {
        None => HEARTBEAT_UNAVAILABLE.to_string(),
        Some(trigger) => {
            if trigger.is_running() {
                return HEARTBEAT_ALREADY_RUNNING.to_string();
            }
            match trigger.trigger().await {
                Ok(result) => result,
                Err(e) => format!("Heartbeat failed: {e}"),
            }
        }
    }
}

// ---- Claude-Calling Command Helpers ----

async fn call_claude_for_command(
    prompt: &str,
    platform: &str,
    chat_id: &str,
    session_manager: &Arc<Mutex<SessionManager>>,
    claude_caller: &Arc<dyn ClaudeCaller>,
    history_writer: &Arc<HistoryWriter>,
    config: &StubbertConfig,
    allowed_tools_override: Option<Vec<String>>,
    add_dirs_override: Option<Vec<String>>,
) -> Result<String, String> {
    let session_key = SessionManager::conversation_key(platform, chat_id);

    let params = {
        let sm = session_manager.lock().await;
        let session = sm.get(&session_key).ok_or("No session found")?;
        let (_, session_id) = session.cli_flags();
        let is_new = !session.initiated;
        let model = session.model.clone();

        let allowed_tools =
            allowed_tools_override.or_else(|| config.claude.allowed_tools.get(platform).cloned());

        let add_dirs = add_dirs_override.or_else(|| {
            if config.claude.add_dirs.is_empty() {
                None
            } else {
                Some(config.claude.add_dirs.clone())
            }
        });

        ClaudeCallParams {
            prompt: prompt.to_string(),
            session_id,
            is_new_session: is_new,
            allowed_tools,
            add_dirs,
            model: Some(model),
            env_file_path: config.claude.env_file_path.clone(),
            timeout_secs: config.claude.timeout_secs,
            working_directory: config.claude.working_directory.clone(),
            cli_path: config.claude.cli_path.clone(),
        }
    };

    let result = claude_caller.call(&params).await.map_err(|e| e.to_string())?;

    // Mark initiated, save sessions, start inactivity timer
    {
        let mut sm = session_manager.lock().await;
        if let Some(session) = sm.get_mut(&session_key) {
            session.mark_initiated();
        }
        if let Err(e) = sm.save() {
            tracing::warn!(error = %e, "failed to save sessions");
        }
        sm.start_inactivity_timer(session_key);
    }

    history_writer.write(platform, "user", prompt);
    history_writer.write(platform, "assistant", &result.result);

    Ok(result.result)
}

// ---- Claude-Calling Commands ----

async fn cmd_new(
    platform: &str,
    chat_id: &str,
    adapter: &Arc<Mutex<dyn PlatformAdapter>>,
    session_manager: &Arc<Mutex<SessionManager>>,
    claude_caller: &Arc<dyn ClaudeCaller>,
    history_writer: &Arc<HistoryWriter>,
    config: &StubbertConfig,
) {
    // Reset session and get display model name
    let display = {
        let mut sm = session_manager.lock().await;
        let session_key = SessionManager::conversation_key(platform, chat_id);
        sm.reset_session(&session_key);
        let session = sm.get_or_create(platform, chat_id);
        display_model(&session.model)
    };

    // Send confirmation immediately
    let confirmation = format!("{NEW_SESSION_CONFIRMATION}{display}");
    if let Err(e) = adapter
        .lock()
        .await
        .send_message(chat_id, &confirmation)
        .await
    {
        tracing::warn!(error = %e, "failed to send new session confirmation");
    }

    // Call Claude with greeting prompt
    match call_claude_for_command(
        NEW_SESSION_GREETING,
        platform,
        chat_id,
        session_manager,
        claude_caller,
        history_writer,
        config,
        None,
        None,
    )
    .await
    {
        Ok(response) => {
            if let Err(e) = adapter
                .lock()
                .await
                .send_message(chat_id, &response)
                .await
            {
                tracing::warn!(error = %e, "failed to send greeting response");
            }
        }
        Err(_) => {
            if let Err(e) = adapter
                .lock()
                .await
                .send_message(chat_id, NEW_SESSION_GREETING_FAILED)
                .await
            {
                tracing::warn!(error = %e, "failed to send greeting failed message");
            }
        }
    }
}

async fn cmd_context(
    platform: &str,
    chat_id: &str,
    session_manager: &Arc<Mutex<SessionManager>>,
    claude_caller: &Arc<dyn ClaudeCaller>,
    history_writer: &Arc<HistoryWriter>,
    config: &StubbertConfig,
) -> String {
    let session_key = SessionManager::conversation_key(platform, chat_id);

    // Check if session is active (initiated)
    {
        let sm = session_manager.lock().await;
        match sm.get(&session_key) {
            Some(session) if session.initiated => {}
            _ => return NO_ACTIVE_SESSION.to_string(),
        }
    }

    match call_claude_for_command(
        CONTEXT_PROMPT,
        platform,
        chat_id,
        session_manager,
        claude_caller,
        history_writer,
        config,
        None,
        None,
    )
    .await
    {
        Ok(response) => response,
        Err(e) => format!("Failed: {e}"),
    }
}

async fn cmd_skill(
    args: &str,
    platform: &str,
    chat_id: &str,
    session_manager: &Arc<Mutex<SessionManager>>,
    claude_caller: &Arc<dyn ClaudeCaller>,
    history_writer: &Arc<HistoryWriter>,
    skill_registry: &Arc<SkillRegistry>,
    config: &StubbertConfig,
) -> String {
    if args.is_empty() {
        let skills = skill_registry.list_skills();
        if skills.is_empty() {
            return "No skills available.".to_string();
        }
        return skills
            .iter()
            .map(|s| format!("{} — {}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n");
    }

    // Parse: first word is skill name, rest is user args
    let (skill_name, user_args) = match args.find(char::is_whitespace) {
        Some(idx) => (&args[..idx], args[idx..].trim()),
        None => (args, ""),
    };

    let skill = match skill_registry.get(skill_name) {
        Some(s) => s,
        None => return UNKNOWN_SKILL.to_string(),
    };

    let prompt = match skill_registry.read_prompt(skill_name) {
        Some(p) => {
            if user_args.is_empty() {
                p
            } else {
                format!("{p}\n\n{user_args}")
            }
        }
        None => return "Failed to read skill prompt.".to_string(),
    };

    let allowed_tools = skill.allowed_tools.clone();
    let add_dirs = skill.add_dirs.clone();

    match call_claude_for_command(
        &prompt,
        platform,
        chat_id,
        session_manager,
        claude_caller,
        history_writer,
        config,
        allowed_tools,
        add_dirs,
    )
    .await
    {
        Ok(response) => response,
        Err(e) => format!("Skill failed: {e}"),
    }
}

// ---- Dispatch ----

pub async fn dispatch_command(
    name: &str,
    args: &str,
    msg: &IncomingMessage,
    adapter: Arc<Mutex<dyn PlatformAdapter>>,
    session_manager: Arc<Mutex<SessionManager>>,
    claude_caller: Arc<dyn ClaudeCaller>,
    history_writer: Arc<HistoryWriter>,
    skill_registry: Arc<SkillRegistry>,
    config: StubbertConfig,
    start_time: Option<Instant>,
    heartbeat_trigger: Option<Arc<dyn HeartbeatTrigger>>,
) {
    let platform = &msg.platform;
    let chat_id = &msg.chat_id;

    let response = match name {
        "help" => Some(cmd_help()),
        "models" => Some(cmd_models(args, platform, chat_id, &session_manager).await),
        "history" => Some(cmd_history(args, platform, &history_writer)),
        "status" => {
            Some(cmd_status(platform, chat_id, &session_manager, start_time).await)
        }
        "restart" => {
            let text = cmd_restart(platform, chat_id, &config).await;
            if let Err(e) = adapter.lock().await.send_message(chat_id, &text).await {
                tracing::warn!(error = %e, "failed to send restart response");
            }
            // Spawn delayed shutdown — the main process signal handler
            // will catch this and run graceful shutdown.
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                #[cfg(unix)]
                {
                    // Send SIGTERM to ourselves for graceful shutdown
                    unsafe {
                        libc::kill(libc::getpid(), libc::SIGTERM);
                    }
                }
            });
            None // Already sent
        }
        "heartbeat" => Some(cmd_heartbeat(&heartbeat_trigger).await),
        "new" => {
            cmd_new(
                platform,
                chat_id,
                &adapter,
                &session_manager,
                &claude_caller,
                &history_writer,
                &config,
            )
            .await;
            None // Sends messages internally
        }
        "context" => Some(
            cmd_context(
                platform,
                chat_id,
                &session_manager,
                &claude_caller,
                &history_writer,
                &config,
            )
            .await,
        ),
        "skill" => Some(
            cmd_skill(
                args,
                platform,
                chat_id,
                &session_manager,
                &claude_caller,
                &history_writer,
                &skill_registry,
                &config,
            )
            .await,
        ),
        _ => None,
    };

    if let Some(text) = response {
        if let Err(e) = adapter.lock().await.send_message(chat_id, &text).await {
            tracing::warn!(error = %e, "failed to send command response");
        }
    }
}

// ---- Tests ----

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::MockPlatformAdapter;
    use crate::config::types::*;
    use crate::gateway::claude_cli::{ClaudeError, ClaudeResponse};
    use crate::gateway::core::MockClaudeCaller;
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn make_config() -> StubbertConfig {
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
                working_directory: "/tmp".to_string(),
                env_file_path: ".env".to_string(),
                allowed_tools: HashMap::new(),
                add_dirs: vec![],
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
            health: HealthConfig { port: 8484 },
            scheduler: None,
            files: None,
            gateway: None,
        }
    }

    fn make_incoming(platform: &str, chat_id: &str, text: &str) -> IncomingMessage {
        IncomingMessage {
            platform: platform.to_string(),
            user_id: "user1".to_string(),
            chat_id: chat_id.to_string(),
            text: Some(text.to_string()),
            image_paths: vec![],
            audio_paths: vec![],
            file_paths: vec![],
            file_names: vec![],
        }
    }

    fn make_session_manager(dir: &std::path::Path) -> SessionManager {
        SessionManager::new(
            dir.join("sessions.json"),
            60,
            "claude-sonnet-4-6".to_string(),
        )
    }

    fn claude_success(text: &str) -> Result<ClaudeResponse, ClaudeError> {
        Ok(ClaudeResponse {
            result: text.to_string(),
            session_id: "mock-session".to_string(),
            cost_usd: 0.01,
            duration_ms: 500,
            input_tokens: 50,
            output_tokens: 25,
        })
    }

    fn claude_exit_error() -> Result<ClaudeResponse, ClaudeError> {
        Err(ClaudeError::ExitError {
            code: 1,
            stderr: "session error".to_string(),
        })
    }

    /// Returns a mock adapter that captures sent messages.
    fn mock_adapter_capturing() -> (
        Arc<Mutex<dyn PlatformAdapter>>,
        Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let sent = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sent_clone = sent.clone();
        let mut mock = MockPlatformAdapter::new();
        mock.expect_send_message().returning(move |_, text| {
            sent_clone.lock().unwrap().push(text.to_string());
            Ok(())
        });
        mock.expect_send_typing().returning(|_| Ok(()));
        (Arc::new(Mutex::new(mock)), sent)
    }

    // ---- parse_command tests ----

    mod test_parse_command {
        use super::super::*;

        #[test]
        fn help_command() {
            assert_eq!(parse_command("/help"), Some(("help", "")));
        }

        #[test]
        fn models_with_args() {
            assert_eq!(parse_command("/models sonnet"), Some(("models", "sonnet")));
        }

        #[test]
        fn botname_stripped() {
            assert_eq!(
                parse_command("/models@stubert_bot sonnet"),
                Some(("models", "sonnet"))
            );
        }

        #[test]
        fn unknown_command() {
            assert_eq!(parse_command("/unknown"), None);
        }

        #[test]
        fn no_slash_prefix() {
            assert_eq!(parse_command("hello"), None);
        }

        #[test]
        fn case_insensitive() {
            assert_eq!(parse_command("/NEW"), Some(("new", "")));
        }

        #[test]
        fn skill_preserves_args() {
            assert_eq!(
                parse_command("/skill trello create card"),
                Some(("skill", "trello create card"))
            );
        }

        #[test]
        fn empty_string() {
            assert_eq!(parse_command(""), None);
        }

        #[test]
        fn just_slash() {
            assert_eq!(parse_command("/"), None);
        }
    }

    // ---- cmd_help tests ----

    mod test_cmd_help {
        use super::super::*;

        #[test]
        fn contains_all_commands() {
            let result = cmd_help();
            for cmd in KNOWN_COMMANDS {
                assert!(
                    result.contains(&format!("/{cmd}")),
                    "missing /{cmd} in help"
                );
            }
        }

        #[test]
        fn format_is_slash_dash_description() {
            let result = cmd_help();
            for line in result.lines() {
                assert!(
                    line.starts_with('/') && line.contains(" — "),
                    "bad format: {line}"
                );
            }
        }
    }

    // ---- cmd_models tests ----

    mod test_cmd_models {
        use super::*;

        #[tokio::test]
        async fn no_args_lists_with_active_marked() {
            let dir = TempDir::new().unwrap();
            let sm = Arc::new(Mutex::new(make_session_manager(dir.path())));

            let result = cmd_models("", "telegram", "123", &sm).await;

            assert!(result.contains("* sonnet — Sonnet 4.6"));
            assert!(result.contains("  opus — Opus 4.6"));
            assert!(result.contains("  haiku — Haiku 4.5"));
        }

        #[tokio::test]
        async fn valid_alias_switches_model() {
            let dir = TempDir::new().unwrap();
            let mut mgr = make_session_manager(dir.path());
            mgr.get_or_create("telegram", "123");
            let sm = Arc::new(Mutex::new(mgr));

            let result = cmd_models("opus", "telegram", "123", &sm).await;

            assert!(result.contains("Switched to Opus 4.6"));
            let locked = sm.lock().await;
            let key = SessionManager::conversation_key("telegram", "123");
            assert_eq!(locked.get(&key).unwrap().model, "claude-opus-4-6");
        }

        #[tokio::test]
        async fn invalid_alias_returns_error() {
            let dir = TempDir::new().unwrap();
            let sm = Arc::new(Mutex::new(make_session_manager(dir.path())));

            let result = cmd_models("gpt4", "telegram", "123", &sm).await;

            assert_eq!(result, UNKNOWN_MODEL);
        }

        #[tokio::test]
        async fn default_model_shows_correctly() {
            let dir = TempDir::new().unwrap();
            let mut mgr = make_session_manager(dir.path());
            mgr.get_or_create("telegram", "123");
            let sm = Arc::new(Mutex::new(mgr));

            let result = cmd_models("", "telegram", "123", &sm).await;

            // Default is sonnet, should be marked
            assert!(result.contains("* sonnet"));
        }
    }

    // ---- cmd_history tests ----

    mod test_cmd_history {
        use super::*;
        use crate::gateway::history::HistoryWriter;

        #[test]
        fn no_args_returns_usage() {
            let dir = TempDir::new().unwrap();
            let hw = HistoryWriter::new(dir.path().to_path_buf());
            assert_eq!(cmd_history("", "telegram", &hw), HISTORY_USAGE);
        }

        #[test]
        fn with_query_returns_results() {
            let dir = TempDir::new().unwrap();
            let hw = HistoryWriter::new(dir.path().to_path_buf());
            hw.write("telegram", "user", "hello world");

            let result = cmd_history("hello", "telegram", &hw);
            assert!(result.contains("hello world"));
        }

        #[test]
        fn no_results_returns_message() {
            let dir = TempDir::new().unwrap();
            let hw = HistoryWriter::new(dir.path().to_path_buf());
            hw.write("telegram", "user", "hello");

            let result = cmd_history("xyzzyx", "telegram", &hw);
            assert!(result.contains("No results"));
        }
    }

    // ---- cmd_status tests ----

    mod test_cmd_status {
        use super::*;

        #[tokio::test]
        async fn returns_formatted_status() {
            let dir = TempDir::new().unwrap();
            let mut mgr = make_session_manager(dir.path());
            mgr.get_or_create("telegram", "123");
            let sm = Arc::new(Mutex::new(mgr));

            let result =
                cmd_status("telegram", "123", &sm, Some(Instant::now())).await;

            assert!(result.contains("Uptime:"));
            assert!(result.contains("Active sessions: 1"));
            assert!(result.contains("In-flight: 0"));
            assert!(result.contains("Model: Sonnet 4.6"));
        }
    }

    // ---- cmd_restart tests ----

    mod test_cmd_restart {
        use super::*;

        #[tokio::test]
        async fn writes_restart_origin_and_returns_message() {
            let dir = TempDir::new().unwrap();
            let mut config = make_config();
            config.claude.working_directory = dir.path().to_str().unwrap().to_string();

            let result = cmd_restart("telegram", "123", &config).await;

            assert_eq!(result, RESTARTING);

            let file_path = dir.path().join("restart_origin.json");
            assert!(file_path.exists());
            let content: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(file_path).unwrap()).unwrap();
            assert_eq!(content["platform"], "telegram");
            assert_eq!(content["chat_id"], "123");
        }
    }

    // ---- cmd_heartbeat tests ----

    mod test_cmd_heartbeat {
        use super::*;

        #[tokio::test]
        async fn trigger_succeeds() {
            let mut mock = MockHeartbeatTrigger::new();
            mock.expect_is_running().returning(|| false);
            mock.expect_trigger()
                .returning(|| Ok("Heartbeat OK".to_string()));
            let trigger: Option<Arc<dyn HeartbeatTrigger>> = Some(Arc::new(mock));

            let result = cmd_heartbeat(&trigger).await;
            assert_eq!(result, "Heartbeat OK");
        }

        #[tokio::test]
        async fn already_running() {
            let mut mock = MockHeartbeatTrigger::new();
            mock.expect_is_running().returning(|| true);
            let trigger: Option<Arc<dyn HeartbeatTrigger>> = Some(Arc::new(mock));

            let result = cmd_heartbeat(&trigger).await;
            assert_eq!(result, HEARTBEAT_ALREADY_RUNNING);
        }

        #[tokio::test]
        async fn no_heartbeat_system() {
            let result = cmd_heartbeat(&None).await;
            assert_eq!(result, HEARTBEAT_UNAVAILABLE);
        }
    }

    // ---- cmd_new tests ----

    mod test_cmd_new {
        use super::*;

        #[tokio::test]
        async fn resets_session_sends_confirmation_and_greeting() {
            let dir = TempDir::new().unwrap();
            let mut mgr = make_session_manager(dir.path());
            mgr.get_or_create("telegram", "123");
            let key = SessionManager::conversation_key("telegram", "123");
            let old_uuid = mgr.get(&key).unwrap().session_id;
            let sm = Arc::new(Mutex::new(mgr));

            let (adapter, sent) = mock_adapter_capturing();

            let mut mock_claude = MockClaudeCaller::new();
            mock_claude
                .expect_call()
                .returning(|_| claude_success("Hello! I'm Stubert."));
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);
            let hw = Arc::new(HistoryWriter::new(dir.path().join("history")));
            let config = make_config();

            cmd_new("telegram", "123", &adapter, &sm, &cc, &hw, &config).await;

            // Session should have new UUID
            let locked_sm = sm.lock().await;
            let session = locked_sm.get(&key).unwrap();
            assert_ne!(session.session_id, old_uuid);
            assert!(session.initiated);
            drop(locked_sm);

            // Should have sent confirmation + greeting
            let msgs = sent.lock().unwrap();
            assert_eq!(msgs.len(), 2);
            assert!(msgs[0].starts_with("New session started · model:"));
            assert_eq!(msgs[1], "Hello! I'm Stubert.");
        }

        #[tokio::test]
        async fn cli_failure_sends_failure_message() {
            let dir = TempDir::new().unwrap();
            let sm = Arc::new(Mutex::new(make_session_manager(dir.path())));

            let (adapter, sent) = mock_adapter_capturing();

            let mut mock_claude = MockClaudeCaller::new();
            mock_claude
                .expect_call()
                .returning(|_| claude_exit_error());
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);
            let hw = Arc::new(HistoryWriter::new(dir.path().join("history")));
            let config = make_config();

            cmd_new("telegram", "123", &adapter, &sm, &cc, &hw, &config).await;

            let msgs = sent.lock().unwrap();
            assert_eq!(msgs.len(), 2);
            assert!(msgs[0].starts_with("New session started · model:"));
            assert_eq!(msgs[1], NEW_SESSION_GREETING_FAILED);
        }
    }

    // ---- cmd_context tests ----

    mod test_cmd_context {
        use super::*;

        #[tokio::test]
        async fn active_session_calls_claude() {
            let dir = TempDir::new().unwrap();
            let mut mgr = make_session_manager(dir.path());
            mgr.get_or_create("telegram", "123");
            let key = SessionManager::conversation_key("telegram", "123");
            mgr.get_mut(&key).unwrap().mark_initiated();
            let sm = Arc::new(Mutex::new(mgr));

            let mut mock_claude = MockClaudeCaller::new();
            mock_claude
                .expect_call()
                .returning(|_| claude_success("50% used (100k/200k tokens)"));
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);
            let hw = Arc::new(HistoryWriter::new(dir.path().join("history")));
            let config = make_config();

            let result =
                cmd_context("telegram", "123", &sm, &cc, &hw, &config).await;

            assert!(result.contains("50%"));
        }

        #[tokio::test]
        async fn no_active_session() {
            let dir = TempDir::new().unwrap();
            let sm = Arc::new(Mutex::new(make_session_manager(dir.path())));

            let mut mock_claude = MockClaudeCaller::new();
            mock_claude.expect_call().never();
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);
            let hw = Arc::new(HistoryWriter::new(dir.path().join("history")));
            let config = make_config();

            let result =
                cmd_context("telegram", "123", &sm, &cc, &hw, &config).await;

            assert_eq!(result, NO_ACTIVE_SESSION);
        }
    }

    // ---- cmd_skill tests ----

    mod test_cmd_skill {
        use super::*;
        use crate::gateway::skills::SkillRegistry;

        fn make_skill_registry(dir: &std::path::Path) -> Arc<SkillRegistry> {
            let skills_dir = dir.join("skills");
            std::fs::create_dir_all(&skills_dir).unwrap();
            std::fs::write(
                skills_dir.join("trello.md"),
                "---\nname: trello\ndescription: Manage Trello boards\nallowed_tools:\n  - Bash\nadd_dirs:\n  - /trello\n---\nCreate a Trello card.",
            )
            .unwrap();
            let mut registry = SkillRegistry::new(skills_dir);
            registry.discover();
            Arc::new(registry)
        }

        #[tokio::test]
        async fn no_args_lists_skills() {
            let dir = TempDir::new().unwrap();
            let sr = make_skill_registry(dir.path());
            let sm = Arc::new(Mutex::new(make_session_manager(dir.path())));
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude.expect_call().never();
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);
            let hw = Arc::new(HistoryWriter::new(dir.path().join("history")));
            let config = make_config();

            let result =
                cmd_skill("", "telegram", "123", &sm, &cc, &hw, &sr, &config).await;

            assert!(result.contains("trello — Manage Trello boards"));
        }

        #[tokio::test]
        async fn invokes_skill_prompt() {
            let dir = TempDir::new().unwrap();
            let sr = make_skill_registry(dir.path());
            let mut mgr = make_session_manager(dir.path());
            mgr.get_or_create("telegram", "123");
            let sm = Arc::new(Mutex::new(mgr));

            let prompts_seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let ps = prompts_seen.clone();
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude.expect_call().returning(move |params| {
                ps.lock().unwrap().push(params.prompt.clone());
                claude_success("Card created!")
            });
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);
            let hw = Arc::new(HistoryWriter::new(dir.path().join("history")));
            let config = make_config();

            let result =
                cmd_skill("trello", "telegram", "123", &sm, &cc, &hw, &sr, &config).await;

            assert_eq!(result, "Card created!");
            let prompts = prompts_seen.lock().unwrap();
            assert!(prompts[0].contains("Create a Trello card."));
        }

        #[tokio::test]
        async fn skill_with_user_args() {
            let dir = TempDir::new().unwrap();
            let sr = make_skill_registry(dir.path());
            let mut mgr = make_session_manager(dir.path());
            mgr.get_or_create("telegram", "123");
            let sm = Arc::new(Mutex::new(mgr));

            let prompts_seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let ps = prompts_seen.clone();
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude.expect_call().returning(move |params| {
                ps.lock().unwrap().push(params.prompt.clone());
                claude_success("Done!")
            });
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);
            let hw = Arc::new(HistoryWriter::new(dir.path().join("history")));
            let config = make_config();

            let result = cmd_skill(
                "trello buy groceries",
                "telegram",
                "123",
                &sm,
                &cc,
                &hw,
                &sr,
                &config,
            )
            .await;

            assert_eq!(result, "Done!");
            let prompts = prompts_seen.lock().unwrap();
            assert!(prompts[0].contains("Create a Trello card."));
            assert!(prompts[0].contains("buy groceries"));
        }

        #[tokio::test]
        async fn unknown_skill_returns_error() {
            let dir = TempDir::new().unwrap();
            let sr = make_skill_registry(dir.path());
            let sm = Arc::new(Mutex::new(make_session_manager(dir.path())));
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude.expect_call().never();
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);
            let hw = Arc::new(HistoryWriter::new(dir.path().join("history")));
            let config = make_config();

            let result = cmd_skill(
                "nonexistent",
                "telegram",
                "123",
                &sm,
                &cc,
                &hw,
                &sr,
                &config,
            )
            .await;

            assert_eq!(result, UNKNOWN_SKILL);
        }

        #[tokio::test]
        async fn skill_uses_allowed_tools_override() {
            let dir = TempDir::new().unwrap();
            let sr = make_skill_registry(dir.path());
            let mut mgr = make_session_manager(dir.path());
            mgr.get_or_create("telegram", "123");
            let sm = Arc::new(Mutex::new(mgr));

            let tools_seen = Arc::new(std::sync::Mutex::new(Vec::<Option<Vec<String>>>::new()));
            let ts = tools_seen.clone();
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude.expect_call().returning(move |params| {
                ts.lock().unwrap().push(params.allowed_tools.clone());
                claude_success("ok")
            });
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);
            let hw = Arc::new(HistoryWriter::new(dir.path().join("history")));
            let config = make_config();

            cmd_skill("trello", "telegram", "123", &sm, &cc, &hw, &sr, &config).await;

            let tools = tools_seen.lock().unwrap();
            assert_eq!(
                tools[0].as_ref().unwrap(),
                &vec!["Bash".to_string()]
            );
        }

        #[tokio::test]
        async fn skill_uses_add_dirs_override() {
            let dir = TempDir::new().unwrap();
            let sr = make_skill_registry(dir.path());
            let mut mgr = make_session_manager(dir.path());
            mgr.get_or_create("telegram", "123");
            let sm = Arc::new(Mutex::new(mgr));

            let dirs_seen =
                Arc::new(std::sync::Mutex::new(Vec::<Option<Vec<String>>>::new()));
            let ds = dirs_seen.clone();
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude.expect_call().returning(move |params| {
                ds.lock().unwrap().push(params.add_dirs.clone());
                claude_success("ok")
            });
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);
            let hw = Arc::new(HistoryWriter::new(dir.path().join("history")));
            let config = make_config();

            cmd_skill("trello", "telegram", "123", &sm, &cc, &hw, &sr, &config).await;

            let dirs = dirs_seen.lock().unwrap();
            assert_eq!(
                dirs[0].as_ref().unwrap(),
                &vec!["/trello".to_string()]
            );
        }
    }

    // ---- dispatch_command tests ----

    mod test_dispatch_command {
        use super::*;
        use crate::gateway::skills::SkillRegistry;

        fn empty_skill_registry() -> Arc<SkillRegistry> {
            let dir = TempDir::new().unwrap();
            Arc::new(SkillRegistry::new(dir.path().to_path_buf()))
        }

        #[tokio::test]
        async fn routes_help_command() {
            let dir = TempDir::new().unwrap();
            let (adapter, sent) = mock_adapter_capturing();
            let sm = Arc::new(Mutex::new(make_session_manager(dir.path())));
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude.expect_call().never();
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);
            let hw = Arc::new(HistoryWriter::new(dir.path().join("history")));
            let msg = make_incoming("telegram", "123", "/help");

            dispatch_command(
                "help",
                "",
                &msg,
                adapter,
                sm,
                cc,
                hw,
                empty_skill_registry(),
                make_config(),
                None,
                None,
            )
            .await;

            let msgs = sent.lock().unwrap();
            assert_eq!(msgs.len(), 1);
            assert!(msgs[0].contains("/help"));
        }

        #[tokio::test]
        async fn routes_status_command() {
            let dir = TempDir::new().unwrap();
            let (adapter, sent) = mock_adapter_capturing();
            let sm = Arc::new(Mutex::new(make_session_manager(dir.path())));
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude.expect_call().never();
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);
            let hw = Arc::new(HistoryWriter::new(dir.path().join("history")));
            let msg = make_incoming("telegram", "123", "/status");

            dispatch_command(
                "status",
                "",
                &msg,
                adapter,
                sm,
                cc,
                hw,
                empty_skill_registry(),
                make_config(),
                Some(Instant::now()),
                None,
            )
            .await;

            let msgs = sent.lock().unwrap();
            assert_eq!(msgs.len(), 1);
            assert!(msgs[0].contains("Uptime:"));
        }
    }
}
