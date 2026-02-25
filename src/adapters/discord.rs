use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::adapters::markdown::to_discord;
use crate::adapters::message_split::split_message;
use crate::adapters::sanitize::sanitize_filename;
use crate::adapters::{AdapterError, IncomingMessage, MessageHandler, PlatformAdapter};
use crate::config::DiscordConfig;

// ---------------------------------------------------------------------------
// DiscordApi trait — mockable abstraction over serenity
// ---------------------------------------------------------------------------

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait DiscordApi: Send + Sync {
    async fn send_channel_message(&self, channel_id: u64, text: &str) -> Result<(), String>;
    async fn send_followup(&self, interaction_token: &str, text: &str) -> Result<(), String>;
    async fn defer_interaction(
        &self,
        interaction_id: u64,
        interaction_token: &str,
    ) -> Result<(), String>;
    async fn send_typing(&self, channel_id: u64) -> Result<(), String>;
    async fn download_attachment(&self, url: &str, destination: &Path) -> Result<(), String>;
    async fn register_commands(&self, commands: Vec<SlashCommandDef>) -> Result<(), String>;
}

// ---------------------------------------------------------------------------
// Supporting types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SlashCommandDef {
    pub name: String,
    pub description: String,
    pub options: Vec<SlashCommandOption>,
}

#[derive(Debug, Clone)]
pub struct SlashCommandOption {
    pub name: String,
    pub description: String,
    pub required: bool,
}

#[derive(Debug, Clone)]
struct ParsedDiscordMessage {
    user_id: u64,
    bot_user_id: u64,
    channel_id: u64,
    guild_id: Option<u64>,
    content: String,
    author_is_bot: bool,
    mentions_bot: bool,
    is_reply_to_bot: bool,
    attachments: Vec<AttachmentInfo>,
}

#[derive(Debug, Clone)]
struct ParsedInteraction {
    user_id: u64,
    channel_id: u64,
    interaction_id: u64,
    interaction_token: String,
    command_name: String,
    options: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct AttachmentInfo {
    url: String,
    filename: String,
    content_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum AttachmentCategory {
    Image,
    Audio,
    Other,
}

// ---------------------------------------------------------------------------
// Pure functions
// ---------------------------------------------------------------------------

fn should_activate(msg: &ParsedDiscordMessage) -> bool {
    if msg.author_is_bot {
        return false;
    }
    if msg.guild_id.is_none() {
        return true;
    }
    msg.mentions_bot || msg.is_reply_to_bot
}

fn strip_bot_mention(content: &str, bot_user_id: u64) -> String {
    let mention = format!("<@{}>", bot_user_id);
    let mention_nick = format!("<@!{}>", bot_user_id);
    content
        .replace(&mention, "")
        .replace(&mention_nick, "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn classify_attachment(content_type: &Option<String>) -> AttachmentCategory {
    match content_type.as_deref() {
        Some(ct) if ct.starts_with("image/") => AttachmentCategory::Image,
        Some(ct) if ct.starts_with("audio/") => AttachmentCategory::Audio,
        _ => AttachmentCategory::Other,
    }
}

fn slash_commands() -> Vec<SlashCommandDef> {
    vec![
        SlashCommandDef {
            name: "new".into(),
            description: "Start a new conversation".into(),
            options: vec![],
        },
        SlashCommandDef {
            name: "context".into(),
            description: "Set or view the current context".into(),
            options: vec![],
        },
        SlashCommandDef {
            name: "restart".into(),
            description: "Restart the current session".into(),
            options: vec![],
        },
        SlashCommandDef {
            name: "models".into(),
            description: "Switch or view Claude model".into(),
            options: vec![SlashCommandOption {
                name: "model".into(),
                description: "Model to switch to".into(),
                required: false,
            }],
        },
        SlashCommandDef {
            name: "skill".into(),
            description: "Run a skill".into(),
            options: vec![
                SlashCommandOption {
                    name: "name".into(),
                    description: "Skill name".into(),
                    required: true,
                },
                SlashCommandOption {
                    name: "args".into(),
                    description: "Skill arguments".into(),
                    required: false,
                },
            ],
        },
        SlashCommandDef {
            name: "history".into(),
            description: "Search conversation history".into(),
            options: vec![SlashCommandOption {
                name: "query".into(),
                description: "Search query".into(),
                required: false,
            }],
        },
        SlashCommandDef {
            name: "status".into(),
            description: "Show session status".into(),
            options: vec![],
        },
        SlashCommandDef {
            name: "heartbeat".into(),
            description: "Trigger a heartbeat check".into(),
            options: vec![],
        },
        SlashCommandDef {
            name: "help".into(),
            description: "Show available commands".into(),
            options: vec![],
        },
    ]
}

fn interaction_to_command_text(interaction: &ParsedInteraction) -> String {
    let mut parts = vec![format!("/{}", interaction.command_name)];

    match interaction.command_name.as_str() {
        "models" => {
            if let Some(model) = interaction.options.get("model") {
                parts.push(model.clone());
            }
        }
        "skill" => {
            if let Some(name) = interaction.options.get("name") {
                parts.push(name.clone());
                if let Some(args) = interaction.options.get("args") {
                    parts.push(args.clone());
                }
            }
        }
        "history" => {
            if let Some(query) = interaction.options.get("query") {
                parts.push(query.clone());
            }
        }
        _ => {}
    }

    parts.join(" ")
}

// ---------------------------------------------------------------------------
// Download helper
// ---------------------------------------------------------------------------

async fn download_attachment_to_path(
    api: &dyn DiscordApi,
    attachment: &AttachmentInfo,
    files_dir: &Path,
    channel_id: u64,
    existing_files: &[String],
) -> Option<(PathBuf, String)> {
    let dir = files_dir.join(format!("submitted-files/discord-{channel_id}"));
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        error!("Failed to create attachment directory: {e}");
        return None;
    }

    let safe_name = sanitize_filename(&attachment.filename, existing_files);
    let dest = dir.join(&safe_name);

    match api.download_attachment(&attachment.url, &dest).await {
        Ok(()) => Some((dest, safe_name)),
        Err(e) => {
            warn!("Failed to download attachment: {e}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Core message processing
// ---------------------------------------------------------------------------

type InteractionStore = Arc<TokioMutex<HashMap<String, (String, Instant)>>>;

async fn process_parsed_message(
    msg: ParsedDiscordMessage,
    config: &DiscordConfig,
    api: &dyn DiscordApi,
    handler: &MessageHandler,
    files_dir: &Path,
) {
    if !should_activate(&msg) {
        return;
    }

    if !config.allowed_users.contains(&msg.user_id) {
        debug!(
            "Unauthorized user {} in channel {}",
            msg.user_id, msg.channel_id
        );
        if let Some(ref response) = config.unauthorized_response {
            if let Err(e) = api.send_channel_message(msg.channel_id, response).await {
                warn!("Failed to send unauthorized response: {e}");
            }
        }
        return;
    }

    let stripped = strip_bot_mention(&msg.content, msg.bot_user_id);
    let text = if stripped.is_empty() {
        None
    } else {
        Some(stripped)
    };

    let mut image_paths = Vec::new();
    let mut audio_paths = Vec::new();
    let mut file_paths = Vec::new();
    let mut file_names: Vec<String> = Vec::new();

    for attachment in &msg.attachments {
        let category = classify_attachment(&attachment.content_type);
        match category {
            AttachmentCategory::Image => {
                if let Some((path, name)) = download_attachment_to_path(
                    api,
                    attachment,
                    files_dir,
                    msg.channel_id,
                    &file_names,
                )
                .await
                {
                    image_paths.push(path);
                    file_names.push(name);
                }
            }
            AttachmentCategory::Audio => {
                if let Some((path, name)) = download_attachment_to_path(
                    api,
                    attachment,
                    files_dir,
                    msg.channel_id,
                    &file_names,
                )
                .await
                {
                    audio_paths.push(path);
                    file_names.push(name);
                }
            }
            AttachmentCategory::Other => {
                if let Some((path, name)) = download_attachment_to_path(
                    api,
                    attachment,
                    files_dir,
                    msg.channel_id,
                    &file_names,
                )
                .await
                {
                    file_paths.push(path);
                    file_names.push(name);
                }
            }
        }
    }

    let incoming = IncomingMessage {
        platform: "discord".to_string(),
        user_id: msg.user_id.to_string(),
        chat_id: msg.channel_id.to_string(),
        text,
        image_paths,
        audio_paths,
        file_paths,
        file_names,
    };

    handler(incoming).await;
}

async fn process_parsed_interaction(
    interaction: ParsedInteraction,
    config: &DiscordConfig,
    api: &dyn DiscordApi,
    handler: &MessageHandler,
    interaction_store: &InteractionStore,
) {
    if let Err(e) = api
        .defer_interaction(interaction.interaction_id, &interaction.interaction_token)
        .await
    {
        warn!("Failed to defer interaction: {e}");
    }

    let channel_key = interaction.channel_id.to_string();
    {
        let mut store = interaction_store.lock().await;
        store.insert(channel_key, (interaction.interaction_token.clone(), Instant::now()));
    }

    if !config.allowed_users.contains(&interaction.user_id) {
        debug!(
            "Unauthorized user {} for interaction",
            interaction.user_id
        );
        if let Some(ref response) = config.unauthorized_response {
            if let Err(e) = api
                .send_followup(&interaction.interaction_token, response)
                .await
            {
                warn!("Failed to send unauthorized followup: {e}");
            }
        }
        return;
    }

    let text = interaction_to_command_text(&interaction);

    let incoming = IncomingMessage {
        platform: "discord".to_string(),
        user_id: interaction.user_id.to_string(),
        chat_id: interaction.channel_id.to_string(),
        text: Some(text),
        image_paths: vec![],
        audio_paths: vec![],
        file_paths: vec![],
        file_names: vec![],
    };

    handler(incoming).await;
}

// ---------------------------------------------------------------------------
// Serenity parsing functions (thin wrappers, not unit-tested)
// ---------------------------------------------------------------------------

fn parse_discord_message(
    msg: &serenity::model::channel::Message,
    bot_user_id: u64,
) -> ParsedDiscordMessage {
    let mentions_bot = msg.mentions.iter().any(|u| u.id.get() == bot_user_id);
    let is_reply_to_bot = msg
        .referenced_message
        .as_ref()
        .map(|m| m.author.id.get() == bot_user_id)
        .unwrap_or(false);

    let attachments = msg
        .attachments
        .iter()
        .map(|a| AttachmentInfo {
            url: a.url.clone(),
            filename: a.filename.clone(),
            content_type: a.content_type.clone(),
        })
        .collect();

    ParsedDiscordMessage {
        user_id: msg.author.id.get(),
        bot_user_id,
        channel_id: msg.channel_id.get(),
        guild_id: msg.guild_id.map(|g| g.get()),
        content: msg.content.clone(),
        author_is_bot: msg.author.bot,
        mentions_bot,
        is_reply_to_bot,
        attachments,
    }
}

fn parse_discord_interaction(
    cmd: &serenity::model::application::CommandInteraction,
) -> ParsedInteraction {
    let mut options = HashMap::new();
    for opt in &cmd.data.options {
        if let serenity::model::application::CommandDataOptionValue::String(ref s) = opt.value {
            options.insert(opt.name.clone(), s.clone());
        }
    }

    ParsedInteraction {
        user_id: cmd.user.id.get(),
        channel_id: cmd.channel_id.get(),
        interaction_id: cmd.id.get(),
        interaction_token: cmd.token.clone(),
        command_name: cmd.data.name.clone(),
        options,
    }
}

// ---------------------------------------------------------------------------
// RealDiscordApi — thin wrapper around serenity Http
// ---------------------------------------------------------------------------

struct RealDiscordApi {
    http: Arc<serenity::http::Http>,
}

#[async_trait]
impl DiscordApi for RealDiscordApi {
    async fn send_channel_message(&self, channel_id: u64, text: &str) -> Result<(), String> {
        use serenity::builder::CreateMessage;
        use serenity::model::id::ChannelId;

        ChannelId::new(channel_id)
            .send_message(&*self.http, CreateMessage::new().content(text))
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn send_followup(&self, interaction_token: &str, text: &str) -> Result<(), String> {
        use serenity::builder::{Builder, CreateInteractionResponseFollowup};

        CreateInteractionResponseFollowup::new()
            .content(text)
            .execute(&*self.http, (None, interaction_token))
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn defer_interaction(
        &self,
        interaction_id: u64,
        interaction_token: &str,
    ) -> Result<(), String> {
        use serenity::builder::{
            Builder, CreateInteractionResponse, CreateInteractionResponseMessage,
        };
        use serenity::model::id::InteractionId;

        CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new())
            .execute(
                &*self.http,
                (InteractionId::new(interaction_id), interaction_token),
            )
            .await
            .map_err(|e| e.to_string())
    }

    async fn send_typing(&self, channel_id: u64) -> Result<(), String> {
        use serenity::model::id::ChannelId;

        ChannelId::new(channel_id)
            .broadcast_typing(&*self.http)
            .await
            .map_err(|e| e.to_string())
    }

    async fn download_attachment(&self, url: &str, destination: &Path) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;

        let response = reqwest::get(url)
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

    async fn register_commands(&self, commands: Vec<SlashCommandDef>) -> Result<(), String> {
        use serenity::builder::{CreateCommand, CreateCommandOption};
        use serenity::model::application::{Command, CommandOptionType};

        let create_commands: Vec<CreateCommand> = commands
            .into_iter()
            .map(|cmd| {
                let mut builder = CreateCommand::new(&cmd.name).description(&cmd.description);
                for opt in cmd.options {
                    builder = builder.add_option(
                        CreateCommandOption::new(
                            CommandOptionType::String,
                            &opt.name,
                            &opt.description,
                        )
                        .required(opt.required),
                    );
                }
                builder
            })
            .collect();

        Command::set_global_commands(&self.http, create_commands)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Serenity EventHandler
// ---------------------------------------------------------------------------

struct Handler {
    config: DiscordConfig,
    handler: MessageHandler,
    files_dir: PathBuf,
    interaction_store: InteractionStore,
    bot_user_id: tokio::sync::OnceCell<u64>,
    api: Arc<dyn DiscordApi>,
    http: Arc<serenity::http::Http>,
}

#[serenity::async_trait]
impl serenity::client::EventHandler for Handler {
    async fn ready(
        &self,
        _ctx: serenity::client::Context,
        ready: serenity::model::gateway::Ready,
    ) {
        let bot_id = ready.user.id.get();
        let _ = self.bot_user_id.set(bot_id);
        info!(
            "Discord bot ready as {} (ID: {})",
            ready.user.name, bot_id
        );

        self.http.set_application_id(ready.application.id);

        if let Err(e) = self.api.register_commands(slash_commands()).await {
            warn!("Failed to register slash commands: {e}");
        }
    }

    async fn message(
        &self,
        _ctx: serenity::client::Context,
        msg: serenity::model::channel::Message,
    ) {
        let Some(&bot_user_id) = self.bot_user_id.get() else {
            return;
        };

        let parsed = parse_discord_message(&msg, bot_user_id);
        if !should_activate(&parsed) {
            return;
        }

        // Spawn into a separate task so the shard runner can continue
        // processing gateway events (including heartbeats) without blocking.
        let config = self.config.clone();
        let api = self.api.clone();
        let handler = self.handler.clone();
        let files_dir = self.files_dir.clone();
        tokio::spawn(async move {
            process_parsed_message(parsed, &config, api.as_ref(), &handler, &files_dir).await;
        });
    }

    async fn interaction_create(
        &self,
        _ctx: serenity::client::Context,
        interaction: serenity::model::application::Interaction,
    ) {
        let serenity::model::application::Interaction::Command(cmd) = interaction else {
            return;
        };

        let parsed = parse_discord_interaction(&cmd);

        // Spawn into a separate task so the shard runner can continue
        // processing gateway events (including heartbeats) without blocking.
        let config = self.config.clone();
        let api = self.api.clone();
        let handler = self.handler.clone();
        let interaction_store = self.interaction_store.clone();
        tokio::spawn(async move {
            process_parsed_interaction(
                parsed,
                &config,
                api.as_ref(),
                &handler,
                &interaction_store,
            )
            .await;
        });
    }
}

// ---------------------------------------------------------------------------
// DiscordAdapter
// ---------------------------------------------------------------------------

pub struct DiscordAdapter {
    config: DiscordConfig,
    api: Option<Arc<dyn DiscordApi>>,
    handler: Option<MessageHandler>,
    files_dir: PathBuf,
    running: bool,
    client_handle: Option<JoinHandle<()>>,
    interaction_store: InteractionStore,
}

impl DiscordAdapter {
    pub fn new(config: DiscordConfig, files_dir: PathBuf) -> Self {
        Self {
            config,
            api: None,
            handler: None,
            files_dir,
            running: false,
            client_handle: None,
            interaction_store: Arc::new(TokioMutex::new(HashMap::new())),
        }
    }

    #[cfg(test)]
    fn with_api(config: DiscordConfig, files_dir: PathBuf, api: Arc<dyn DiscordApi>) -> Self {
        Self {
            config,
            api: Some(api),
            handler: None,
            files_dir,
            running: false,
            client_handle: None,
            interaction_store: Arc::new(TokioMutex::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl PlatformAdapter for DiscordAdapter {
    async fn start(&mut self) -> Result<(), AdapterError> {
        if self.running {
            return Err(AdapterError::AlreadyStarted);
        }

        let handler = self
            .handler
            .clone()
            .expect("message handler must be set before start");

        if self.api.is_none() {
            let intents = serenity::model::gateway::GatewayIntents::GUILD_MESSAGES
                | serenity::model::gateway::GatewayIntents::DIRECT_MESSAGES
                | serenity::model::gateway::GatewayIntents::MESSAGE_CONTENT;

            let http = Arc::new(serenity::http::Http::new(&self.config.token));
            let api: Arc<dyn DiscordApi> = Arc::new(RealDiscordApi {
                http: http.clone(),
            });
            self.api = Some(api.clone());

            let event_handler = Handler {
                config: self.config.clone(),
                handler,
                files_dir: self.files_dir.clone(),
                interaction_store: self.interaction_store.clone(),
                bot_user_id: tokio::sync::OnceCell::new(),
                api,
                http,
            };

            let token = self.config.token.clone();
            let handle = tokio::spawn(async move {
                let mut client = serenity::Client::builder(&token, intents)
                    .event_handler(event_handler)
                    .await
                    .expect("Failed to create Discord client");

                if let Err(e) = client.start().await {
                    error!("Discord client error: {e}");
                }
            });

            self.client_handle = Some(handle);
        }

        self.running = true;
        info!("Discord adapter started");
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), AdapterError> {
        if !self.running {
            return Err(AdapterError::NotStarted);
        }

        if let Some(handle) = self.client_handle.take() {
            handle.abort();
        }

        self.running = false;
        info!("Discord adapter stopped");
        Ok(())
    }

    async fn send_message(&self, chat_id: &str, text: &str) -> Result<(), AdapterError> {
        let api = self.api.as_ref().ok_or(AdapterError::NotStarted)?;

        let channel_id: u64 = chat_id
            .parse()
            .map_err(|e| AdapterError::SendFailed(format!("invalid chat_id: {e}")))?;

        let converted = to_discord(text);
        let chunks = split_message(&converted, 2000);

        let token = {
            let mut store = self.interaction_store.lock().await;
            store.remove(chat_id).and_then(|(token, created)| {
                if created.elapsed() < std::time::Duration::from_secs(900) {
                    Some(token)
                } else {
                    None
                }
            })
        };

        for (i, chunk) in chunks.iter().enumerate() {
            if i == 0 {
                if let Some(ref token) = token {
                    match api.send_followup(token, chunk).await {
                        Ok(()) => continue,
                        Err(e) => {
                            warn!("Followup failed, falling back to channel message: {e}");
                        }
                    }
                }
            }

            api.send_channel_message(channel_id, chunk)
                .await
                .map_err(AdapterError::SendFailed)?;
        }

        Ok(())
    }

    async fn send_typing(&self, chat_id: &str) -> Result<(), AdapterError> {
        let api = self.api.as_ref().ok_or(AdapterError::NotStarted)?;

        let channel_id: u64 = chat_id
            .parse()
            .map_err(|e| AdapterError::PlatformError(format!("invalid chat_id: {e}")))?;

        api.send_typing(channel_id)
            .await
            .map_err(AdapterError::PlatformError)
    }

    fn set_message_handler(&mut self, handler: MessageHandler) {
        self.handler = Some(handler);
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

    fn make_config() -> DiscordConfig {
        DiscordConfig {
            token: "test-token".to_string(),
            allowed_users: vec![111, 222],
            unauthorized_response: None,
        }
    }

    fn make_config_with_unauth_response() -> DiscordConfig {
        DiscordConfig {
            token: "test-token".to_string(),
            allowed_users: vec![111],
            unauthorized_response: Some("Not authorized".to_string()),
        }
    }

    fn make_parsed_dm(user_id: u64, channel_id: u64, content: &str) -> ParsedDiscordMessage {
        ParsedDiscordMessage {
            user_id,
            bot_user_id: 999,
            channel_id,
            guild_id: None,
            content: content.to_string(),
            author_is_bot: false,
            mentions_bot: false,
            is_reply_to_bot: false,
            attachments: vec![],
        }
    }

    fn make_parsed_channel(
        user_id: u64,
        channel_id: u64,
        guild_id: u64,
        content: &str,
    ) -> ParsedDiscordMessage {
        ParsedDiscordMessage {
            user_id,
            bot_user_id: 999,
            channel_id,
            guild_id: Some(guild_id),
            content: content.to_string(),
            author_is_bot: false,
            mentions_bot: false,
            is_reply_to_bot: false,
            attachments: vec![],
        }
    }

    fn make_interaction(
        user_id: u64,
        channel_id: u64,
        command: &str,
        options: HashMap<String, String>,
    ) -> ParsedInteraction {
        ParsedInteraction {
            user_id,
            channel_id,
            interaction_id: 12345,
            interaction_token: "test-token-abc".to_string(),
            command_name: command.to_string(),
            options,
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
    // should_activate tests
    // -----------------------------------------------------------------------

    mod test_should_activate {
        use super::*;

        #[test]
        fn dm_activates() {
            let msg = make_parsed_dm(111, 42, "hello");
            assert!(should_activate(&msg));
        }

        #[test]
        fn mention_in_channel_activates() {
            let mut msg = make_parsed_channel(111, 42, 1, "hello");
            msg.mentions_bot = true;
            assert!(should_activate(&msg));
        }

        #[test]
        fn reply_to_bot_activates() {
            let mut msg = make_parsed_channel(111, 42, 1, "hello");
            msg.is_reply_to_bot = true;
            assert!(should_activate(&msg));
        }

        #[test]
        fn regular_channel_message_does_not_activate() {
            let msg = make_parsed_channel(111, 42, 1, "hello");
            assert!(!should_activate(&msg));
        }

        #[test]
        fn bot_message_does_not_activate() {
            let mut msg = make_parsed_dm(111, 42, "hello");
            msg.author_is_bot = true;
            assert!(!should_activate(&msg));
        }

        #[test]
        fn bot_channel_message_does_not_activate() {
            let mut msg = make_parsed_channel(111, 42, 1, "hello");
            msg.author_is_bot = true;
            msg.mentions_bot = true;
            assert!(!should_activate(&msg));
        }
    }

    // -----------------------------------------------------------------------
    // strip_bot_mention tests
    // -----------------------------------------------------------------------

    mod test_strip_bot_mention {
        use super::*;

        #[test]
        fn strips_leading_mention() {
            assert_eq!(strip_bot_mention("<@123> hello", 123), "hello");
        }

        #[test]
        fn strips_middle_mention() {
            assert_eq!(strip_bot_mention("hello <@123> world", 123), "hello world");
        }

        #[test]
        fn no_mention_unchanged() {
            assert_eq!(strip_bot_mention("hello world", 123), "hello world");
        }

        #[test]
        fn only_mention_returns_empty() {
            assert_eq!(strip_bot_mention("<@123>", 123), "");
        }

        #[test]
        fn strips_nickname_mention() {
            assert_eq!(strip_bot_mention("<@!123> hello", 123), "hello");
        }
    }

    // -----------------------------------------------------------------------
    // classify_attachment tests
    // -----------------------------------------------------------------------

    mod test_classify_attachment {
        use super::*;

        #[test]
        fn image_jpeg() {
            assert_eq!(
                classify_attachment(&Some("image/jpeg".into())),
                AttachmentCategory::Image
            );
        }

        #[test]
        fn image_png() {
            assert_eq!(
                classify_attachment(&Some("image/png".into())),
                AttachmentCategory::Image
            );
        }

        #[test]
        fn audio_mpeg() {
            assert_eq!(
                classify_attachment(&Some("audio/mpeg".into())),
                AttachmentCategory::Audio
            );
        }

        #[test]
        fn audio_ogg() {
            assert_eq!(
                classify_attachment(&Some("audio/ogg".into())),
                AttachmentCategory::Audio
            );
        }

        #[test]
        fn application_pdf_is_other() {
            assert_eq!(
                classify_attachment(&Some("application/pdf".into())),
                AttachmentCategory::Other
            );
        }

        #[test]
        fn none_is_other() {
            assert_eq!(classify_attachment(&None), AttachmentCategory::Other);
        }
    }

    // -----------------------------------------------------------------------
    // slash_commands tests
    // -----------------------------------------------------------------------

    mod test_slash_commands {
        use super::*;

        #[test]
        fn returns_nine_commands() {
            assert_eq!(slash_commands().len(), 9);
        }

        #[test]
        fn models_has_one_option() {
            let cmds = slash_commands();
            let models = cmds.iter().find(|c| c.name == "models").unwrap();
            assert_eq!(models.options.len(), 1);
            assert_eq!(models.options[0].name, "model");
        }

        #[test]
        fn skill_has_two_options() {
            let cmds = slash_commands();
            let skill = cmds.iter().find(|c| c.name == "skill").unwrap();
            assert_eq!(skill.options.len(), 2);
            assert_eq!(skill.options[0].name, "name");
            assert_eq!(skill.options[1].name, "args");
        }

        #[test]
        fn history_has_one_option() {
            let cmds = slash_commands();
            let history = cmds.iter().find(|c| c.name == "history").unwrap();
            assert_eq!(history.options.len(), 1);
            assert_eq!(history.options[0].name, "query");
        }

        #[test]
        fn parameterless_commands_have_no_options() {
            let cmds = slash_commands();
            for name in &["new", "context", "restart", "status", "heartbeat", "help"] {
                let cmd = cmds.iter().find(|c| c.name == *name).unwrap();
                assert!(
                    cmd.options.is_empty(),
                    "{name} should have no options but has {}",
                    cmd.options.len()
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // interaction_to_command_text tests
    // -----------------------------------------------------------------------

    mod test_interaction_to_command_text {
        use super::*;

        #[test]
        fn parameterless_command() {
            let i = make_interaction(111, 42, "new", HashMap::new());
            assert_eq!(interaction_to_command_text(&i), "/new");
        }

        #[test]
        fn models_with_arg() {
            let mut opts = HashMap::new();
            opts.insert("model".into(), "sonnet".into());
            let i = make_interaction(111, 42, "models", opts);
            assert_eq!(interaction_to_command_text(&i), "/models sonnet");
        }

        #[test]
        fn models_without_arg() {
            let i = make_interaction(111, 42, "models", HashMap::new());
            assert_eq!(interaction_to_command_text(&i), "/models");
        }

        #[test]
        fn skill_with_name_and_args() {
            let mut opts = HashMap::new();
            opts.insert("name".into(), "test".into());
            opts.insert("args".into(), "foo".into());
            let i = make_interaction(111, 42, "skill", opts);
            assert_eq!(interaction_to_command_text(&i), "/skill test foo");
        }

        #[test]
        fn skill_with_name_only() {
            let mut opts = HashMap::new();
            opts.insert("name".into(), "test".into());
            let i = make_interaction(111, 42, "skill", opts);
            assert_eq!(interaction_to_command_text(&i), "/skill test");
        }

        #[test]
        fn skill_without_args() {
            let i = make_interaction(111, 42, "skill", HashMap::new());
            assert_eq!(interaction_to_command_text(&i), "/skill");
        }

        #[test]
        fn history_with_query() {
            let mut opts = HashMap::new();
            opts.insert("query".into(), "search term".into());
            let i = make_interaction(111, 42, "history", opts);
            assert_eq!(interaction_to_command_text(&i), "/history search term");
        }

        #[test]
        fn history_without_query() {
            let i = make_interaction(111, 42, "history", HashMap::new());
            assert_eq!(interaction_to_command_text(&i), "/history");
        }
    }

    // -----------------------------------------------------------------------
    // download_attachment_to_path tests
    // -----------------------------------------------------------------------

    mod test_download {
        use super::*;
        use tempfile::TempDir;

        #[tokio::test]
        async fn creates_dir_and_downloads_file() {
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_download_attachment()
                .returning(|_, dest| {
                    std::fs::create_dir_all(dest.parent().unwrap()).ok();
                    std::fs::write(dest, b"file bytes").unwrap();
                    Ok(())
                });

            let tmp = TempDir::new().unwrap();
            let att = AttachmentInfo {
                url: "https://cdn.discord.com/file.txt".into(),
                filename: "report.txt".into(),
                content_type: None,
            };

            let result =
                download_attachment_to_path(&mock_api, &att, tmp.path(), 42, &[]).await;
            assert!(result.is_some());
            let (path, name) = result.unwrap();
            assert_eq!(name, "report.txt");
            assert!(path.exists());
            assert!(path
                .to_str()
                .unwrap()
                .contains("submitted-files/discord-42"));
        }

        #[tokio::test]
        async fn sanitizes_filename() {
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_download_attachment()
                .returning(|_, dest| {
                    std::fs::create_dir_all(dest.parent().unwrap()).ok();
                    std::fs::write(dest, b"data").unwrap();
                    Ok(())
                });

            let tmp = TempDir::new().unwrap();
            let att = AttachmentInfo {
                url: "https://cdn.discord.com/file".into(),
                filename: "my file (1).pdf".into(),
                content_type: None,
            };

            let result =
                download_attachment_to_path(&mock_api, &att, tmp.path(), 42, &[]).await;
            let (_path, name) = result.unwrap();
            assert_eq!(name, "my_file__1_.pdf");
        }

        #[tokio::test]
        async fn download_failure_returns_none() {
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_download_attachment()
                .returning(|_, _| Err("download failed".to_string()));

            let tmp = TempDir::new().unwrap();
            let att = AttachmentInfo {
                url: "https://cdn.discord.com/bad".into(),
                filename: "file.txt".into(),
                content_type: None,
            };

            let result =
                download_attachment_to_path(&mock_api, &att, tmp.path(), 42, &[]).await;
            assert!(result.is_none());
        }
    }

    // -----------------------------------------------------------------------
    // process_parsed_message tests
    // -----------------------------------------------------------------------

    mod test_process_parsed_message {
        use super::*;
        use tempfile::TempDir;

        #[tokio::test]
        async fn dm_text_produces_correct_incoming() {
            let config = make_config();
            let mock_api = MockDiscordApi::new();
            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let msg = make_parsed_dm(111, 42, "hello world");
            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].platform, "discord");
            assert_eq!(msgs[0].user_id, "111");
            assert_eq!(msgs[0].chat_id, "42");
            assert_eq!(msgs[0].text.as_deref(), Some("hello world"));
            assert!(msgs[0].image_paths.is_empty());
            assert!(msgs[0].audio_paths.is_empty());
            assert!(msgs[0].file_paths.is_empty());
            assert!(msgs[0].file_names.is_empty());
        }

        #[tokio::test]
        async fn mention_activates_and_strips() {
            let config = make_config();
            let mock_api = MockDiscordApi::new();
            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let mut msg = make_parsed_channel(111, 42, 1, "<@999> hello");
            msg.mentions_bot = true;
            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].text.as_deref(), Some("hello"));
        }

        #[tokio::test]
        async fn reply_to_bot_activates() {
            let config = make_config();
            let mock_api = MockDiscordApi::new();
            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let mut msg = make_parsed_channel(111, 42, 1, "hello");
            msg.is_reply_to_bot = true;
            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs.len(), 1);
        }

        #[tokio::test]
        async fn regular_channel_message_ignored() {
            let config = make_config();
            let mock_api = MockDiscordApi::new();
            let called = Arc::new(AtomicBool::new(false));
            let called_clone = called.clone();
            let handler: MessageHandler = Arc::new(move |_| {
                called_clone.store(true, Ordering::SeqCst);
                Box::pin(async {})
            });
            let tmp = TempDir::new().unwrap();

            let msg = make_parsed_channel(111, 42, 1, "hello");
            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            assert!(!called.load(Ordering::SeqCst));
        }

        #[tokio::test]
        async fn bot_message_ignored() {
            let config = make_config();
            let mock_api = MockDiscordApi::new();
            let called = Arc::new(AtomicBool::new(false));
            let called_clone = called.clone();
            let handler: MessageHandler = Arc::new(move |_| {
                called_clone.store(true, Ordering::SeqCst);
                Box::pin(async {})
            });
            let tmp = TempDir::new().unwrap();

            let mut msg = make_parsed_dm(111, 42, "hello");
            msg.author_is_bot = true;
            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            assert!(!called.load(Ordering::SeqCst));
        }

        #[tokio::test]
        async fn unauthorized_user_handler_not_called() {
            let config = make_config();
            let mock_api = MockDiscordApi::new();
            let called = Arc::new(AtomicBool::new(false));
            let called_clone = called.clone();
            let handler: MessageHandler = Arc::new(move |_| {
                called_clone.store(true, Ordering::SeqCst);
                Box::pin(async {})
            });
            let tmp = TempDir::new().unwrap();

            let msg = make_parsed_dm(999, 42, "hello");
            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            assert!(!called.load(Ordering::SeqCst));
        }

        #[tokio::test]
        async fn unauthorized_user_with_response_sends_message() {
            let config = make_config_with_unauth_response();
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_send_channel_message()
                .withf(|channel_id, text| *channel_id == 42 && text == "Not authorized")
                .times(1)
                .returning(|_, _| Ok(()));

            let called = Arc::new(AtomicBool::new(false));
            let called_clone = called.clone();
            let handler: MessageHandler = Arc::new(move |_| {
                called_clone.store(true, Ordering::SeqCst);
                Box::pin(async {})
            });
            let tmp = TempDir::new().unwrap();

            let msg = make_parsed_dm(999, 42, "hello");
            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            assert!(!called.load(Ordering::SeqCst));
        }

        #[tokio::test]
        async fn image_attachment_to_image_paths() {
            let config = make_config();
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_download_attachment()
                .returning(|_, dest| {
                    std::fs::create_dir_all(dest.parent().unwrap()).ok();
                    std::fs::write(dest, b"img data").unwrap();
                    Ok(())
                });

            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let mut msg = make_parsed_dm(111, 42, "look at this");
            msg.attachments.push(AttachmentInfo {
                url: "https://cdn.discord.com/photo.jpg".into(),
                filename: "photo.jpg".into(),
                content_type: Some("image/jpeg".into()),
            });

            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs[0].image_paths.len(), 1);
            assert!(msgs[0].audio_paths.is_empty());
            assert!(msgs[0].file_paths.is_empty());
        }

        #[tokio::test]
        async fn audio_attachment_to_audio_paths() {
            let config = make_config();
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_download_attachment()
                .returning(|_, dest| {
                    std::fs::create_dir_all(dest.parent().unwrap()).ok();
                    std::fs::write(dest, b"audio data").unwrap();
                    Ok(())
                });

            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let mut msg = make_parsed_dm(111, 42, "listen to this");
            msg.attachments.push(AttachmentInfo {
                url: "https://cdn.discord.com/song.mp3".into(),
                filename: "song.mp3".into(),
                content_type: Some("audio/mpeg".into()),
            });

            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            let msgs = captured.lock().unwrap();
            assert!(msgs[0].image_paths.is_empty());
            assert_eq!(msgs[0].audio_paths.len(), 1);
            assert!(msgs[0].file_paths.is_empty());
        }

        #[tokio::test]
        async fn other_attachment_to_file_paths_with_sanitized_name() {
            let config = make_config();
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_download_attachment()
                .returning(|_, dest| {
                    std::fs::create_dir_all(dest.parent().unwrap()).ok();
                    std::fs::write(dest, b"pdf data").unwrap();
                    Ok(())
                });

            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let mut msg = make_parsed_dm(111, 42, "here's my file");
            msg.attachments.push(AttachmentInfo {
                url: "https://cdn.discord.com/doc.pdf".into(),
                filename: "my report (final).pdf".into(),
                content_type: Some("application/pdf".into()),
            });

            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            let msgs = captured.lock().unwrap();
            assert!(msgs[0].image_paths.is_empty());
            assert!(msgs[0].audio_paths.is_empty());
            assert_eq!(msgs[0].file_paths.len(), 1);
            assert_eq!(msgs[0].file_names.len(), 1);
            assert_eq!(msgs[0].file_names[0], "my_report__final_.pdf");
        }

        #[tokio::test]
        async fn multiple_attachments_classified_correctly() {
            let config = make_config();
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_download_attachment()
                .returning(|_, dest| {
                    std::fs::create_dir_all(dest.parent().unwrap()).ok();
                    std::fs::write(dest, b"data").unwrap();
                    Ok(())
                });

            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let mut msg = make_parsed_dm(111, 42, "multiple files");
            msg.attachments.push(AttachmentInfo {
                url: "https://cdn.discord.com/photo.jpg".into(),
                filename: "photo.jpg".into(),
                content_type: Some("image/jpeg".into()),
            });
            msg.attachments.push(AttachmentInfo {
                url: "https://cdn.discord.com/song.mp3".into(),
                filename: "song.mp3".into(),
                content_type: Some("audio/mpeg".into()),
            });
            msg.attachments.push(AttachmentInfo {
                url: "https://cdn.discord.com/doc.pdf".into(),
                filename: "doc.pdf".into(),
                content_type: Some("application/pdf".into()),
            });

            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs[0].image_paths.len(), 1);
            assert_eq!(msgs[0].audio_paths.len(), 1);
            assert_eq!(msgs[0].file_paths.len(), 1);
            assert_eq!(msgs[0].file_names.len(), 3);
        }

        #[tokio::test]
        async fn mention_stripped_from_content() {
            let config = make_config();
            let mock_api = MockDiscordApi::new();
            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let mut msg = make_parsed_channel(111, 42, 1, "<@999> do something");
            msg.mentions_bot = true;
            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs[0].text.as_deref(), Some("do something"));
        }

        #[tokio::test]
        async fn empty_content_after_mention_strip() {
            let config = make_config();
            let mock_api = MockDiscordApi::new();
            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let mut msg = make_parsed_channel(111, 42, 1, "<@999>");
            msg.mentions_bot = true;
            process_parsed_message(msg, &config, &mock_api, &handler, tmp.path()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs.len(), 1);
            assert!(msgs[0].text.is_none());
        }
    }

    // -----------------------------------------------------------------------
    // process_parsed_interaction tests
    // -----------------------------------------------------------------------

    mod test_process_parsed_interaction {
        use super::*;

        fn make_store() -> InteractionStore {
            Arc::new(TokioMutex::new(HashMap::new()))
        }

        #[tokio::test]
        async fn parameterless_command_produces_correct_incoming() {
            let config = make_config();
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_defer_interaction()
                .returning(|_, _| Ok(()));

            let (handler, captured) = capturing_handler();
            let store = make_store();

            let interaction = make_interaction(111, 42, "new", HashMap::new());
            process_parsed_interaction(interaction, &config, &mock_api, &handler, &store).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].platform, "discord");
            assert_eq!(msgs[0].text.as_deref(), Some("/new"));
        }

        #[tokio::test]
        async fn models_with_arg_produces_correct_text() {
            let config = make_config();
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_defer_interaction()
                .returning(|_, _| Ok(()));

            let (handler, captured) = capturing_handler();
            let store = make_store();

            let mut opts = HashMap::new();
            opts.insert("model".into(), "sonnet".into());
            let interaction = make_interaction(111, 42, "models", opts);
            process_parsed_interaction(interaction, &config, &mock_api, &handler, &store).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs[0].text.as_deref(), Some("/models sonnet"));
        }

        #[tokio::test]
        async fn skill_with_name_and_args_produces_correct_text() {
            let config = make_config();
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_defer_interaction()
                .returning(|_, _| Ok(()));

            let (handler, captured) = capturing_handler();
            let store = make_store();

            let mut opts = HashMap::new();
            opts.insert("name".into(), "test".into());
            opts.insert("args".into(), "foo".into());
            let interaction = make_interaction(111, 42, "skill", opts);
            process_parsed_interaction(interaction, &config, &mock_api, &handler, &store).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs[0].text.as_deref(), Some("/skill test foo"));
        }

        #[tokio::test]
        async fn history_with_query_produces_correct_text() {
            let config = make_config();
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_defer_interaction()
                .returning(|_, _| Ok(()));

            let (handler, captured) = capturing_handler();
            let store = make_store();

            let mut opts = HashMap::new();
            opts.insert("query".into(), "search term".into());
            let interaction = make_interaction(111, 42, "history", opts);
            process_parsed_interaction(interaction, &config, &mock_api, &handler, &store).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs[0].text.as_deref(), Some("/history search term"));
        }

        #[tokio::test]
        async fn defers_interaction_before_handler() {
            let config = make_config();
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_defer_interaction()
                .withf(|id, token| *id == 12345 && token == "test-token-abc")
                .times(1)
                .returning(|_, _| Ok(()));

            let (handler, _captured) = capturing_handler();
            let store = make_store();

            let interaction = make_interaction(111, 42, "new", HashMap::new());
            process_parsed_interaction(interaction, &config, &mock_api, &handler, &store).await;
        }

        #[tokio::test]
        async fn unauthorized_user_handler_not_called() {
            let config = make_config();
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_defer_interaction()
                .returning(|_, _| Ok(()));

            let called = Arc::new(AtomicBool::new(false));
            let called_clone = called.clone();
            let handler: MessageHandler = Arc::new(move |_| {
                called_clone.store(true, Ordering::SeqCst);
                Box::pin(async {})
            });
            let store = make_store();

            let interaction = make_interaction(999, 42, "new", HashMap::new());
            process_parsed_interaction(interaction, &config, &mock_api, &handler, &store).await;

            assert!(!called.load(Ordering::SeqCst));
        }

        #[tokio::test]
        async fn unauthorized_user_sends_followup() {
            let config = make_config_with_unauth_response();
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_defer_interaction()
                .returning(|_, _| Ok(()));
            mock_api
                .expect_send_followup()
                .withf(|token, text| token == "test-token-abc" && text == "Not authorized")
                .times(1)
                .returning(|_, _| Ok(()));

            let handler = noop_handler();
            let store = make_store();

            let interaction = make_interaction(999, 42, "new", HashMap::new());
            process_parsed_interaction(interaction, &config, &mock_api, &handler, &store).await;
        }

        #[tokio::test]
        async fn defer_failure_still_processes() {
            let config = make_config();
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_defer_interaction()
                .returning(|_, _| Err("defer failed".to_string()));

            let (handler, captured) = capturing_handler();
            let store = make_store();

            let interaction = make_interaction(111, 42, "new", HashMap::new());
            process_parsed_interaction(interaction, &config, &mock_api, &handler, &store).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs.len(), 1);
        }
    }

    // -----------------------------------------------------------------------
    // send_message + interaction routing tests
    // -----------------------------------------------------------------------

    mod test_send_message {
        use super::*;
        use tempfile::TempDir;

        #[tokio::test]
        async fn sends_text_with_discord_conversion() {
            let config = make_config();
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_send_channel_message()
                .withf(|channel_id, text| {
                    // to_discord converts ## Header → **Header**
                    *channel_id == 42 && text.contains("**Header**")
                })
                .times(1)
                .returning(|_, _| Ok(()));

            let tmp = TempDir::new().unwrap();
            let mut adapter =
                DiscordAdapter::with_api(config, tmp.path().to_path_buf(), Arc::new(mock_api));
            adapter.running = true;
            adapter.set_message_handler(noop_handler());

            let result = adapter.send_message("42", "## Header").await;
            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn splits_long_messages() {
            let config = make_config();
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_send_channel_message()
                .returning(|_, _| Ok(()));

            let tmp = TempDir::new().unwrap();
            let mut adapter =
                DiscordAdapter::with_api(config, tmp.path().to_path_buf(), Arc::new(mock_api));
            adapter.running = true;
            adapter.set_message_handler(noop_handler());

            let long_text = "a".repeat(3000);
            let result = adapter.send_message("42", &long_text).await;
            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn active_interaction_uses_followup() {
            let config = make_config();
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_send_followup()
                .withf(|token, _text| token == "test-followup-token")
                .times(1)
                .returning(|_, _| Ok(()));
            // send_channel_message should NOT be called
            mock_api.expect_send_channel_message().never();

            let tmp = TempDir::new().unwrap();
            let api = Arc::new(mock_api);
            let mut adapter =
                DiscordAdapter::with_api(config, tmp.path().to_path_buf(), api);
            adapter.running = true;
            adapter.set_message_handler(noop_handler());

            // Pre-populate interaction store
            {
                let mut store = adapter.interaction_store.lock().await;
                store.insert("42".to_string(), ("test-followup-token".to_string(), Instant::now()));
            }

            let result = adapter.send_message("42", "response").await;
            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn followup_failure_falls_back_to_channel() {
            let config = make_config();
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_send_followup()
                .returning(|_, _| Err("followup expired".to_string()));
            mock_api
                .expect_send_channel_message()
                .times(1)
                .returning(|_, _| Ok(()));

            let tmp = TempDir::new().unwrap();
            let api = Arc::new(mock_api);
            let mut adapter =
                DiscordAdapter::with_api(config, tmp.path().to_path_buf(), api);
            adapter.running = true;
            adapter.set_message_handler(noop_handler());

            {
                let mut store = adapter.interaction_store.lock().await;
                store.insert("42".to_string(), ("expired-token".to_string(), Instant::now()));
            }

            let result = adapter.send_message("42", "response").await;
            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn api_error_returns_send_failed() {
            let config = make_config();
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_send_channel_message()
                .returning(|_, _| Err("API timeout".to_string()));

            let tmp = TempDir::new().unwrap();
            let mut adapter =
                DiscordAdapter::with_api(config, tmp.path().to_path_buf(), Arc::new(mock_api));
            adapter.running = true;
            adapter.set_message_handler(noop_handler());

            let result = adapter.send_message("42", "hello").await;
            assert!(matches!(result, Err(AdapterError::SendFailed(_))));
        }

        #[tokio::test]
        async fn invalid_chat_id_returns_send_failed() {
            let config = make_config();
            let mock_api = MockDiscordApi::new();
            let tmp = TempDir::new().unwrap();
            let mut adapter =
                DiscordAdapter::with_api(config, tmp.path().to_path_buf(), Arc::new(mock_api));
            adapter.running = true;
            adapter.set_message_handler(noop_handler());

            let result = adapter.send_message("not_a_number", "hello").await;
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
        async fn calls_typing_with_correct_channel() {
            let config = make_config();
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_send_typing()
                .withf(|channel_id| *channel_id == 42)
                .times(1)
                .returning(|_| Ok(()));

            let tmp = TempDir::new().unwrap();
            let mut adapter =
                DiscordAdapter::with_api(config, tmp.path().to_path_buf(), Arc::new(mock_api));
            adapter.running = true;
            adapter.set_message_handler(noop_handler());

            let result = adapter.send_typing("42").await;
            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn api_error_returns_platform_error() {
            let config = make_config();
            let mut mock_api = MockDiscordApi::new();
            mock_api
                .expect_send_typing()
                .returning(|_| Err("connection error".to_string()));

            let tmp = TempDir::new().unwrap();
            let mut adapter =
                DiscordAdapter::with_api(config, tmp.path().to_path_buf(), Arc::new(mock_api));
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
            let mock_api = MockDiscordApi::new();
            let mut adapter =
                DiscordAdapter::with_api(config, tmp.path().to_path_buf(), Arc::new(mock_api));
            adapter.running = true;
            adapter.set_message_handler(noop_handler());

            let result = adapter.start().await;
            assert!(matches!(result, Err(AdapterError::AlreadyStarted)));
        }

        #[tokio::test]
        async fn stop_when_not_started_returns_not_started() {
            let config = make_config();
            let tmp = TempDir::new().unwrap();
            let mut adapter = DiscordAdapter::new(config, tmp.path().to_path_buf());

            let result = adapter.stop().await;
            assert!(matches!(result, Err(AdapterError::NotStarted)));
        }

        #[tokio::test]
        async fn send_message_before_start_returns_not_started() {
            let config = make_config();
            let tmp = TempDir::new().unwrap();
            let adapter = DiscordAdapter::new(config, tmp.path().to_path_buf());

            let result = adapter.send_message("42", "hello").await;
            assert!(matches!(result, Err(AdapterError::NotStarted)));
        }

        #[tokio::test]
        async fn send_typing_before_start_returns_not_started() {
            let config = make_config();
            let tmp = TempDir::new().unwrap();
            let adapter = DiscordAdapter::new(config, tmp.path().to_path_buf());

            let result = adapter.send_typing("42").await;
            assert!(matches!(result, Err(AdapterError::NotStarted)));
        }
    }
}
