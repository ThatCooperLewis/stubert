use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use uuid::Uuid;

use crate::adapters::{IncomingMessage, MessageHandler, PlatformAdapter};
use crate::config::types::StubbertConfig;
use crate::gateway::claude_cli::{call_claude, ClaudeCallParams, ClaudeError, ClaudeResponse};
use crate::gateway::commands::{dispatch_command, parse_command, HeartbeatTrigger};
use crate::gateway::history::HistoryWriter;
use crate::gateway::scheduler::{load_schedules, TaskScheduler};
use crate::gateway::session::SessionManager;
use crate::gateway::skills::SkillRegistry;

// Constants
const RESTART_MESSAGE: &str = "Bot is restarting, one moment.";
const SESSION_FAILURE_MESSAGE: &str = "Session restore failure, starting fresh.";
const ERROR_MESSAGE: &str = "Something went wrong, try again.";
const FILES_CLEANUP_DAYS: u64 = 30;
const TYPING_INTERVAL_SECS: u64 = 5;
const RESTART_ORIGIN_FILE: &str = "restart_origin.json";
const RESTART_GREETING_PROMPT: &str =
    "You were just restarted. Send a brief greeting to let the user know you're back online.";

// ---- Traits ----

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait ClaudeCaller: Send + Sync {
    async fn call(&self, params: &ClaudeCallParams) -> Result<ClaudeResponse, ClaudeError>;
}

pub struct RealClaudeCaller;

#[async_trait]
impl ClaudeCaller for RealClaudeCaller {
    async fn call(&self, params: &ClaudeCallParams) -> Result<ClaudeResponse, ClaudeError> {
        call_claude(params).await
    }
}

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait Transcriber: Send + Sync {
    async fn transcribe(&self, audio_path: &Path) -> Result<String, String>;
}

// ---- Pure Functions ----

pub async fn build_prompt(
    msg: &IncomingMessage,
    transcriber: Option<&dyn Transcriber>,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    // Audio transcription
    if !msg.audio_paths.is_empty() {
        if let Some(transcriber) = transcriber {
            for audio_path in &msg.audio_paths {
                match transcriber.transcribe(audio_path).await {
                    Ok(text) => {
                        parts.push(format!("[Voice transcription]: {text}"));
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %audio_path.display(),
                            error = %e,
                            "audio transcription failed"
                        );
                    }
                }
            }
        }
    }

    // Text
    if let Some(ref text) = msg.text {
        if !text.is_empty() {
            parts.push(text.clone());
        }
    }

    // File references
    for (i, file_path) in msg.file_paths.iter().enumerate() {
        let name = msg
            .file_names
            .get(i)
            .map(|s| s.as_str())
            .unwrap_or("file");
        parts.push(format!("`{name}`: {}", file_path.display()));
    }

    // Image references
    for image_path in &msg.image_paths {
        parts.push(image_path.display().to_string());
    }

    let result = parts.join("\n\n");
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

pub fn cleanup_old_files(dir: &Path, max_age_days: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let cutoff = std::time::SystemTime::now() - Duration::from_secs(max_age_days * 86400);

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Recurse into subdirectories first
            cleanup_old_files(&path, max_age_days);
            // Remove directory if now empty
            if let Ok(mut dir_entries) = std::fs::read_dir(&path) {
                if dir_entries.next().is_none() {
                    if let Err(e) = std::fs::remove_dir(&path) {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "failed to remove empty directory"
                        );
                    }
                }
            }
        } else if path.is_file() {
            if let Ok(metadata) = path.metadata() {
                if let Ok(modified) = metadata.modified() {
                    if modified < cutoff {
                        if let Err(e) = std::fs::remove_file(&path) {
                            tracing::warn!(
                                path = %path.display(),
                                error = %e,
                                "failed to remove old file"
                            );
                        }
                    }
                }
            }
        }
    }
}

// ---- Async Helpers ----

#[derive(Deserialize)]
struct RestartOrigin {
    platform: String,
    chat_id: String,
}

pub async fn handle_restart_greeting(
    dir: &Path,
    adapters: &Arc<Mutex<HashMap<String, Arc<Mutex<dyn PlatformAdapter>>>>>,
    claude_caller: &Arc<dyn ClaudeCaller>,
    config: &StubbertConfig,
) {
    let file_path = dir.join(RESTART_ORIGIN_FILE);
    if !file_path.exists() {
        return;
    }

    let origin = match std::fs::read_to_string(&file_path) {
        Ok(data) => match serde_json::from_str::<RestartOrigin>(&data) {
            Ok(origin) => origin,
            Err(e) => {
                tracing::warn!(error = %e, "invalid restart_origin.json, deleting");
                let _ = std::fs::remove_file(&file_path);
                return;
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "failed to read restart_origin.json");
            let _ = std::fs::remove_file(&file_path);
            return;
        }
    };

    // Find the adapter for the platform
    let adapter = {
        let adapters = adapters.lock().await;
        adapters.get(&origin.platform).cloned()
    };

    let Some(adapter) = adapter else {
        tracing::warn!(
            platform = %origin.platform,
            "no adapter for restart greeting platform"
        );
        let _ = std::fs::remove_file(&file_path);
        return;
    };

    // Call Claude with greeting prompt
    let session_id = Uuid::new_v4().to_string();
    let params = build_claude_params(
        RESTART_GREETING_PROMPT,
        &session_id,
        true,
        &config.claude.default_model,
        &origin.platform,
        config,
    );

    match claude_caller.call(&params).await {
        Ok(response) => {
            let adapter = adapter.lock().await;
            if let Err(e) = adapter.send_message(&origin.chat_id, &response.result).await {
                tracing::warn!(error = %e, "failed to send restart greeting");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "claude call failed for restart greeting");
        }
    }

    let _ = std::fs::remove_file(&file_path);
}

async fn typing_loop(adapter: Arc<Mutex<dyn PlatformAdapter>>, chat_id: String) {
    loop {
        if let Err(e) = adapter.lock().await.send_typing(&chat_id).await {
            tracing::warn!(
                chat_id = %chat_id,
                error = %e,
                "typing indicator failed"
            );
        }
        tokio::time::sleep(Duration::from_secs(TYPING_INTERVAL_SECS)).await;
    }
}

// ---- Core Processing ----

fn build_claude_params(
    prompt: &str,
    session_id: &str,
    is_new_session: bool,
    model: &str,
    platform: &str,
    config: &StubbertConfig,
) -> ClaudeCallParams {
    ClaudeCallParams {
        prompt: prompt.to_string(),
        session_id: session_id.to_string(),
        is_new_session,
        allowed_tools: config.claude.allowed_tools.get(platform).cloned(),
        add_dirs: if config.claude.add_dirs.is_empty() {
            None
        } else {
            Some(config.claude.add_dirs.clone())
        },
        model: Some(model.to_string()),
        env_file_path: config.claude.env_file_path.clone(),
        timeout_secs: config.claude.timeout_secs,
        working_directory: config.claude.working_directory.clone(),
        cli_path: config.claude.cli_path.clone(),
    }
}

async fn handle_claude_success(
    session_key: &str,
    prompt: &str,
    platform: &str,
    chat_id: &str,
    response: &ClaudeResponse,
    adapter: &Arc<Mutex<dyn PlatformAdapter>>,
    session_manager: &Arc<Mutex<SessionManager>>,
    history_writer: &Arc<HistoryWriter>,
) {
    {
        let mut sm = session_manager.lock().await;
        if let Some(session) = sm.get_mut(session_key) {
            session.mark_initiated();
        }
        if let Err(e) = sm.save() {
            tracing::warn!(error = %e, "failed to save sessions");
        }
    }

    history_writer.write(platform, "user", prompt);
    history_writer.write(platform, "assistant", &response.result);

    if let Err(e) = adapter
        .lock()
        .await
        .send_message(chat_id, &response.result)
        .await
    {
        tracing::warn!(error = %e, "failed to send response");
    }

    {
        let mut sm = session_manager.lock().await;
        sm.start_inactivity_timer(session_key.to_string());
    }
}

async fn process_prompt(
    session_key: &str,
    prompt: &str,
    platform: &str,
    chat_id: &str,
    adapter: Arc<Mutex<dyn PlatformAdapter>>,
    session_manager: Arc<Mutex<SessionManager>>,
    claude_caller: Arc<dyn ClaudeCaller>,
    history_writer: Arc<HistoryWriter>,
    config: &StubbertConfig,
) {
    // Build params from current session state
    let (params, initiated) = {
        let sm = session_manager.lock().await;
        let session = sm.get(session_key).expect("session must exist");
        let (_, session_id) = session.cli_flags();
        let is_new = !session.initiated;
        let model = session.model.clone();
        let initiated = session.initiated;
        (
            build_claude_params(prompt, &session_id, is_new, &model, platform, config),
            initiated,
        )
    };

    // Start typing indicator
    let typing_handle = tokio::spawn(typing_loop(adapter.clone(), chat_id.to_string()));

    // Call Claude
    let result = claude_caller.call(&params).await;

    // Stop typing indicator
    typing_handle.abort();
    let _ = typing_handle.await;

    match result {
        Ok(response) => {
            handle_claude_success(
                session_key,
                prompt,
                platform,
                chat_id,
                &response,
                &adapter,
                &session_manager,
                &history_writer,
            )
            .await;
        }
        Err(ClaudeError::ExitError { .. }) if initiated => {
            // Resume failure — notify user and retry with fresh session
            if let Err(e) = adapter
                .lock()
                .await
                .send_message(chat_id, SESSION_FAILURE_MESSAGE)
                .await
            {
                tracing::warn!(error = %e, "failed to send session failure message");
            }

            // Reset session and build fresh params
            let retry_params = {
                let mut sm = session_manager.lock().await;
                sm.reset_session(session_key);
                let session = sm.get(session_key).expect("session must exist after reset");
                let (_, session_id) = session.cli_flags();
                let model = session.model.clone();
                build_claude_params(prompt, &session_id, true, &model, platform, config)
            };

            // Retry with fresh session
            let typing_handle = tokio::spawn(typing_loop(adapter.clone(), chat_id.to_string()));
            let retry_result = claude_caller.call(&retry_params).await;
            typing_handle.abort();
            let _ = typing_handle.await;

            match retry_result {
                Ok(response) => {
                    handle_claude_success(
                        session_key,
                        prompt,
                        platform,
                        chat_id,
                        &response,
                        &adapter,
                        &session_manager,
                        &history_writer,
                    )
                    .await;
                }
                Err(e) => {
                    tracing::error!(error = %e, "retry with fresh session also failed");
                    if let Err(e) = adapter
                        .lock()
                        .await
                        .send_message(chat_id, ERROR_MESSAGE)
                        .await
                    {
                        tracing::warn!(error = %e, "failed to send error message");
                    }
                }
            }
        }
        Err(ClaudeError::Timeout { timeout_secs }) => {
            let msg = format!("Claude timed out after {timeout_secs}s. Try a shorter request.");
            if let Err(e) = adapter.lock().await.send_message(chat_id, &msg).await {
                tracing::warn!(error = %e, "failed to send timeout message");
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "claude call failed");
            if let Err(e) = adapter
                .lock()
                .await
                .send_message(chat_id, ERROR_MESSAGE)
                .await
            {
                tracing::warn!(error = %e, "failed to send error message");
            }
        }
    }
}

fn try_drain_batch(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> Option<String> {
    let first = rx.try_recv().ok()?;
    let mut messages = vec![first];
    while let Ok(msg) = rx.try_recv() {
        messages.push(msg);
    }
    if messages.len() == 1 {
        Some(messages.pop().unwrap())
    } else {
        Some(format!(
            "Batched messages from user:\n{}",
            messages.join("\n")
        ))
    }
}

async fn consume_queue(
    session_key: String,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    platform: String,
    chat_id: String,
    adapter: Arc<Mutex<dyn PlatformAdapter>>,
    session_manager: Arc<Mutex<SessionManager>>,
    claude_caller: Arc<dyn ClaudeCaller>,
    history_writer: Arc<HistoryWriter>,
    consumer_tasks: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
    config: StubbertConfig,
) {
    // Mark session as processing
    {
        let mut sm = session_manager.lock().await;
        if let Some(s) = sm.get_mut(&session_key) {
            s.processing = true;
        }
    }

    loop {
        // Try to drain all available messages (non-blocking)
        let prompt = try_drain_batch(&mut rx);
        if let Some(prompt) = prompt {
            process_prompt(
                &session_key,
                &prompt,
                &platform,
                &chat_id,
                adapter.clone(),
                session_manager.clone(),
                claude_caller.clone(),
                history_writer.clone(),
                &config,
            )
            .await;
            continue;
        }

        // No messages found. Take the tasks lock and do a final check.
        let mut tasks = consumer_tasks.lock().await;
        let prompt = try_drain_batch(&mut rx);
        if let Some(prompt) = prompt {
            drop(tasks);
            process_prompt(
                &session_key,
                &prompt,
                &platform,
                &chat_id,
                adapter.clone(),
                session_manager.clone(),
                claude_caller.clone(),
                history_writer.clone(),
                &config,
            )
            .await;
            continue;
        }

        // Truly empty. Clean up and exit.
        let mut sm = session_manager.lock().await;
        if let Some(s) = sm.get_mut(&session_key) {
            s.processing = false;
            s.return_rx(rx);
        }
        tasks.remove(&session_key);
        return;
    }
}

// ---- Message Routing ----

async fn handle_message(
    msg: IncomingMessage,
    session_manager: Arc<Mutex<SessionManager>>,
    adapters: Arc<Mutex<HashMap<String, Arc<Mutex<dyn PlatformAdapter>>>>>,
    claude_caller: Arc<dyn ClaudeCaller>,
    history_writer: Arc<HistoryWriter>,
    consumer_tasks: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
    config: StubbertConfig,
    transcriber: Option<Arc<dyn Transcriber>>,
    skill_registry: Arc<SkillRegistry>,
    heartbeat_trigger: Option<Arc<dyn HeartbeatTrigger>>,
    start_time: Option<Instant>,
) {
    // Check for slash commands — dispatch immediately, bypass queue
    if let Some(text) = &msg.text {
        if let Some((name, args)) = parse_command(text) {
            let adapter = {
                let adapters = adapters.lock().await;
                match adapters.get(&msg.platform) {
                    Some(a) => a.clone(),
                    None => {
                        tracing::warn!(platform = %msg.platform, "no adapter for command");
                        return;
                    }
                }
            };
            dispatch_command(
                name,
                args,
                &msg,
                adapter,
                session_manager,
                claude_caller,
                history_writer,
                skill_registry,
                config,
                start_time,
                heartbeat_trigger,
            )
            .await;
            return;
        }
        // Unrecognized slash command — ignore
        if text.starts_with('/') {
            return;
        }
    }

    // Build prompt
    let prompt = match build_prompt(&msg, transcriber.as_deref()).await {
        Some(p) => p,
        None => return,
    };

    // Get adapter for this platform
    let adapter = {
        let adapters = adapters.lock().await;
        match adapters.get(&msg.platform) {
            Some(a) => a.clone(),
            None => {
                tracing::warn!(
                    platform = %msg.platform,
                    "no adapter registered for platform"
                );
                return;
            }
        }
    };

    let session_key = SessionManager::conversation_key(&msg.platform, &msg.chat_id);

    // Enqueue message to session
    {
        let mut sm = session_manager.lock().await;
        sm.get_or_create(&msg.platform, &msg.chat_id);
        let session = sm.get_mut(&session_key).unwrap();
        session.enqueue(prompt);
    }

    // Ensure consumer task is running
    {
        let mut tasks = consumer_tasks.lock().await;
        if !tasks.contains_key(&session_key) {
            let mut sm = session_manager.lock().await;
            if let Some(session) = sm.get_mut(&session_key) {
                if let Some(rx) = session.take_rx() {
                    let handle = tokio::spawn(consume_queue(
                        session_key.clone(),
                        rx,
                        msg.platform.clone(),
                        msg.chat_id.clone(),
                        adapter,
                        session_manager.clone(),
                        claude_caller.clone(),
                        history_writer.clone(),
                        consumer_tasks.clone(),
                        config,
                    ));
                    tasks.insert(session_key, handle);
                } else {
                    tracing::warn!(
                        session_key = %session_key,
                        "could not take message receiver for consumer"
                    );
                }
            }
        }
    }
}

// ---- Gateway Struct ----

pub struct Gateway {
    config: StubbertConfig,
    session_manager: Arc<Mutex<SessionManager>>,
    history_writer: Arc<HistoryWriter>,
    transcriber: Option<Arc<dyn Transcriber>>,
    claude_caller: Arc<dyn ClaudeCaller>,
    submitted_files_dir: PathBuf,
    adapters: Arc<Mutex<HashMap<String, Arc<Mutex<dyn PlatformAdapter>>>>>,
    consumer_tasks: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
    start_time: Option<Instant>,
    running: Arc<AtomicBool>,
    skill_registry: Arc<SkillRegistry>,
    heartbeat_trigger: Option<Arc<dyn HeartbeatTrigger>>,
    scheduler: Option<Arc<TaskScheduler>>,
    health_server: Option<super::health::HealthServer>,
}

impl Gateway {
    pub fn new(
        config: StubbertConfig,
        session_manager: SessionManager,
        history_writer: HistoryWriter,
        claude_caller: Arc<dyn ClaudeCaller>,
        transcriber: Option<Arc<dyn Transcriber>>,
        skill_registry: SkillRegistry,
        heartbeat_trigger: Option<Arc<dyn HeartbeatTrigger>>,
    ) -> Self {
        let submitted_files_dir =
            PathBuf::from(&config.claude.working_directory).join("submitted-files");
        Self {
            config,
            session_manager: Arc::new(Mutex::new(session_manager)),
            history_writer: Arc::new(history_writer),
            transcriber,
            claude_caller,
            submitted_files_dir,
            adapters: Arc::new(Mutex::new(HashMap::new())),
            consumer_tasks: Arc::new(Mutex::new(HashMap::new())),
            start_time: None,
            running: Arc::new(AtomicBool::new(false)),
            skill_registry: Arc::new(skill_registry),
            heartbeat_trigger,
            scheduler: None,
            health_server: None,
        }
    }

    pub async fn register_adapter(
        &self,
        platform: &str,
        adapter: impl PlatformAdapter + 'static,
    ) {
        let mut adapters = self.adapters.lock().await;
        adapters.insert(platform.to_string(), Arc::new(Mutex::new(adapter)));
    }

    pub async fn start(&mut self) {
        // Record start time before handlers are created (they capture it)
        self.start_time = Some(Instant::now());

        // Discover skills
        {
            let registry = Arc::get_mut(&mut self.skill_registry)
                .expect("skill_registry not yet shared");
            registry.discover();
        }

        // Load sessions (ignore missing file)
        {
            let mut sm = self.session_manager.lock().await;
            if let Err(e) = sm.load() {
                tracing::info!(
                    error = %e,
                    "no existing sessions loaded (expected on first run)"
                );
            }
        }

        // Clean up old submitted files
        let cleanup_days = self
            .config
            .files
            .as_ref()
            .map(|f| f.cleanup_days)
            .unwrap_or(FILES_CLEANUP_DAYS);
        cleanup_old_files(&self.submitted_files_dir, cleanup_days);

        // Set message handler on all adapters and start them
        {
            let adapters = self.adapters.lock().await;
            for (platform, adapter) in adapters.iter() {
                let handler = self.make_message_handler();
                let mut adapter = adapter.lock().await;
                adapter.set_message_handler(handler);
                if let Err(e) = adapter.start().await {
                    tracing::error!(
                        platform = %platform,
                        error = %e,
                        "failed to start adapter"
                    );
                }
            }
        }

        // Start scheduler if configured
        if let Some(sched_config) = &self.config.scheduler {
            let schedules_path = PathBuf::from(&self.config.claude.working_directory)
                .join(&sched_config.schedules_file);
            match load_schedules(&schedules_path) {
                Ok(tasks) if !tasks.is_empty() => {
                    match TaskScheduler::new(
                        tasks,
                        sched_config,
                        &self.config.claude,
                        Arc::clone(&self.claude_caller),
                        Arc::clone(&self.adapters),
                    ) {
                        Ok(scheduler) => {
                            let scheduler = Arc::new(scheduler);
                            scheduler.start();
                            self.scheduler = Some(scheduler);
                            tracing::info!("scheduler started");
                        }
                        Err(e) => tracing::error!(error = %e, "failed to create scheduler"),
                    }
                }
                Ok(_) => tracing::info!("no scheduled tasks configured"),
                Err(e) => tracing::warn!(error = %e, "failed to load schedules"),
            }
        }

        // Start health server
        {
            let state = super::health::HealthState {
                start_time: self.start_time.unwrap().into_std(),
                session_manager: Arc::clone(&self.session_manager),
                heartbeat_trigger: self.heartbeat_trigger.clone(),
                scheduler: self.scheduler.clone(),
            };
            let mut server = super::health::HealthServer::new();
            let port = server.start(self.config.health.port, state).await;
            self.health_server = Some(server);
            tracing::info!(port = port, "health server started");
        }

        // Post restart greeting
        let working_dir = PathBuf::from(&self.config.claude.working_directory);
        handle_restart_greeting(&working_dir, &self.adapters, &self.claude_caller, &self.config)
            .await;

        // Mark as running
        self.running.store(true, Ordering::SeqCst);
    }

    pub async fn shutdown(&mut self) {
        self.running.store(false, Ordering::SeqCst);

        // Stop scheduler
        if let Some(scheduler) = &self.scheduler {
            scheduler.stop();
        }

        // Send restart message to currently processing sessions
        {
            let sm = self.session_manager.lock().await;
            let adapters = self.adapters.lock().await;
            let processing = sm.processing_sessions();
            for (platform, chat_id) in processing {
                if let Some(adapter) = adapters.get(&platform) {
                    let adapter = adapter.lock().await;
                    if let Err(e) = adapter.send_message(&chat_id, RESTART_MESSAGE).await {
                        tracing::warn!(error = %e, "failed to send restart message");
                    }
                }
            }
        }

        // Stop all adapters
        {
            let adapters = self.adapters.lock().await;
            for (platform, adapter) in adapters.iter() {
                let mut adapter = adapter.lock().await;
                if let Err(e) = adapter.stop().await {
                    tracing::warn!(
                        platform = %platform,
                        error = %e,
                        "failed to stop adapter"
                    );
                }
            }
        }

        // Stop health server
        if let Some(mut server) = self.health_server.take() {
            server.stop();
        }

        // Cancel and await all consumer tasks
        {
            let mut tasks = self.consumer_tasks.lock().await;
            for (_, handle) in tasks.drain() {
                handle.abort();
                let _ = handle.await;
            }
        }
    }

    pub fn start_time(&self) -> Option<Instant> {
        self.start_time
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub async fn active_session_count(&self) -> usize {
        let sm = self.session_manager.lock().await;
        sm.active_session_count()
    }

    fn make_message_handler(&self) -> MessageHandler {
        let sm = self.session_manager.clone();
        let adapters = self.adapters.clone();
        let cc = self.claude_caller.clone();
        let hw = self.history_writer.clone();
        let tasks = self.consumer_tasks.clone();
        let config = self.config.clone();
        let transcriber = self.transcriber.clone();
        let sr = self.skill_registry.clone();
        let ht = self.heartbeat_trigger.clone();
        let start_time = self.start_time;

        Arc::new(move |msg| {
            let sm = sm.clone();
            let adapters = adapters.clone();
            let cc = cc.clone();
            let hw = hw.clone();
            let tasks = tasks.clone();
            let config = config.clone();
            let transcriber = transcriber.clone();
            let sr = sr.clone();
            let ht = ht.clone();

            Box::pin(async move {
                handle_message(
                    msg, sm, adapters, cc, hw, tasks, config, transcriber, sr, ht, start_time,
                )
                .await;
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::MockPlatformAdapter;
    use crate::config::types::*;
    use crate::gateway::skills::SkillRegistry;
    use std::sync::atomic::AtomicU32;
    use tempfile::TempDir;

    // ---- Test Helpers ----

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
            health: HealthConfig { port: 0 },
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

    fn make_incoming_empty(platform: &str, chat_id: &str) -> IncomingMessage {
        IncomingMessage {
            platform: platform.to_string(),
            user_id: "user1".to_string(),
            chat_id: chat_id.to_string(),
            text: None,
            image_paths: vec![],
            audio_paths: vec![],
            file_paths: vec![],
            file_names: vec![],
        }
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

    fn claude_timeout(secs: u64) -> Result<ClaudeResponse, ClaudeError> {
        Err(ClaudeError::Timeout { timeout_secs: secs })
    }

    fn make_session_manager(dir: &Path) -> SessionManager {
        SessionManager::new(
            dir.join("sessions.json"),
            60,
            "claude-sonnet-4-6".to_string(),
        )
    }

    fn mock_adapter_success() -> MockPlatformAdapter {
        let mut mock = MockPlatformAdapter::new();
        mock.expect_send_message().returning(|_, _| Ok(()));
        mock.expect_send_typing().returning(|_| Ok(()));
        mock
    }

    /// Wait for all consumer tasks to complete (for test synchronization)
    async fn wait_for_consumers(
        consumer_tasks: &Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
    ) {
        for _ in 0..200 {
            {
                let tasks = consumer_tasks.lock().await;
                if tasks.is_empty() {
                    return;
                }
            }
            tokio::task::yield_now().await;
        }
        panic!("consumer tasks did not complete in time");
    }

    // ---- build_prompt tests ----

    mod test_build_prompt {
        use super::*;

        #[tokio::test]
        async fn text_only() {
            let msg = make_incoming("telegram", "123", "hello world");
            let result = build_prompt(&msg, None).await;
            assert_eq!(result, Some("hello world".to_string()));
        }

        #[tokio::test]
        async fn text_with_files() {
            let mut msg = make_incoming("telegram", "123", "check this");
            msg.file_paths = vec![PathBuf::from("/tmp/doc.pdf")];
            msg.file_names = vec!["doc.pdf".to_string()];
            let result = build_prompt(&msg, None).await.unwrap();
            assert!(result.contains("check this"));
            assert!(result.contains("`doc.pdf`: /tmp/doc.pdf"));
        }

        #[tokio::test]
        async fn text_with_images() {
            let mut msg = make_incoming("telegram", "123", "what is this");
            msg.image_paths = vec![PathBuf::from("/tmp/photo.jpg")];
            let result = build_prompt(&msg, None).await.unwrap();
            assert!(result.contains("what is this"));
            assert!(result.contains("/tmp/photo.jpg"));
        }

        #[tokio::test]
        async fn audio_with_transcriber() {
            let mut msg = make_incoming_empty("telegram", "123");
            msg.audio_paths = vec![PathBuf::from("/tmp/voice.ogg")];

            let mut mock_transcriber = MockTranscriber::new();
            mock_transcriber
                .expect_transcribe()
                .returning(|_| Ok("hello from voice".to_string()));

            let result = build_prompt(&msg, Some(&mock_transcriber)).await.unwrap();
            assert_eq!(result, "[Voice transcription]: hello from voice");
        }

        #[tokio::test]
        async fn audio_with_text_caption() {
            let mut msg = make_incoming("telegram", "123", "caption text");
            msg.audio_paths = vec![PathBuf::from("/tmp/voice.ogg")];

            let mut mock_transcriber = MockTranscriber::new();
            mock_transcriber
                .expect_transcribe()
                .returning(|_| Ok("transcribed words".to_string()));

            let result = build_prompt(&msg, Some(&mock_transcriber)).await.unwrap();
            assert!(result.contains("[Voice transcription]: transcribed words"));
            assert!(result.contains("caption text"));
        }

        #[tokio::test]
        async fn audio_without_transcriber() {
            let mut msg = make_incoming("telegram", "123", "some text");
            msg.audio_paths = vec![PathBuf::from("/tmp/voice.ogg")];
            let result = build_prompt(&msg, None).await.unwrap();
            // Audio skipped, only text returned
            assert_eq!(result, "some text");
        }

        #[tokio::test]
        async fn transcription_failure_skips_audio() {
            let mut msg = make_incoming("telegram", "123", "some text");
            msg.audio_paths = vec![PathBuf::from("/tmp/voice.ogg")];

            let mut mock_transcriber = MockTranscriber::new();
            mock_transcriber
                .expect_transcribe()
                .returning(|_| Err("whisper error".to_string()));

            let result = build_prompt(&msg, Some(&mock_transcriber)).await.unwrap();
            assert_eq!(result, "some text");
        }

        #[tokio::test]
        async fn files_use_file_names() {
            let mut msg = make_incoming_empty("telegram", "123");
            msg.file_paths = vec![PathBuf::from("/tmp/submitted-files/abc123")];
            msg.file_names = vec!["my-document.txt".to_string()];
            let result = build_prompt(&msg, None).await.unwrap();
            assert!(result.contains("`my-document.txt`"));
        }

        #[tokio::test]
        async fn empty_message_returns_none() {
            let msg = make_incoming_empty("telegram", "123");
            let result = build_prompt(&msg, None).await;
            assert!(result.is_none());
        }

        #[tokio::test]
        async fn empty_text_returns_none() {
            let mut msg = make_incoming_empty("telegram", "123");
            msg.text = Some("".to_string());
            let result = build_prompt(&msg, None).await;
            assert!(result.is_none());
        }

        #[tokio::test]
        async fn multiple_files_and_images() {
            let mut msg = make_incoming("telegram", "123", "look");
            msg.file_paths = vec![
                PathBuf::from("/tmp/a.txt"),
                PathBuf::from("/tmp/b.txt"),
            ];
            msg.file_names = vec!["a.txt".to_string(), "b.txt".to_string()];
            msg.image_paths = vec![PathBuf::from("/tmp/img.png")];
            let result = build_prompt(&msg, None).await.unwrap();
            assert!(result.contains("`a.txt`: /tmp/a.txt"));
            assert!(result.contains("`b.txt`: /tmp/b.txt"));
            assert!(result.contains("/tmp/img.png"));
        }
    }

    // ---- cleanup_old_files tests ----

    mod test_cleanup_old_files {
        use super::*;
        use std::fs;
        use std::time::SystemTime;

        fn set_file_old(path: &Path) {
            let old_time = filetime::FileTime::from_system_time(
                SystemTime::now() - Duration::from_secs(31 * 86400),
            );
            filetime::set_file_mtime(path, old_time).unwrap();
        }

        #[test]
        fn old_file_deleted() {
            let dir = TempDir::new().unwrap();
            let file = dir.path().join("old.txt");
            fs::write(&file, "data").unwrap();
            set_file_old(&file);

            cleanup_old_files(dir.path(), 30);
            assert!(!file.exists());
        }

        #[test]
        fn recent_file_preserved() {
            let dir = TempDir::new().unwrap();
            let file = dir.path().join("recent.txt");
            fs::write(&file, "data").unwrap();

            cleanup_old_files(dir.path(), 30);
            assert!(file.exists());
        }

        #[test]
        fn empty_dir_removed_after_files_deleted() {
            let dir = TempDir::new().unwrap();
            let subdir = dir.path().join("subdir");
            fs::create_dir(&subdir).unwrap();
            let file = subdir.join("old.txt");
            fs::write(&file, "data").unwrap();
            set_file_old(&file);

            cleanup_old_files(dir.path(), 30);
            assert!(!file.exists());
            assert!(!subdir.exists());
        }

        #[test]
        fn non_empty_dir_preserved() {
            let dir = TempDir::new().unwrap();
            let subdir = dir.path().join("subdir");
            fs::create_dir(&subdir).unwrap();
            let file = subdir.join("recent.txt");
            fs::write(&file, "data").unwrap();

            cleanup_old_files(dir.path(), 30);
            assert!(subdir.exists());
            assert!(file.exists());
        }

        #[test]
        fn missing_dir_no_panic() {
            let dir = TempDir::new().unwrap();
            let nonexistent = dir.path().join("nope");
            cleanup_old_files(&nonexistent, 30); // Should not panic
        }

        #[test]
        fn nested_dirs_handled() {
            let dir = TempDir::new().unwrap();
            let nested = dir.path().join("a").join("b");
            fs::create_dir_all(&nested).unwrap();
            let file = nested.join("old.txt");
            fs::write(&file, "data").unwrap();
            set_file_old(&file);

            cleanup_old_files(dir.path(), 30);
            assert!(!file.exists());
            assert!(!nested.exists());
            assert!(!dir.path().join("a").exists());
        }
    }

    // ---- typing_loop tests ----

    mod test_typing_loop {
        use super::*;

        #[tokio::test(start_paused = true)]
        async fn sends_typing_at_intervals() {
            let typing_count = Arc::new(AtomicU32::new(0));
            let tc = typing_count.clone();

            let mut mock = MockPlatformAdapter::new();
            mock.expect_send_typing().returning(move |_| {
                tc.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });

            let adapter: Arc<Mutex<dyn PlatformAdapter>> = Arc::new(Mutex::new(mock));
            let handle = tokio::spawn(typing_loop(adapter, "c1".to_string()));

            // Let the task run - initial typing
            tokio::task::yield_now().await;
            assert_eq!(typing_count.load(Ordering::SeqCst), 1);

            // After 5 seconds
            tokio::time::advance(Duration::from_secs(5)).await;
            tokio::task::yield_now().await;
            assert_eq!(typing_count.load(Ordering::SeqCst), 2);

            // After another 5 seconds
            tokio::time::advance(Duration::from_secs(5)).await;
            tokio::task::yield_now().await;
            assert_eq!(typing_count.load(Ordering::SeqCst), 3);

            handle.abort();
            let _ = handle.await;
        }

        #[tokio::test(start_paused = true)]
        async fn stops_when_aborted() {
            let mut mock = MockPlatformAdapter::new();
            mock.expect_send_typing().returning(|_| Ok(()));

            let adapter: Arc<Mutex<dyn PlatformAdapter>> = Arc::new(Mutex::new(mock));
            let handle = tokio::spawn(typing_loop(adapter, "c1".to_string()));

            tokio::task::yield_now().await;
            handle.abort();
            let result = handle.await;
            assert!(result.unwrap_err().is_cancelled());
        }

        #[tokio::test(start_paused = true)]
        async fn failure_continues_loop() {
            let typing_count = Arc::new(AtomicU32::new(0));
            let tc = typing_count.clone();

            let mut mock = MockPlatformAdapter::new();
            mock.expect_send_typing().returning(move |_| {
                tc.fetch_add(1, Ordering::SeqCst);
                Err(crate::adapters::AdapterError::SendFailed(
                    "network error".to_string(),
                ))
            });

            let adapter: Arc<Mutex<dyn PlatformAdapter>> = Arc::new(Mutex::new(mock));
            let handle = tokio::spawn(typing_loop(adapter, "c1".to_string()));

            tokio::task::yield_now().await;
            assert_eq!(typing_count.load(Ordering::SeqCst), 1);

            // Loop continues despite failure
            tokio::time::advance(Duration::from_secs(5)).await;
            tokio::task::yield_now().await;
            assert_eq!(typing_count.load(Ordering::SeqCst), 2);

            handle.abort();
            let _ = handle.await;
        }
    }

    // ---- handle_restart_greeting tests ----

    mod test_restart_greeting {
        use super::*;
        use std::fs;

        fn setup_adapters(
            mock: MockPlatformAdapter,
        ) -> Arc<Mutex<HashMap<String, Arc<Mutex<dyn PlatformAdapter>>>>> {
            let mut map: HashMap<String, Arc<Mutex<dyn PlatformAdapter>>> = HashMap::new();
            map.insert("telegram".to_string(), Arc::new(Mutex::new(mock)));
            Arc::new(Mutex::new(map))
        }

        #[tokio::test]
        async fn file_exists_sends_greeting_and_deletes() {
            let dir = TempDir::new().unwrap();
            let origin = serde_json::json!({"platform": "telegram", "chat_id": "123"});
            fs::write(
                dir.path().join(RESTART_ORIGIN_FILE),
                origin.to_string(),
            )
            .unwrap();

            let sent_messages = Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
            let sm = sent_messages.clone();
            let mut mock = MockPlatformAdapter::new();
            mock.expect_send_message().returning(move |chat_id, text| {
                sent_messages.lock().unwrap().push((chat_id.to_string(), text.to_string()));
                Ok(())
            });

            let adapters = setup_adapters(mock);
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude
                .expect_call()
                .returning(|_| claude_success("I'm back!"));
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);
            let config = make_config();

            handle_restart_greeting(dir.path(), &adapters, &cc, &config).await;

            let msgs = sm.lock().unwrap();
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].0, "123");
            assert_eq!(msgs[0].1, "I'm back!");
            assert!(!dir.path().join(RESTART_ORIGIN_FILE).exists());
        }

        #[tokio::test]
        async fn file_not_exists_no_op() {
            let dir = TempDir::new().unwrap();
            let adapters = Arc::new(Mutex::new(HashMap::new()));
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude.expect_call().never();
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);
            let config = make_config();

            handle_restart_greeting(dir.path(), &adapters, &cc, &config).await;
            // No panic, no calls
        }

        #[tokio::test]
        async fn invalid_json_deletes_file() {
            let dir = TempDir::new().unwrap();
            fs::write(
                dir.path().join(RESTART_ORIGIN_FILE),
                "not valid json",
            )
            .unwrap();

            let adapters = Arc::new(Mutex::new(HashMap::new()));
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude.expect_call().never();
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);
            let config = make_config();

            handle_restart_greeting(dir.path(), &adapters, &cc, &config).await;
            assert!(!dir.path().join(RESTART_ORIGIN_FILE).exists());
        }

        #[tokio::test]
        async fn claude_fails_deletes_file() {
            let dir = TempDir::new().unwrap();
            let origin = serde_json::json!({"platform": "telegram", "chat_id": "123"});
            fs::write(
                dir.path().join(RESTART_ORIGIN_FILE),
                origin.to_string(),
            )
            .unwrap();

            let mut mock = MockPlatformAdapter::new();
            mock.expect_send_message().never();
            let adapters = setup_adapters(mock);

            let mut mock_claude = MockClaudeCaller::new();
            mock_claude
                .expect_call()
                .returning(|_| claude_exit_error());
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);
            let config = make_config();

            handle_restart_greeting(dir.path(), &adapters, &cc, &config).await;
            assert!(!dir.path().join(RESTART_ORIGIN_FILE).exists());
        }

        #[tokio::test]
        async fn unknown_platform_deletes_file() {
            let dir = TempDir::new().unwrap();
            let origin = serde_json::json!({"platform": "slack", "chat_id": "123"});
            fs::write(
                dir.path().join(RESTART_ORIGIN_FILE),
                origin.to_string(),
            )
            .unwrap();

            let adapters = Arc::new(Mutex::new(HashMap::new()));
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude.expect_call().never();
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);
            let config = make_config();

            handle_restart_greeting(dir.path(), &adapters, &cc, &config).await;
            assert!(!dir.path().join(RESTART_ORIGIN_FILE).exists());
        }
    }

    // ---- process_prompt tests ----

    mod test_process_prompt {
        use super::*;

        struct ProcessPromptSetup {
            session_key: String,
            adapter: Arc<Mutex<dyn PlatformAdapter>>,
            session_manager: Arc<Mutex<SessionManager>>,
            history_writer: Arc<HistoryWriter>,
            config: StubbertConfig,
            _dir: TempDir,
        }

        fn setup(platform: &str, chat_id: &str) -> ProcessPromptSetup {
            let dir = TempDir::new().unwrap();
            let mut sm = make_session_manager(dir.path());
            sm.get_or_create(platform, chat_id);
            let session_key = SessionManager::conversation_key(platform, chat_id);
            let hw = HistoryWriter::new(dir.path().join("history"));

            let mock = mock_adapter_success();
            let adapter: Arc<Mutex<dyn PlatformAdapter>> = Arc::new(Mutex::new(mock));

            ProcessPromptSetup {
                session_key,
                adapter,
                session_manager: Arc::new(Mutex::new(sm)),
                history_writer: Arc::new(hw),
                config: make_config(),
                _dir: dir,
            }
        }

        #[tokio::test]
        async fn success_marks_initiated() {
            let s = setup("telegram", "123");
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude
                .expect_call()
                .returning(|_| claude_success("Hello!"));
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);

            process_prompt(
                &s.session_key, "hi", "telegram", "123", s.adapter, s.session_manager.clone(),
                cc, s.history_writer, &s.config,
            )
            .await;

            let sm = s.session_manager.lock().await;
            let session = sm.get(&s.session_key).unwrap();
            assert!(session.initiated);
        }

        #[tokio::test]
        async fn success_saves_sessions() {
            let s = setup("telegram", "123");
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude
                .expect_call()
                .returning(|_| claude_success("Hello!"));
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);

            process_prompt(
                &s.session_key, "hi", "telegram", "123", s.adapter, s.session_manager.clone(),
                cc, s.history_writer, &s.config,
            )
            .await;

            // sessions.json should exist after save
            assert!(s._dir.path().join("sessions.json").exists());
        }

        #[tokio::test]
        async fn success_writes_history() {
            let s = setup("telegram", "123");
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude
                .expect_call()
                .returning(|_| claude_success("Hello!"));
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);

            process_prompt(
                &s.session_key, "user prompt", "telegram", "123", s.adapter,
                s.session_manager.clone(), cc, s.history_writer.clone(), &s.config,
            )
            .await;

            // Check history file was written
            let history_dir = s._dir.path().join("history");
            let entries: Vec<_> = std::fs::read_dir(&history_dir)
                .unwrap()
                .flatten()
                .collect();
            assert_eq!(entries.len(), 1);
            let content = std::fs::read_to_string(entries[0].path()).unwrap();
            assert!(content.contains("user: user prompt"));
            assert!(content.contains("assistant: Hello!"));
        }

        #[tokio::test]
        async fn success_sends_response() {
            let dir = TempDir::new().unwrap();
            let mut sm = make_session_manager(dir.path());
            sm.get_or_create("telegram", "123");
            let session_key = SessionManager::conversation_key("telegram", "123");
            let hw = Arc::new(HistoryWriter::new(dir.path().join("history")));

            let sent = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let sent_clone = sent.clone();
            let mut mock = MockPlatformAdapter::new();
            mock.expect_send_message()
                .returning(move |_, text| {
                    sent_clone.lock().unwrap().push(text.to_string());
                    Ok(())
                });
            mock.expect_send_typing().returning(|_| Ok(()));
            let adapter: Arc<Mutex<dyn PlatformAdapter>> = Arc::new(Mutex::new(mock));

            let mut mock_claude = MockClaudeCaller::new();
            mock_claude
                .expect_call()
                .returning(|_| claude_success("Bot response"));
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);

            process_prompt(
                &session_key, "hi", "telegram", "123", adapter,
                Arc::new(Mutex::new(sm)), cc, hw, &make_config(),
            )
            .await;

            let sent = sent.lock().unwrap();
            assert!(sent.contains(&"Bot response".to_string()));
        }

        #[tokio::test]
        async fn resume_failure_sends_failure_message_and_retries() {
            let dir = TempDir::new().unwrap();
            let mut sm = make_session_manager(dir.path());
            sm.get_or_create("telegram", "123");
            let session_key = SessionManager::conversation_key("telegram", "123");
            // Mark as initiated so resume failure path triggers
            sm.get_mut(&session_key).unwrap().mark_initiated();

            let hw = Arc::new(HistoryWriter::new(dir.path().join("history")));

            let sent = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let sent_clone = sent.clone();
            let mut mock = MockPlatformAdapter::new();
            mock.expect_send_message()
                .returning(move |_, text| {
                    sent_clone.lock().unwrap().push(text.to_string());
                    Ok(())
                });
            mock.expect_send_typing().returning(|_| Ok(()));
            let adapter: Arc<Mutex<dyn PlatformAdapter>> = Arc::new(Mutex::new(mock));

            let call_count = Arc::new(AtomicU32::new(0));
            let cc_count = call_count.clone();
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude.expect_call().returning(move |_| {
                let n = cc_count.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    claude_exit_error()
                } else {
                    claude_success("Retry worked!")
                }
            });
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);

            let session_manager = Arc::new(Mutex::new(sm));
            process_prompt(
                &session_key, "hi", "telegram", "123", adapter,
                session_manager.clone(), cc, hw, &make_config(),
            )
            .await;

            // Should have sent failure message + retry response
            let sent = sent.lock().unwrap();
            assert!(sent.contains(&SESSION_FAILURE_MESSAGE.to_string()));
            assert!(sent.contains(&"Retry worked!".to_string()));
            assert_eq!(call_count.load(Ordering::SeqCst), 2);

            // Session should be re-initiated after retry
            let sm = session_manager.lock().await;
            let session = sm.get(&session_key).unwrap();
            assert!(session.initiated);
        }

        #[tokio::test]
        async fn resume_failure_resets_session() {
            let dir = TempDir::new().unwrap();
            let mut sm = make_session_manager(dir.path());
            sm.get_or_create("telegram", "123");
            let session_key = SessionManager::conversation_key("telegram", "123");
            sm.get_mut(&session_key).unwrap().mark_initiated();
            let original_uuid = sm.get(&session_key).unwrap().session_id;

            let hw = Arc::new(HistoryWriter::new(dir.path().join("history")));
            let mock = mock_adapter_success();
            let adapter: Arc<Mutex<dyn PlatformAdapter>> = Arc::new(Mutex::new(mock));

            let mut mock_claude = MockClaudeCaller::new();
            let call_count = Arc::new(AtomicU32::new(0));
            let cc = call_count.clone();
            mock_claude.expect_call().returning(move |_| {
                let n = cc.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    claude_exit_error()
                } else {
                    claude_success("ok")
                }
            });
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);

            let session_manager = Arc::new(Mutex::new(sm));
            process_prompt(
                &session_key, "hi", "telegram", "123", adapter,
                session_manager.clone(), cc, hw, &make_config(),
            )
            .await;

            let sm = session_manager.lock().await;
            let session = sm.get(&session_key).unwrap();
            assert_ne!(session.session_id, original_uuid);
        }

        #[tokio::test]
        async fn timeout_sends_timeout_message() {
            let dir = TempDir::new().unwrap();
            let mut sm = make_session_manager(dir.path());
            sm.get_or_create("telegram", "123");
            let session_key = SessionManager::conversation_key("telegram", "123");
            let hw = Arc::new(HistoryWriter::new(dir.path().join("history")));

            let sent = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let sent_clone = sent.clone();
            let mut mock = MockPlatformAdapter::new();
            mock.expect_send_message()
                .returning(move |_, text| {
                    sent_clone.lock().unwrap().push(text.to_string());
                    Ok(())
                });
            mock.expect_send_typing().returning(|_| Ok(()));
            let adapter: Arc<Mutex<dyn PlatformAdapter>> = Arc::new(Mutex::new(mock));

            let mut mock_claude = MockClaudeCaller::new();
            mock_claude
                .expect_call()
                .returning(|_| claude_timeout(300));
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);

            process_prompt(
                &session_key, "hi", "telegram", "123", adapter,
                Arc::new(Mutex::new(sm)), cc, hw, &make_config(),
            )
            .await;

            let sent = sent.lock().unwrap();
            assert!(sent.iter().any(|m| m.contains("timed out after 300s")));
        }

        #[tokio::test]
        async fn generic_error_sends_error_message() {
            let dir = TempDir::new().unwrap();
            let mut sm = make_session_manager(dir.path());
            sm.get_or_create("telegram", "123");
            let session_key = SessionManager::conversation_key("telegram", "123");
            let hw = Arc::new(HistoryWriter::new(dir.path().join("history")));

            let sent = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let sent_clone = sent.clone();
            let mut mock = MockPlatformAdapter::new();
            mock.expect_send_message()
                .returning(move |_, text| {
                    sent_clone.lock().unwrap().push(text.to_string());
                    Ok(())
                });
            mock.expect_send_typing().returning(|_| Ok(()));
            let adapter: Arc<Mutex<dyn PlatformAdapter>> = Arc::new(Mutex::new(mock));

            let mut mock_claude = MockClaudeCaller::new();
            mock_claude
                .expect_call()
                .returning(|_| Err(ClaudeError::CliFailure("bad".to_string())));
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);

            process_prompt(
                &session_key, "hi", "telegram", "123", adapter,
                Arc::new(Mutex::new(sm)), cc, hw, &make_config(),
            )
            .await;

            let sent = sent.lock().unwrap();
            assert!(sent.contains(&ERROR_MESSAGE.to_string()));
        }

        #[tokio::test]
        async fn exit_error_without_initiated_sends_error_message() {
            let dir = TempDir::new().unwrap();
            let mut sm = make_session_manager(dir.path());
            sm.get_or_create("telegram", "123");
            let session_key = SessionManager::conversation_key("telegram", "123");
            // NOT marking as initiated — exit error should not trigger retry
            let hw = Arc::new(HistoryWriter::new(dir.path().join("history")));

            let sent = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let sent_clone = sent.clone();
            let mut mock = MockPlatformAdapter::new();
            mock.expect_send_message()
                .returning(move |_, text| {
                    sent_clone.lock().unwrap().push(text.to_string());
                    Ok(())
                });
            mock.expect_send_typing().returning(|_| Ok(()));
            let adapter: Arc<Mutex<dyn PlatformAdapter>> = Arc::new(Mutex::new(mock));

            let mut mock_claude = MockClaudeCaller::new();
            mock_claude.expect_call().times(1).returning(|_| claude_exit_error());
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);

            process_prompt(
                &session_key, "hi", "telegram", "123", adapter,
                Arc::new(Mutex::new(sm)), cc, hw, &make_config(),
            )
            .await;

            let sent = sent.lock().unwrap();
            assert!(sent.contains(&ERROR_MESSAGE.to_string()));
            assert!(!sent.contains(&SESSION_FAILURE_MESSAGE.to_string()));
        }

        #[tokio::test]
        async fn adapter_send_failure_doesnt_crash() {
            let s = setup("telegram", "123");
            // Replace adapter with one that fails on send
            let mut mock = MockPlatformAdapter::new();
            mock.expect_send_message().returning(|_, _| {
                Err(crate::adapters::AdapterError::SendFailed(
                    "network".to_string(),
                ))
            });
            mock.expect_send_typing().returning(|_| Ok(()));
            let adapter: Arc<Mutex<dyn PlatformAdapter>> = Arc::new(Mutex::new(mock));

            let mut mock_claude = MockClaudeCaller::new();
            mock_claude
                .expect_call()
                .returning(|_| claude_success("Hello!"));
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);

            // Should not panic
            process_prompt(
                &s.session_key, "hi", "telegram", "123", adapter, s.session_manager,
                cc, s.history_writer, &s.config,
            )
            .await;
        }
    }

    // ---- consume_queue tests ----

    mod test_consume_queue {
        use super::*;

        #[tokio::test]
        async fn single_message_processed_and_exits() {
            let dir = TempDir::new().unwrap();
            let mut sm = make_session_manager(dir.path());
            sm.get_or_create("telegram", "123");
            let session_key = SessionManager::conversation_key("telegram", "123");
            sm.get_mut(&session_key).unwrap().enqueue("hello".to_string());
            let rx = sm.get_mut(&session_key).unwrap().take_rx().unwrap();

            let session_manager = Arc::new(Mutex::new(sm));
            let consumer_tasks: Arc<Mutex<HashMap<String, JoinHandle<()>>>> =
                Arc::new(Mutex::new(HashMap::new()));

            let mock = mock_adapter_success();
            let adapter: Arc<Mutex<dyn PlatformAdapter>> = Arc::new(Mutex::new(mock));

            let mut mock_claude = MockClaudeCaller::new();
            mock_claude
                .expect_call()
                .returning(|_| claude_success("response"));
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);
            let hw = Arc::new(HistoryWriter::new(dir.path().join("history")));

            consume_queue(
                session_key.clone(), rx, "telegram".to_string(), "123".to_string(),
                adapter, session_manager.clone(), cc, hw,
                consumer_tasks.clone(), make_config(),
            )
            .await;

            // Session should not be processing after consumer exits
            let sm = session_manager.lock().await;
            let session = sm.get(&session_key).unwrap();
            assert!(!session.processing);
            assert!(session.initiated);

            // Consumer should have removed itself from tasks
            let tasks = consumer_tasks.lock().await;
            assert!(!tasks.contains_key(&session_key));
        }

        #[tokio::test]
        async fn channel_closed_exits_cleanly() {
            let dir = TempDir::new().unwrap();
            let mut sm = make_session_manager(dir.path());
            sm.get_or_create("telegram", "123");
            let session_key = SessionManager::conversation_key("telegram", "123");
            // Take rx but don't enqueue anything, then drop the session to close channel
            let rx = sm.get_mut(&session_key).unwrap().take_rx().unwrap();

            let session_manager = Arc::new(Mutex::new(sm));
            let consumer_tasks: Arc<Mutex<HashMap<String, JoinHandle<()>>>> =
                Arc::new(Mutex::new(HashMap::new()));

            let mock = mock_adapter_success();
            let adapter: Arc<Mutex<dyn PlatformAdapter>> = Arc::new(Mutex::new(mock));

            let mut mock_claude = MockClaudeCaller::new();
            mock_claude.expect_call().never(); // No calls expected
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);
            let hw = Arc::new(HistoryWriter::new(dir.path().join("history")));

            // Should exit cleanly without processing anything
            consume_queue(
                session_key.clone(), rx, "telegram".to_string(), "123".to_string(),
                adapter, session_manager.clone(), cc, hw,
                consumer_tasks.clone(), make_config(),
            )
            .await;

            let sm = session_manager.lock().await;
            assert!(!sm.get(&session_key).unwrap().processing);
        }

        #[tokio::test]
        async fn sets_processing_true_then_false() {
            let dir = TempDir::new().unwrap();
            let mut sm = make_session_manager(dir.path());
            sm.get_or_create("telegram", "123");
            let session_key = SessionManager::conversation_key("telegram", "123");
            sm.get_mut(&session_key).unwrap().enqueue("hello".to_string());
            let rx = sm.get_mut(&session_key).unwrap().take_rx().unwrap();

            let session_manager = Arc::new(Mutex::new(sm));
            let consumer_tasks = Arc::new(Mutex::new(HashMap::new()));

            let mut mock_claude = MockClaudeCaller::new();
            mock_claude
                .expect_call()
                .returning(|_| claude_success("ok"));
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);

            let mock = mock_adapter_success();
            let adapter: Arc<Mutex<dyn PlatformAdapter>> = Arc::new(Mutex::new(mock));
            let hw = Arc::new(HistoryWriter::new(dir.path().join("history")));

            consume_queue(
                session_key.clone(), rx, "telegram".to_string(), "123".to_string(),
                adapter, session_manager.clone(), cc, hw,
                consumer_tasks, make_config(),
            )
            .await;

            let sm = session_manager.lock().await;
            assert!(!sm.get(&session_key).unwrap().processing);
        }

        #[tokio::test]
        async fn multiple_messages_batched() {
            let dir = TempDir::new().unwrap();
            let mut sm = make_session_manager(dir.path());
            sm.get_or_create("telegram", "123");
            let session_key = SessionManager::conversation_key("telegram", "123");
            sm.get_mut(&session_key)
                .unwrap()
                .enqueue("first".to_string());
            sm.get_mut(&session_key)
                .unwrap()
                .enqueue("second".to_string());
            let rx = sm.get_mut(&session_key).unwrap().take_rx().unwrap();

            let session_manager = Arc::new(Mutex::new(sm));
            let consumer_tasks = Arc::new(Mutex::new(HashMap::new()));

            let prompts_seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let ps = prompts_seen.clone();
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude.expect_call().returning(move |params| {
                ps.lock().unwrap().push(params.prompt.clone());
                claude_success("ok")
            });
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);

            let mock = mock_adapter_success();
            let adapter: Arc<Mutex<dyn PlatformAdapter>> = Arc::new(Mutex::new(mock));
            let hw = Arc::new(HistoryWriter::new(dir.path().join("history")));

            consume_queue(
                session_key.clone(), rx, "telegram".to_string(), "123".to_string(),
                adapter, session_manager, cc, hw,
                consumer_tasks, make_config(),
            )
            .await;

            let prompts = prompts_seen.lock().unwrap();
            assert_eq!(prompts.len(), 1);
            assert!(prompts[0].contains("Batched messages from user:"));
            assert!(prompts[0].contains("first"));
            assert!(prompts[0].contains("second"));
        }
    }

    // ---- handle_message tests ----

    mod test_handle_message {
        use super::*;

        struct HandleMessageSetup {
            session_manager: Arc<Mutex<SessionManager>>,
            adapters: Arc<Mutex<HashMap<String, Arc<Mutex<dyn PlatformAdapter>>>>>,
            consumer_tasks: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
            history_writer: Arc<HistoryWriter>,
            skill_registry: Arc<SkillRegistry>,
            config: StubbertConfig,
            _dir: TempDir,
        }

        fn setup() -> HandleMessageSetup {
            let dir = TempDir::new().unwrap();
            let sm = make_session_manager(dir.path());
            let hw = HistoryWriter::new(dir.path().join("history"));

            let mut mock = MockPlatformAdapter::new();
            mock.expect_send_message().returning(|_, _| Ok(()));
            mock.expect_send_typing().returning(|_| Ok(()));

            let mut adapters_map: HashMap<String, Arc<Mutex<dyn PlatformAdapter>>> =
                HashMap::new();
            adapters_map.insert(
                "telegram".to_string(),
                Arc::new(Mutex::new(mock)),
            );

            let sr = SkillRegistry::new(dir.path().join(".claude").join("skills"));

            HandleMessageSetup {
                session_manager: Arc::new(Mutex::new(sm)),
                adapters: Arc::new(Mutex::new(adapters_map)),
                consumer_tasks: Arc::new(Mutex::new(HashMap::new())),
                history_writer: Arc::new(hw),
                skill_registry: Arc::new(sr),
                config: make_config(),
                _dir: dir,
            }
        }

        #[tokio::test]
        async fn non_command_text_enqueued_and_consumer_spawned() {
            let s = setup();
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude
                .expect_call()
                .returning(|_| claude_success("reply"));
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);

            let msg = make_incoming("telegram", "123", "hello");
            handle_message(
                msg, s.session_manager.clone(), s.adapters, cc, s.history_writer,
                s.consumer_tasks.clone(), s.config, None,
                s.skill_registry, None, None,
            )
            .await;

            // Consumer should have been spawned
            {
                let tasks = s.consumer_tasks.lock().await;
                assert_eq!(tasks.len(), 1);
            }

            // Wait for consumer to finish
            wait_for_consumers(&s.consumer_tasks).await;

            // Session should exist and be initiated
            let sm = s.session_manager.lock().await;
            let key = SessionManager::conversation_key("telegram", "123");
            assert!(sm.get(&key).unwrap().initiated);
        }

        #[tokio::test]
        async fn command_not_enqueued() {
            let s = setup();
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude.expect_call().never();
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);

            let msg = make_incoming("telegram", "123", "/status");
            handle_message(
                msg, s.session_manager.clone(), s.adapters, cc, s.history_writer,
                s.consumer_tasks.clone(), s.config, None,
                s.skill_registry, None, None,
            )
            .await;

            let tasks = s.consumer_tasks.lock().await;
            assert!(tasks.is_empty());
        }

        #[tokio::test]
        async fn empty_prompt_skipped() {
            let s = setup();
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude.expect_call().never();
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);

            let msg = make_incoming_empty("telegram", "123");
            handle_message(
                msg, s.session_manager.clone(), s.adapters, cc, s.history_writer,
                s.consumer_tasks.clone(), s.config, None,
                s.skill_registry, None, None,
            )
            .await;

            let tasks = s.consumer_tasks.lock().await;
            assert!(tasks.is_empty());
        }

        #[tokio::test]
        async fn unknown_platform_logged_not_crashed() {
            let s = setup();
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude.expect_call().never();
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);

            let msg = make_incoming("slack", "123", "hello");
            handle_message(
                msg, s.session_manager.clone(), s.adapters, cc, s.history_writer,
                s.consumer_tasks.clone(), s.config, None,
                s.skill_registry, None, None,
            )
            .await;

            let tasks = s.consumer_tasks.lock().await;
            assert!(tasks.is_empty());
        }

        #[tokio::test]
        async fn same_session_same_consumer() {
            let s = setup();
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude
                .expect_call()
                .returning(|_| claude_success("reply"));
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);

            let msg1 = make_incoming("telegram", "123", "first");
            let msg2 = make_incoming("telegram", "123", "second");

            handle_message(
                msg1, s.session_manager.clone(), s.adapters.clone(), cc.clone(),
                s.history_writer.clone(), s.consumer_tasks.clone(), s.config.clone(), None,
                s.skill_registry.clone(), None, None,
            )
            .await;

            handle_message(
                msg2, s.session_manager.clone(), s.adapters.clone(), cc,
                s.history_writer.clone(), s.consumer_tasks.clone(), s.config.clone(), None,
                s.skill_registry.clone(), None, None,
            )
            .await;

            // Only one consumer should exist
            let tasks = s.consumer_tasks.lock().await;
            assert_eq!(tasks.len(), 1);
        }

        #[tokio::test]
        async fn different_sessions_different_consumers() {
            let s = setup();

            // Add discord adapter too
            let mut discord_mock = MockPlatformAdapter::new();
            discord_mock
                .expect_send_message()
                .returning(|_, _| Ok(()));
            discord_mock
                .expect_send_typing()
                .returning(|_| Ok(()));
            {
                let mut adapters = s.adapters.lock().await;
                adapters.insert(
                    "discord".to_string(),
                    Arc::new(Mutex::new(discord_mock)),
                );
            }

            let mut mock_claude = MockClaudeCaller::new();
            mock_claude
                .expect_call()
                .returning(|_| claude_success("reply"));
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);

            let msg1 = make_incoming("telegram", "123", "hello");
            let msg2 = make_incoming("discord", "456", "hi");

            handle_message(
                msg1, s.session_manager.clone(), s.adapters.clone(), cc.clone(),
                s.history_writer.clone(), s.consumer_tasks.clone(), s.config.clone(), None,
                s.skill_registry.clone(), None, None,
            )
            .await;

            handle_message(
                msg2, s.session_manager.clone(), s.adapters.clone(), cc,
                s.history_writer.clone(), s.consumer_tasks.clone(), s.config.clone(), None,
                s.skill_registry.clone(), None, None,
            )
            .await;

            let tasks = s.consumer_tasks.lock().await;
            assert_eq!(tasks.len(), 2);
        }
    }

    // ---- Gateway struct tests ----

    mod test_gateway {
        use super::*;

        fn make_gateway(dir: &Path) -> Gateway {
            let sm = make_session_manager(dir);
            let hw = HistoryWriter::new(dir.join("history"));
            let mut mock_claude = MockClaudeCaller::new();
            mock_claude
                .expect_call()
                .returning(|_| claude_success("ok"));
            let cc: Arc<dyn ClaudeCaller> = Arc::new(mock_claude);
            let mut config = make_config();
            config.claude.working_directory = dir.to_str().unwrap().to_string();
            let sr = SkillRegistry::new(dir.join(".claude").join("skills"));

            Gateway::new(config, sm, hw, cc, None, sr, None)
        }

        #[tokio::test]
        async fn register_adapter_stores_by_platform() {
            let dir = TempDir::new().unwrap();
            let gw = make_gateway(dir.path());

            let mut mock = MockPlatformAdapter::new();
            mock.expect_start().returning(|| Ok(()));
            mock.expect_set_message_handler().returning(|_| ());
            gw.register_adapter("telegram", mock).await;

            let adapters = gw.adapters.lock().await;
            assert!(adapters.contains_key("telegram"));
            assert_eq!(adapters.len(), 1);
        }

        #[tokio::test]
        async fn register_multiple_adapters() {
            let dir = TempDir::new().unwrap();
            let gw = make_gateway(dir.path());

            let mut mock1 = MockPlatformAdapter::new();
            mock1.expect_start().returning(|| Ok(()));
            mock1.expect_set_message_handler().returning(|_| ());
            gw.register_adapter("telegram", mock1).await;

            let mut mock2 = MockPlatformAdapter::new();
            mock2.expect_start().returning(|| Ok(()));
            mock2.expect_set_message_handler().returning(|_| ());
            gw.register_adapter("discord", mock2).await;

            let adapters = gw.adapters.lock().await;
            assert_eq!(adapters.len(), 2);
        }

        #[tokio::test]
        async fn start_calls_adapter_start() {
            let dir = TempDir::new().unwrap();
            let mut gw = make_gateway(dir.path());

            let started = Arc::new(AtomicBool::new(false));
            let s = started.clone();
            let mut mock = MockPlatformAdapter::new();
            mock.expect_start().returning(move || {
                s.store(true, Ordering::SeqCst);
                Ok(())
            });
            mock.expect_set_message_handler().returning(|_| ());
            gw.register_adapter("telegram", mock).await;

            gw.start().await;
            assert!(started.load(Ordering::SeqCst));
        }

        #[tokio::test]
        async fn start_sets_message_handler() {
            let dir = TempDir::new().unwrap();
            let mut gw = make_gateway(dir.path());

            let handler_set = Arc::new(AtomicBool::new(false));
            let hs = handler_set.clone();
            let mut mock = MockPlatformAdapter::new();
            mock.expect_start().returning(|| Ok(()));
            mock.expect_set_message_handler().returning(move |_| {
                hs.store(true, Ordering::SeqCst);
            });
            gw.register_adapter("telegram", mock).await;

            gw.start().await;
            assert!(handler_set.load(Ordering::SeqCst));
        }

        #[tokio::test]
        async fn start_sets_running_true() {
            let dir = TempDir::new().unwrap();
            let mut gw = make_gateway(dir.path());

            assert!(!gw.is_running());
            gw.start().await;
            assert!(gw.is_running());
        }

        #[tokio::test]
        async fn start_records_start_time() {
            let dir = TempDir::new().unwrap();
            let mut gw = make_gateway(dir.path());

            assert!(gw.start_time().is_none());
            gw.start().await;
            assert!(gw.start_time().is_some());
        }

        #[tokio::test]
        async fn start_loads_sessions() {
            let dir = TempDir::new().unwrap();
            // Pre-create a sessions file
            let mut pre_sm = make_session_manager(dir.path());
            pre_sm.get_or_create("telegram", "999");
            pre_sm.save().unwrap();

            let mut gw = make_gateway(dir.path());
            gw.start().await;

            let sm = gw.session_manager.lock().await;
            let key = SessionManager::conversation_key("telegram", "999");
            assert!(sm.get(&key).is_some());
        }

        #[tokio::test]
        async fn start_runs_file_cleanup() {
            let dir = TempDir::new().unwrap();
            // Create submitted-files dir with an old file
            let files_dir = dir.path().join("submitted-files");
            std::fs::create_dir(&files_dir).unwrap();
            let old_file = files_dir.join("old.txt");
            std::fs::write(&old_file, "data").unwrap();
            let old_time = filetime::FileTime::from_system_time(
                std::time::SystemTime::now() - Duration::from_secs(31 * 86400),
            );
            filetime::set_file_mtime(&old_file, old_time).unwrap();

            let mut gw = make_gateway(dir.path());
            gw.start().await;

            assert!(!old_file.exists());
        }

        #[tokio::test]
        async fn shutdown_sets_running_false() {
            let dir = TempDir::new().unwrap();
            let mut gw = make_gateway(dir.path());
            gw.start().await;
            assert!(gw.is_running());

            gw.shutdown().await;
            assert!(!gw.is_running());
        }

        #[tokio::test]
        async fn shutdown_stops_adapters() {
            let dir = TempDir::new().unwrap();
            let mut gw = make_gateway(dir.path());

            let stopped = Arc::new(AtomicBool::new(false));
            let s = stopped.clone();
            let mut mock = MockPlatformAdapter::new();
            mock.expect_start().returning(|| Ok(()));
            mock.expect_set_message_handler().returning(|_| ());
            mock.expect_stop().returning(move || {
                s.store(true, Ordering::SeqCst);
                Ok(())
            });
            gw.register_adapter("telegram", mock).await;

            gw.start().await;
            gw.shutdown().await;
            assert!(stopped.load(Ordering::SeqCst));
        }

        #[tokio::test]
        async fn shutdown_sends_restart_to_processing_sessions() {
            let dir = TempDir::new().unwrap();
            let mut gw = make_gateway(dir.path());

            let sent = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let sent_clone = sent.clone();
            let mut mock = MockPlatformAdapter::new();
            mock.expect_start().returning(|| Ok(()));
            mock.expect_set_message_handler().returning(|_| ());
            mock.expect_stop().returning(|| Ok(()));
            mock.expect_send_message()
                .returning(move |_, text| {
                    sent_clone.lock().unwrap().push(text.to_string());
                    Ok(())
                });
            gw.register_adapter("telegram", mock).await;

            // Create a session and mark it as processing
            {
                let mut sm = gw.session_manager.lock().await;
                sm.get_or_create("telegram", "123");
                let key = SessionManager::conversation_key("telegram", "123");
                sm.get_mut(&key).unwrap().processing = true;
            }

            gw.start().await;
            gw.shutdown().await;

            let sent = sent.lock().unwrap();
            assert!(sent.contains(&RESTART_MESSAGE.to_string()));
        }

        #[tokio::test]
        async fn shutdown_no_restart_for_non_processing() {
            let dir = TempDir::new().unwrap();
            let mut gw = make_gateway(dir.path());

            let mut mock = MockPlatformAdapter::new();
            mock.expect_start().returning(|| Ok(()));
            mock.expect_set_message_handler().returning(|_| ());
            mock.expect_stop().returning(|| Ok(()));
            mock.expect_send_message().never(); // No messages should be sent
            gw.register_adapter("telegram", mock).await;

            // Create a session but don't mark as processing
            {
                let mut sm = gw.session_manager.lock().await;
                sm.get_or_create("telegram", "123");
            }

            gw.start().await;
            gw.shutdown().await;
        }

        #[tokio::test]
        async fn active_session_count_delegates() {
            let dir = TempDir::new().unwrap();
            let gw = make_gateway(dir.path());

            assert_eq!(gw.active_session_count().await, 0);

            {
                let mut sm = gw.session_manager.lock().await;
                sm.get_or_create("telegram", "123");
            }

            assert_eq!(gw.active_session_count().await, 1);
        }
    }
}
