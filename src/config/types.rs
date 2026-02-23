use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct StubbertConfig {
    pub telegram: TelegramConfig,
    pub discord: DiscordConfig,
    pub claude: ClaudeConfig,
    pub sessions: SessionConfig,
    pub history: HistoryConfig,
    pub logging: LoggingConfig,
    pub heartbeat: HeartbeatConfig,
    pub health: HealthConfig,
    #[serde(default)]
    pub scheduler: Option<SchedulerConfig>,
    #[serde(default)]
    pub files: Option<FilesConfig>,
    #[serde(default)]
    pub gateway: Option<GatewayConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramConfig {
    pub token: String,
    pub allowed_users: Vec<u64>,
    pub unauthorized_response: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DiscordConfig {
    pub token: String,
    pub allowed_users: Vec<u64>,
    pub unauthorized_response: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClaudeConfig {
    pub cli_path: String,
    pub timeout_secs: u64,
    pub default_model: String,
    pub working_directory: String,
    pub env_file_path: String,
    pub allowed_tools: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub add_dirs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionConfig {
    pub timeout_minutes: u64,
    pub sessions_file: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HistoryConfig {
    pub base_dir: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoggingConfig {
    pub log_file: String,
    pub log_max_bytes: u64,
    pub log_backup_count: u32,
    pub level: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HeartbeatConfig {
    pub interval_minutes: u64,
    pub file: String,
    #[serde(default)]
    pub log_file: Option<String>,
    #[serde(default)]
    pub log_max_bytes: Option<u64>,
    #[serde(default)]
    pub log_backup_count: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HealthConfig {
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SchedulerConfig {
    pub schedules_file: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FilesConfig {
    pub cleanup_days: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    pub max_message_length: usize,
}
