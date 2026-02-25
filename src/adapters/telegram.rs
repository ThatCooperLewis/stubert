use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::adapters::markdown::to_telegram;
use crate::adapters::message_split::split_message;
use crate::adapters::sanitize::sanitize_filename;
use crate::adapters::{AdapterError, IncomingMessage, MessageHandler, PlatformAdapter};
use crate::config::TelegramConfig;

// ---------------------------------------------------------------------------
// TelegramApi trait — mockable abstraction over teloxide Bot
// ---------------------------------------------------------------------------

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait TelegramApi: Send + Sync {
    async fn send_message(&self, chat_id: i64, text: &str) -> Result<(), String>;
    async fn send_chat_action_typing(&self, chat_id: i64) -> Result<(), String>;
    async fn get_file(&self, file_id: &str) -> Result<String, String>;
    async fn download_file(&self, file_path: &str, destination: &Path) -> Result<(), String>;
    async fn set_my_commands(&self, commands: Vec<(String, String)>) -> Result<(), String>;
}

// ---------------------------------------------------------------------------
// Intermediate structs for testability
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct PhotoInfo {
    file_id: String,
    file_unique_id: String,
}

#[derive(Debug, Clone)]
struct VoiceInfo {
    file_id: String,
    file_unique_id: String,
}

#[derive(Debug, Clone)]
struct DocumentInfo {
    file_id: String,
    file_unique_id: String,
    file_name: Option<String>,
}

#[derive(Debug, Clone)]
struct ParsedMessage {
    user_id: u64,
    username: Option<String>,
    chat_id: i64,
    text: Option<String>,
    photos: Vec<PhotoInfo>,
    voice: Option<VoiceInfo>,
    audio: Option<VoiceInfo>,
    document: Option<DocumentInfo>,
}

// ---------------------------------------------------------------------------
// Pure functions
// ---------------------------------------------------------------------------

/// Strip `@botname` suffix from slash commands.
///
/// `/help@stubert_bot` → `/help`
/// `/models@stubert_bot sonnet` → `/models sonnet`
/// Non-command text is unchanged.
fn strip_bot_suffix(text: &str) -> String {
    if !text.starts_with('/') {
        return text.to_string();
    }

    let mut parts = text.splitn(2, ' ');
    let command_part = parts.next().unwrap_or("");
    let rest = parts.next();

    let stripped_command = match command_part.find('@') {
        Some(pos) => &command_part[..pos],
        None => command_part,
    };

    match rest {
        Some(args) => format!("{stripped_command} {args}"),
        None => stripped_command.to_string(),
    }
}

/// Returns the 9 standard bot commands with descriptions.
fn bot_commands() -> Vec<(String, String)> {
    vec![
        ("new".into(), "Start a new conversation".into()),
        ("context".into(), "Set or view the current context".into()),
        ("restart".into(), "Restart the current session".into()),
        ("models".into(), "Switch or view Claude model".into()),
        ("skill".into(), "Run a skill".into()),
        ("history".into(), "Search conversation history".into()),
        ("status".into(), "Show session status".into()),
        ("heartbeat".into(), "Trigger a heartbeat check".into()),
        ("help".into(), "Show available commands".into()),
    ]
}

// ---------------------------------------------------------------------------
// Download helpers
// ---------------------------------------------------------------------------

async fn download_media(
    api: &dyn TelegramApi,
    file_id: &str,
    file_unique_id: &str,
    extension: &str,
    label: &str,
    files_dir: &Path,
    chat_id: i64,
) -> Option<PathBuf> {
    let dir = files_dir.join(format!("submitted-files/telegram-{chat_id}"));
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        error!("Failed to create {label} directory: {e}");
        return None;
    }

    let dest = dir.join(format!("{file_unique_id}.{extension}"));
    match api.get_file(file_id).await {
        Ok(path) => match api.download_file(&path, &dest).await {
            Ok(()) => Some(dest),
            Err(e) => {
                warn!("Failed to download {label}: {e}");
                None
            }
        },
        Err(e) => {
            warn!("Failed to get {label} file info: {e}");
            None
        }
    }
}

async fn download_document(
    api: &dyn TelegramApi,
    doc: &DocumentInfo,
    files_dir: &Path,
    chat_id: i64,
    existing_files: &[String],
) -> Option<(PathBuf, String)> {
    let dir = files_dir.join(format!("submitted-files/telegram-{chat_id}"));
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        error!("Failed to create document directory: {e}");
        return None;
    }

    let raw_name = doc
        .file_name
        .as_deref()
        .unwrap_or(&doc.file_unique_id);
    let safe_name = sanitize_filename(raw_name, existing_files);
    let dest = dir.join(&safe_name);

    match api.get_file(&doc.file_id).await {
        Ok(file_path) => match api.download_file(&file_path, &dest).await {
            Ok(()) => Some((dest, safe_name)),
            Err(e) => {
                warn!("Failed to download document: {e}");
                None
            }
        },
        Err(e) => {
            warn!("Failed to get document file info: {e}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Core message processing
// ---------------------------------------------------------------------------

async fn process_parsed_message(
    msg: ParsedMessage,
    config: &TelegramConfig,
    api: &dyn TelegramApi,
    handler: &MessageHandler,
    files_dir: &Path,
) {
    // Allowlist check
    if !config.allowed_users.contains(&msg.user_id) {
        debug!("Unauthorized user {} in chat {}", msg.user_id, msg.chat_id);
        if let Some(ref response) = config.unauthorized_response {
            if let Err(e) = api.send_message(msg.chat_id, response).await {
                warn!("Failed to send unauthorized response: {e}");
            }
        }
        return;
    }

    // Extract and process text
    let text = msg.text.map(|t| strip_bot_suffix(&t));

    // Download media
    let mut image_paths = Vec::new();
    let mut audio_paths = Vec::new();
    let mut file_paths = Vec::new();
    let mut file_names = Vec::new();

    // Photos — pick highest resolution (last in vec)
    if let Some(photo) = msg.photos.last() {
        if let Some(path) =
            download_media(api, &photo.file_id, &photo.file_unique_id, "jpg", "photo", files_dir, msg.chat_id).await
        {
            image_paths.push(path);
        }
    }

    // Voice
    if let Some(ref voice) = msg.voice {
        if let Some(path) =
            download_media(api, &voice.file_id, &voice.file_unique_id, "ogg", "voice", files_dir, msg.chat_id).await
        {
            audio_paths.push(path);
        }
    }

    // Audio
    if let Some(ref audio) = msg.audio {
        if let Some(path) =
            download_media(api, &audio.file_id, &audio.file_unique_id, "ogg", "audio", files_dir, msg.chat_id).await
        {
            audio_paths.push(path);
        }
    }

    // Document
    if let Some(ref doc) = msg.document {
        if let Some((path, name)) =
            download_document(api, doc, files_dir, msg.chat_id, &file_names).await
        {
            file_paths.push(path);
            file_names.push(name);
        }
    }

    let incoming = IncomingMessage {
        platform: "telegram".to_string(),
        user_id: msg.user_id.to_string(),
        username: msg.username,
        chat_id: msg.chat_id.to_string(),
        text,
        image_paths,
        audio_paths,
        file_paths,
        file_names,
    };

    handler(incoming).await;
}

// ---------------------------------------------------------------------------
// Parse teloxide Message → ParsedMessage
// ---------------------------------------------------------------------------

fn parse_telegram_message(msg: &teloxide::types::Message) -> Option<ParsedMessage> {
    let user = msg.from.as_ref()?;

    let photos: Vec<PhotoInfo> = msg
        .photo()
        .map(|sizes| {
            sizes
                .iter()
                .map(|p| PhotoInfo {
                    file_id: p.file.id.to_string(),
                    file_unique_id: p.file.unique_id.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();

    let voice = msg.voice().map(|v| VoiceInfo {
        file_id: v.file.id.to_string(),
        file_unique_id: v.file.unique_id.to_string(),
    });

    let audio = msg.audio().map(|a| VoiceInfo {
        file_id: a.file.id.to_string(),
        file_unique_id: a.file.unique_id.to_string(),
    });

    let document = msg.document().map(|d| DocumentInfo {
        file_id: d.file.id.to_string(),
        file_unique_id: d.file.unique_id.to_string(),
        file_name: d.file_name.clone(),
    });

    // text() returns the message text; caption() returns media captions
    let text = msg
        .text()
        .or_else(|| msg.caption())
        .map(|s| s.to_string());

    Some(ParsedMessage {
        user_id: user.id.0,
        username: Some(user.first_name.clone()),
        chat_id: msg.chat.id.0,
        text,
        photos,
        voice,
        audio,
        document,
    })
}

// ---------------------------------------------------------------------------
// RealTelegramApi — thin wrapper around teloxide::Bot
// ---------------------------------------------------------------------------

struct RealTelegramApi {
    bot: teloxide::Bot,
}

#[async_trait]
impl TelegramApi for RealTelegramApi {
    async fn send_message(&self, chat_id: i64, text: &str) -> Result<(), String> {
        use teloxide::payloads::SendMessageSetters;
        use teloxide::requests::Requester;
        use teloxide::types::{ChatId, ParseMode};

        self.bot
            .send_message(ChatId(chat_id), text)
            .parse_mode(ParseMode::MarkdownV2)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn send_chat_action_typing(&self, chat_id: i64) -> Result<(), String> {
        use teloxide::requests::Requester;
        use teloxide::types::{ChatAction, ChatId};

        self.bot
            .send_chat_action(ChatId(chat_id), ChatAction::Typing)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn get_file(&self, file_id: &str) -> Result<String, String> {
        use teloxide::requests::Requester;

        let fid: teloxide::types::FileId = file_id.to_string().into();
        let file = self
            .bot
            .get_file(fid)
            .await
            .map_err(|e| e.to_string())?;
        Ok(file.path)
    }

    async fn download_file(&self, file_path: &str, destination: &Path) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;

        let url = format!(
            "https://api.telegram.org/file/bot{}/{}",
            self.bot.token(),
            file_path
        );

        let response = reqwest::get(&url)
            .await
            .map_err(|e| format!("HTTP request failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("HTTP error: {e}"))?;

        let bytes = response
            .bytes()
            .await
            .map_err(|e| format!("Failed to read response: {e}"))?;

        let mut file = tokio::fs::File::create(destination)
            .await
            .map_err(|e| format!("Failed to create file: {e}"))?;

        file.write_all(&bytes)
            .await
            .map_err(|e| format!("Failed to write file: {e}"))?;

        Ok(())
    }

    async fn set_my_commands(&self, commands: Vec<(String, String)>) -> Result<(), String> {
        use teloxide::requests::Requester;
        use teloxide::types::BotCommand;

        let bot_commands: Vec<BotCommand> = commands
            .into_iter()
            .map(|(cmd, desc)| BotCommand::new(cmd, desc))
            .collect();

        self.bot
            .set_my_commands(bot_commands)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// TelegramAdapter
// ---------------------------------------------------------------------------

pub struct TelegramAdapter {
    config: TelegramConfig,
    api: Option<Arc<dyn TelegramApi>>,
    handler: Option<MessageHandler>,
    files_dir: PathBuf,
    running: bool,
    poll_handle: Option<JoinHandle<()>>,
}

impl TelegramAdapter {
    pub fn new(config: TelegramConfig, files_dir: PathBuf) -> Self {
        Self {
            config,
            api: None,
            handler: None,
            files_dir,
            running: false,
            poll_handle: None,
        }
    }

    /// Create an adapter with a custom API implementation (for testing).
    #[cfg(test)]
    fn with_api(config: TelegramConfig, files_dir: PathBuf, api: Arc<dyn TelegramApi>) -> Self {
        Self {
            config,
            api: Some(api),
            handler: None,
            files_dir,
            running: false,
            poll_handle: None,
        }
    }
}

#[async_trait]
impl PlatformAdapter for TelegramAdapter {
    async fn start(&mut self) -> Result<(), AdapterError> {
        if self.running {
            return Err(AdapterError::AlreadyStarted);
        }

        // Create API if not injected (test vs production)
        if self.api.is_none() {
            let bot = teloxide::Bot::new(&self.config.token);
            self.api = Some(Arc::new(RealTelegramApi { bot }));
        }

        let api = self.api.as_ref().unwrap().clone();

        // Register bot commands
        if let Err(e) = api.set_my_commands(bot_commands()).await {
            warn!("Failed to set bot commands: {e}");
        }

        // Spawn polling task
        let config = self.config.clone();
        let handler = self
            .handler
            .clone()
            .expect("message handler must be set before start");
        let files_dir = self.files_dir.clone();

        let handle = tokio::spawn(async move {
            run_polling(config, api, handler, files_dir).await;
        });

        self.poll_handle = Some(handle);
        self.running = true;
        info!("Telegram adapter started");
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), AdapterError> {
        if !self.running {
            return Err(AdapterError::NotStarted);
        }

        if let Some(handle) = self.poll_handle.take() {
            handle.abort();
        }

        self.running = false;
        info!("Telegram adapter stopped");
        Ok(())
    }

    async fn send_message(&self, chat_id: &str, text: &str) -> Result<(), AdapterError> {
        let api = self
            .api
            .as_ref()
            .ok_or(AdapterError::NotStarted)?;

        let chat_id_num: i64 = chat_id
            .parse()
            .map_err(|e| AdapterError::SendFailed(format!("invalid chat_id: {e}")))?;

        let converted = to_telegram(text);
        let chunks = split_message(&converted, 2000);

        for chunk in &chunks {
            api.send_message(chat_id_num, chunk)
                .await
                .map_err(|e| AdapterError::SendFailed(e))?;
        }

        Ok(())
    }

    async fn send_typing(&self, chat_id: &str) -> Result<(), AdapterError> {
        let api = self
            .api
            .as_ref()
            .ok_or(AdapterError::NotStarted)?;

        let chat_id_num: i64 = chat_id
            .parse()
            .map_err(|e| AdapterError::PlatformError(format!("invalid chat_id: {e}")))?;

        api.send_chat_action_typing(chat_id_num)
            .await
            .map_err(|e| AdapterError::PlatformError(e))
    }

    fn set_message_handler(&mut self, handler: MessageHandler) {
        self.handler = Some(handler);
    }
}

// ---------------------------------------------------------------------------
// Polling loop (not unit-tested — thin teloxide glue)
// ---------------------------------------------------------------------------

async fn run_polling(
    config: TelegramConfig,
    api: Arc<dyn TelegramApi>,
    handler: MessageHandler,
    files_dir: PathBuf,
) {
    use teloxide::prelude::*;
    use teloxide::types::UpdateKind;

    // Custom HTTP client with TCP keepalive to prevent NAT/firewall from
    // dropping the long-poll connection during the 30-second idle wait.
    let client = reqwest::Client::builder()
        .tcp_keepalive(std::time::Duration::from_secs(15))
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .expect("Failed to build HTTP client for Telegram polling");

    let bot = teloxide::Bot::with_client(&config.token, client);
    let mut offset: Option<i32> = None;
    let mut consecutive_errors: u32 = 0;

    info!("Telegram polling loop started");

    loop {
        let mut req = bot.get_updates();
        if let Some(off) = offset {
            req = req.offset(off);
        }
        req = req.timeout(30);

        match req.await {
            Ok(updates) => {
                if consecutive_errors > 0 {
                    info!(
                        "Telegram polling recovered after {} consecutive error(s)",
                        consecutive_errors
                    );
                    consecutive_errors = 0;
                }

                for update in updates {
                    offset = Some(update.id.as_offset());

                    if let UpdateKind::Message(msg) = update.kind {
                        if let Some(parsed) = parse_telegram_message(&msg) {
                            process_parsed_message(
                                parsed,
                                &config,
                                api.as_ref(),
                                &handler,
                                &files_dir,
                            )
                            .await;
                        }
                    }
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors == 1 {
                    warn!("Telegram polling error: {e}");
                } else if consecutive_errors.is_power_of_two() {
                    warn!(
                        "Telegram polling error ({} consecutive): {e}",
                        consecutive_errors
                    );
                }
                let backoff = std::cmp::min(5 * consecutive_errors as u64, 60);
                tokio::time::sleep(tokio::time::Duration::from_secs(backoff)).await;
            }
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex as StdMutex;

    fn make_config() -> TelegramConfig {
        TelegramConfig {
            token: "test-token".to_string(),
            allowed_users: vec![111, 222],
            unauthorized_response: None,
        }
    }

    fn make_config_with_unauth_response() -> TelegramConfig {
        TelegramConfig {
            token: "test-token".to_string(),
            allowed_users: vec![111],
            unauthorized_response: Some("Not authorized".to_string()),
        }
    }

    fn make_parsed(user_id: u64, chat_id: i64, text: Option<&str>) -> ParsedMessage {
        ParsedMessage {
            user_id,
            username: None,
            chat_id,
            text: text.map(|s| s.to_string()),
            photos: vec![],
            voice: None,
            audio: None,
            document: None,
        }
    }

    fn noop_handler() -> MessageHandler {
        Arc::new(|_msg| Box::pin(async {}))
    }

    fn capturing_handler() -> (MessageHandler, Arc<StdMutex<Vec<IncomingMessage>>>) {
        let captured: Arc<StdMutex<Vec<IncomingMessage>>> = Arc::new(StdMutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let handler: MessageHandler = Arc::new(move |msg| {
            let cap = captured_clone.clone();
            Box::pin(async move {
                cap.lock().unwrap().push(msg);
            })
        });
        (handler, captured)
    }

    // -----------------------------------------------------------------------
    // strip_bot_suffix tests
    // -----------------------------------------------------------------------

    mod test_strip_bot_suffix {
        use super::*;

        #[test]
        fn strips_bot_name_from_command() {
            assert_eq!(strip_bot_suffix("/help@stubert_bot"), "/help");
        }

        #[test]
        fn strips_bot_name_preserves_args() {
            assert_eq!(
                strip_bot_suffix("/models@stubert_bot sonnet"),
                "/models sonnet"
            );
        }

        #[test]
        fn leaves_command_without_bot_name() {
            assert_eq!(strip_bot_suffix("/help"), "/help");
        }

        #[test]
        fn leaves_non_command_unchanged() {
            assert_eq!(strip_bot_suffix("hello world"), "hello world");
        }

        #[test]
        fn handles_empty_string() {
            assert_eq!(strip_bot_suffix(""), "");
        }
    }

    // -----------------------------------------------------------------------
    // bot_commands tests
    // -----------------------------------------------------------------------

    mod test_bot_commands {
        use super::*;

        #[test]
        fn returns_nine_commands() {
            let cmds = bot_commands();
            assert_eq!(cmds.len(), 9);
        }

        #[test]
        fn contains_expected_commands() {
            let cmds = bot_commands();
            let names: Vec<&str> = cmds.iter().map(|(n, _)| n.as_str()).collect();
            assert!(names.contains(&"new"));
            assert!(names.contains(&"context"));
            assert!(names.contains(&"restart"));
            assert!(names.contains(&"models"));
            assert!(names.contains(&"skill"));
            assert!(names.contains(&"history"));
            assert!(names.contains(&"status"));
            assert!(names.contains(&"heartbeat"));
            assert!(names.contains(&"help"));
        }
    }

    // -----------------------------------------------------------------------
    // process_parsed_message tests
    // -----------------------------------------------------------------------

    mod test_process_parsed_message {
        use super::*;
        use tempfile::TempDir;

        #[tokio::test]
        async fn text_message_produces_correct_incoming() {
            let config = make_config();
            let mut mock_api = MockTelegramApi::new();
            mock_api.expect_send_message().never();
            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let msg = make_parsed(111, 42, Some("hello world"));
            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].platform, "telegram");
            assert_eq!(msgs[0].user_id, "111");
            assert_eq!(msgs[0].chat_id, "42");
            assert_eq!(msgs[0].text.as_deref(), Some("hello world"));
            assert!(msgs[0].image_paths.is_empty());
            assert!(msgs[0].audio_paths.is_empty());
            assert!(msgs[0].file_paths.is_empty());
            assert!(msgs[0].file_names.is_empty());
        }

        #[tokio::test]
        async fn photo_message_downloads_and_sets_image_paths() {
            let config = make_config();
            let mut mock_api = MockTelegramApi::new();
            mock_api
                .expect_get_file()
                .withf(|id| id == "photo_file_id")
                .returning(|_| Ok("photos/photo123.jpg".to_string()));
            mock_api
                .expect_download_file()
                .returning(|_, dest| {
                    std::fs::create_dir_all(dest.parent().unwrap()).ok();
                    std::fs::write(dest, b"fake photo data").unwrap();
                    Ok(())
                });

            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let mut msg = make_parsed(111, 42, None);
            msg.photos = vec![
                PhotoInfo {
                    file_id: "small_id".into(),
                    file_unique_id: "small_unique".into(),
                },
                PhotoInfo {
                    file_id: "photo_file_id".into(),
                    file_unique_id: "photo_unique".into(),
                },
            ];

            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].image_paths.len(), 1);
            assert!(msgs[0].image_paths[0]
                .to_str()
                .unwrap()
                .contains("photo_unique.jpg"));
        }

        #[tokio::test]
        async fn voice_message_downloads_and_sets_audio_paths() {
            let config = make_config();
            let mut mock_api = MockTelegramApi::new();
            mock_api
                .expect_get_file()
                .returning(|_| Ok("voice/voice123.ogg".to_string()));
            mock_api
                .expect_download_file()
                .returning(|_, dest| {
                    std::fs::create_dir_all(dest.parent().unwrap()).ok();
                    std::fs::write(dest, b"fake voice data").unwrap();
                    Ok(())
                });

            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let mut msg = make_parsed(111, 42, None);
            msg.voice = Some(VoiceInfo {
                file_id: "voice_file_id".into(),
                file_unique_id: "voice_unique".into(),
            });

            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].audio_paths.len(), 1);
            assert!(msgs[0].audio_paths[0]
                .to_str()
                .unwrap()
                .contains("voice_unique.ogg"));
        }

        #[tokio::test]
        async fn document_downloads_with_sanitized_filename() {
            let config = make_config();
            let mut mock_api = MockTelegramApi::new();
            mock_api
                .expect_get_file()
                .returning(|_| Ok("docs/doc123".to_string()));
            mock_api
                .expect_download_file()
                .returning(|_, dest| {
                    std::fs::create_dir_all(dest.parent().unwrap()).ok();
                    std::fs::write(dest, b"fake doc data").unwrap();
                    Ok(())
                });

            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let mut msg = make_parsed(111, 42, None);
            msg.document = Some(DocumentInfo {
                file_id: "doc_file_id".into(),
                file_unique_id: "doc_unique".into(),
                file_name: Some("my file (1).pdf".into()),
            });

            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].file_paths.len(), 1);
            assert_eq!(msgs[0].file_names.len(), 1);
            assert_eq!(msgs[0].file_names[0], "my_file__1_.pdf");
        }

        #[tokio::test]
        async fn caption_extracted_from_photo_message() {
            let config = make_config();
            let mut mock_api = MockTelegramApi::new();
            mock_api
                .expect_get_file()
                .returning(|_| Ok("photos/p.jpg".to_string()));
            mock_api
                .expect_download_file()
                .returning(|_, dest| {
                    std::fs::create_dir_all(dest.parent().unwrap()).ok();
                    std::fs::write(dest, b"img").unwrap();
                    Ok(())
                });

            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let mut msg = make_parsed(111, 42, Some("Look at this photo"));
            msg.photos = vec![PhotoInfo {
                file_id: "ph1".into(),
                file_unique_id: "phu1".into(),
            }];

            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs[0].text.as_deref(), Some("Look at this photo"));
            assert_eq!(msgs[0].image_paths.len(), 1);
        }

        #[tokio::test]
        async fn bot_suffix_stripped_from_command() {
            let config = make_config();
            let mock_api = MockTelegramApi::new();
            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let msg = make_parsed(111, 42, Some("/help@stubert_bot"));
            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs[0].text.as_deref(), Some("/help"));
        }

        #[tokio::test]
        async fn unauthorized_user_handler_not_called() {
            let config = make_config();
            let mock_api = MockTelegramApi::new();
            let called = Arc::new(AtomicBool::new(false));
            let called_clone = called.clone();
            let handler: MessageHandler = Arc::new(move |_| {
                called_clone.store(true, Ordering::SeqCst);
                Box::pin(async {})
            });
            let tmp = TempDir::new().unwrap();

            let msg = make_parsed(999, 42, Some("hello"));
            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            assert!(!called.load(Ordering::SeqCst));
        }

        #[tokio::test]
        async fn unauthorized_user_with_response_sends_message() {
            let config = make_config_with_unauth_response();
            let mut mock_api = MockTelegramApi::new();
            mock_api
                .expect_send_message()
                .withf(|chat_id, text| *chat_id == 42 && text == "Not authorized")
                .times(1)
                .returning(|_, _| Ok(()));

            let called = Arc::new(AtomicBool::new(false));
            let called_clone = called.clone();
            let handler: MessageHandler = Arc::new(move |_| {
                called_clone.store(true, Ordering::SeqCst);
                Box::pin(async {})
            });
            let tmp = TempDir::new().unwrap();

            let msg = make_parsed(999, 42, Some("hello"));
            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            assert!(!called.load(Ordering::SeqCst));
        }

        #[tokio::test]
        async fn photo_selects_highest_resolution() {
            let config = make_config();
            let mut mock_api = MockTelegramApi::new();
            // Only the last (highest-res) photo should be downloaded
            mock_api
                .expect_get_file()
                .withf(|id| id == "large_id")
                .times(1)
                .returning(|_| Ok("photos/large.jpg".to_string()));
            mock_api
                .expect_download_file()
                .returning(|_, dest| {
                    std::fs::create_dir_all(dest.parent().unwrap()).ok();
                    std::fs::write(dest, b"large photo").unwrap();
                    Ok(())
                });

            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let mut msg = make_parsed(111, 42, None);
            msg.photos = vec![
                PhotoInfo {
                    file_id: "small_id".into(),
                    file_unique_id: "small_u".into(),
                },
                PhotoInfo {
                    file_id: "medium_id".into(),
                    file_unique_id: "medium_u".into(),
                },
                PhotoInfo {
                    file_id: "large_id".into(),
                    file_unique_id: "large_u".into(),
                },
            ];

            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs[0].image_paths.len(), 1);
            assert!(msgs[0].image_paths[0]
                .to_str()
                .unwrap()
                .contains("large_u.jpg"));
        }

        #[tokio::test]
        async fn audio_message_downloads_and_sets_audio_paths() {
            let config = make_config();
            let mut mock_api = MockTelegramApi::new();
            mock_api
                .expect_get_file()
                .returning(|_| Ok("audio/track.ogg".to_string()));
            mock_api
                .expect_download_file()
                .returning(|_, dest| {
                    std::fs::create_dir_all(dest.parent().unwrap()).ok();
                    std::fs::write(dest, b"fake audio data").unwrap();
                    Ok(())
                });

            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let mut msg = make_parsed(111, 42, None);
            msg.audio = Some(VoiceInfo {
                file_id: "audio_file_id".into(),
                file_unique_id: "audio_unique".into(),
            });

            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].audio_paths.len(), 1);
            assert!(msgs[0].audio_paths[0]
                .to_str()
                .unwrap()
                .contains("audio_unique.ogg"));
        }

        #[tokio::test]
        async fn document_without_filename_uses_unique_id() {
            let config = make_config();
            let mut mock_api = MockTelegramApi::new();
            mock_api
                .expect_get_file()
                .returning(|_| Ok("docs/d1".to_string()));
            mock_api
                .expect_download_file()
                .returning(|_, dest| {
                    std::fs::create_dir_all(dest.parent().unwrap()).ok();
                    std::fs::write(dest, b"data").unwrap();
                    Ok(())
                });

            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let mut msg = make_parsed(111, 42, None);
            msg.document = Some(DocumentInfo {
                file_id: "d_fid".into(),
                file_unique_id: "d_uid_abc".into(),
                file_name: None,
            });

            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs[0].file_names[0], "d_uid_abc");
        }
    }

    // -----------------------------------------------------------------------
    // Download helper tests
    // -----------------------------------------------------------------------

    mod test_downloads {
        use super::*;
        use tempfile::TempDir;

        fn mock_api_with_download() -> MockTelegramApi {
            let mut mock_api = MockTelegramApi::new();
            mock_api
                .expect_get_file()
                .returning(|_| Ok("remote/path".to_string()));
            mock_api
                .expect_download_file()
                .returning(|_, dest| {
                    std::fs::create_dir_all(dest.parent().unwrap()).ok();
                    std::fs::write(dest, b"file bytes").unwrap();
                    Ok(())
                });
            mock_api
        }

        #[tokio::test]
        async fn download_media_creates_directory_and_file() {
            let mock_api = mock_api_with_download();
            let tmp = TempDir::new().unwrap();

            let result =
                download_media(&mock_api, "fid", "uid123", "jpg", "photo", tmp.path(), 42).await;
            assert!(result.is_some());
            let path = result.unwrap();
            assert!(path.exists());
            assert!(path.to_str().unwrap().contains("uid123.jpg"));
            assert!(path
                .to_str()
                .unwrap()
                .contains("submitted-files/telegram-42"));
        }

        #[tokio::test]
        async fn download_media_voice_saves_as_ogg() {
            let mock_api = mock_api_with_download();
            let tmp = TempDir::new().unwrap();

            let result =
                download_media(&mock_api, "vfid", "vuid456", "ogg", "voice", tmp.path(), 42).await;
            assert!(result.is_some());
            let path = result.unwrap();
            assert!(path.to_str().unwrap().contains("vuid456.ogg"));
        }

        #[tokio::test]
        async fn download_document_sanitizes_filename() {
            let mut mock_api = MockTelegramApi::new();
            mock_api
                .expect_get_file()
                .returning(|_| Ok("docs/d1".to_string()));
            mock_api
                .expect_download_file()
                .returning(|_, dest| {
                    std::fs::create_dir_all(dest.parent().unwrap()).ok();
                    std::fs::write(dest, b"doc bytes").unwrap();
                    Ok(())
                });

            let tmp = TempDir::new().unwrap();
            let doc = DocumentInfo {
                file_id: "dfid".into(),
                file_unique_id: "duid".into(),
                file_name: Some("my report (final).pdf".into()),
            };

            let result = download_document(&mock_api, &doc, tmp.path(), 42, &[]).await;
            assert!(result.is_some());
            let (path, name) = result.unwrap();
            assert_eq!(name, "my_report__final_.pdf");
            assert!(path.exists());
        }

        #[tokio::test]
        async fn get_file_failure_returns_none() {
            let mut mock_api = MockTelegramApi::new();
            mock_api
                .expect_get_file()
                .returning(|_| Err("API error".to_string()));

            let tmp = TempDir::new().unwrap();

            let result =
                download_media(&mock_api, "bad_id", "bad_uid", "jpg", "photo", tmp.path(), 42)
                    .await;
            assert!(result.is_none());
        }

        #[tokio::test]
        async fn download_file_failure_returns_none() {
            let mut mock_api = MockTelegramApi::new();
            mock_api
                .expect_get_file()
                .returning(|_| Ok("path/file".to_string()));
            mock_api
                .expect_download_file()
                .returning(|_, _| Err("download failed".to_string()));

            let tmp = TempDir::new().unwrap();

            let result =
                download_media(&mock_api, "vfid", "vuid", "ogg", "voice", tmp.path(), 42).await;
            assert!(result.is_none());
        }
    }

    // -----------------------------------------------------------------------
    // Outbound send_message tests
    // -----------------------------------------------------------------------

    mod test_send_message {
        use super::*;
        use tempfile::TempDir;

        #[tokio::test]
        async fn converts_markdown_and_sends() {
            let config = make_config();
            let mut mock_api = MockTelegramApi::new();
            mock_api
                .expect_send_message()
                .withf(|chat_id, text| {
                    *chat_id == 42 && text.contains("*bold*")
                })
                .times(1)
                .returning(|_, _| Ok(()));

            let tmp = TempDir::new().unwrap();
            let mut adapter = TelegramAdapter::with_api(
                config,
                tmp.path().to_path_buf(),
                Arc::new(mock_api),
            );
            adapter.running = true;
            adapter.set_message_handler(noop_handler());

            let result = adapter.send_message("42", "**bold**").await;
            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn splits_long_messages() {
            let config = make_config();
            let mut mock_api = MockTelegramApi::new();
            // Expect multiple send calls for a long message
            mock_api
                .expect_send_message()
                .returning(|_, _| Ok(()));

            let tmp = TempDir::new().unwrap();
            let api = Arc::new(mock_api);
            let mut adapter =
                TelegramAdapter::with_api(config, tmp.path().to_path_buf(), api);
            adapter.running = true;
            adapter.set_message_handler(noop_handler());

            // Create a message that will exceed 2000 chars after conversion
            let long_text = "a".repeat(3000);
            let result = adapter.send_message("42", &long_text).await;
            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn api_error_returns_send_failed() {
            let config = make_config();
            let mut mock_api = MockTelegramApi::new();
            mock_api
                .expect_send_message()
                .returning(|_, _| Err("API timeout".to_string()));

            let tmp = TempDir::new().unwrap();
            let mut adapter = TelegramAdapter::with_api(
                config,
                tmp.path().to_path_buf(),
                Arc::new(mock_api),
            );
            adapter.running = true;
            adapter.set_message_handler(noop_handler());

            let result = adapter.send_message("42", "hello").await;
            assert!(matches!(result, Err(AdapterError::SendFailed(_))));
        }
    }

    // -----------------------------------------------------------------------
    // send_typing tests
    // -----------------------------------------------------------------------

    mod test_send_typing {
        use super::*;
        use tempfile::TempDir;

        #[tokio::test]
        async fn calls_typing_action() {
            let config = make_config();
            let mut mock_api = MockTelegramApi::new();
            mock_api
                .expect_send_chat_action_typing()
                .withf(|chat_id| *chat_id == 42)
                .times(1)
                .returning(|_| Ok(()));

            let tmp = TempDir::new().unwrap();
            let mut adapter = TelegramAdapter::with_api(
                config,
                tmp.path().to_path_buf(),
                Arc::new(mock_api),
            );
            adapter.running = true;
            adapter.set_message_handler(noop_handler());

            let result = adapter.send_typing("42").await;
            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn api_error_returns_platform_error() {
            let config = make_config();
            let mut mock_api = MockTelegramApi::new();
            mock_api
                .expect_send_chat_action_typing()
                .returning(|_| Err("connection error".to_string()));

            let tmp = TempDir::new().unwrap();
            let mut adapter = TelegramAdapter::with_api(
                config,
                tmp.path().to_path_buf(),
                Arc::new(mock_api),
            );
            adapter.running = true;
            adapter.set_message_handler(noop_handler());

            let result = adapter.send_typing("42").await;
            assert!(matches!(result, Err(AdapterError::PlatformError(_))));
        }
    }

    // -----------------------------------------------------------------------
    // Lifecycle tests
    // -----------------------------------------------------------------------

    mod test_lifecycle {
        use super::*;
        use tempfile::TempDir;

        #[tokio::test]
        async fn start_when_already_started_returns_already_started() {
            let config = make_config();
            let tmp = TempDir::new().unwrap();
            let mock_api = MockTelegramApi::new();
            let mut adapter = TelegramAdapter::with_api(
                config,
                tmp.path().to_path_buf(),
                Arc::new(mock_api),
            );
            adapter.running = true;
            adapter.set_message_handler(noop_handler());

            let result = adapter.start().await;
            assert!(matches!(result, Err(AdapterError::AlreadyStarted)));
        }

        #[tokio::test]
        async fn stop_when_not_started_returns_not_started() {
            let config = make_config();
            let tmp = TempDir::new().unwrap();
            let mut adapter = TelegramAdapter::new(config, tmp.path().to_path_buf());

            let result = adapter.stop().await;
            assert!(matches!(result, Err(AdapterError::NotStarted)));
        }

        #[tokio::test]
        async fn send_message_before_start_returns_not_started() {
            let config = make_config();
            let tmp = TempDir::new().unwrap();
            let adapter = TelegramAdapter::new(config, tmp.path().to_path_buf());

            let result = adapter.send_message("42", "hello").await;
            assert!(matches!(result, Err(AdapterError::NotStarted)));
        }

        #[tokio::test]
        async fn send_typing_before_start_returns_not_started() {
            let config = make_config();
            let tmp = TempDir::new().unwrap();
            let adapter = TelegramAdapter::new(config, tmp.path().to_path_buf());

            let result = adapter.send_typing("42").await;
            assert!(matches!(result, Err(AdapterError::NotStarted)));
        }
    }
}
