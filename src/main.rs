use std::path::{Path, PathBuf};
use std::sync::Arc;

use stubert::adapters::discord::DiscordAdapter;
use stubert::adapters::telegram::TelegramAdapter;
use stubert::config::load_config;
use stubert::gateway::claude_cli::resolve_model;
use stubert::gateway::commands::HeartbeatTrigger;
use stubert::gateway::core::{Gateway, RealClaudeCaller};
use stubert::gateway::heartbeat::HeartbeatRunner;
use stubert::gateway::history::HistoryWriter;
use stubert::gateway::session::SessionManager;
use stubert::gateway::skills::SkillRegistry;
use stubert::logging::setup_logging;

fn parse_runtime_dir() -> PathBuf {
    let args: Vec<String> = std::env::args().collect();
    for i in 0..args.len() {
        if args[i] == "--runtime-dir" {
            if let Some(dir) = args.get(i + 1) {
                return PathBuf::from(dir);
            }
        }
    }
    PathBuf::from(".")
}

#[tokio::main]
async fn main() {
    let runtime_dir = parse_runtime_dir();
    std::env::set_current_dir(&runtime_dir).expect("failed to set working directory");

    dotenvy::dotenv().ok();

    let config = load_config(Path::new("config.yaml")).expect("failed to load config");

    setup_logging(&config.logging).expect("failed to setup logging");

    tracing::info!("stubert starting");

    // Component construction
    let session_manager = SessionManager::new(
        PathBuf::from(&config.sessions.sessions_file),
        config.sessions.timeout_minutes,
        resolve_model(&config.claude.default_model),
    );

    let history_writer = HistoryWriter::new(PathBuf::from(&config.history.base_dir));

    let skills_dir = PathBuf::from(&config.claude.working_directory).join(".claude/skills");
    let skill_registry = SkillRegistry::new(skills_dir);

    let claude_caller = Arc::new(RealClaudeCaller);

    // Heartbeat
    let heartbeat_runner = Arc::new(HeartbeatRunner::new(
        config.heartbeat.clone(),
        &config.claude,
        Arc::clone(&claude_caller) as Arc<_>,
    ));
    heartbeat_runner.start();
    let heartbeat_trigger: Option<Arc<dyn HeartbeatTrigger>> = Some(heartbeat_runner);

    // Adapters
    let files_dir =
        PathBuf::from(&config.claude.working_directory).join("submitted-files");

    let telegram = TelegramAdapter::new(config.telegram.clone(), files_dir.clone());
    let discord = DiscordAdapter::new(config.discord.clone(), files_dir);

    // Gateway
    let mut gateway = Gateway::new(
        config,
        session_manager,
        history_writer,
        claude_caller,
        None, // no transcriber
        skill_registry,
        heartbeat_trigger,
    );

    gateway.register_adapter("telegram", telegram).await;
    gateway.register_adapter("discord", discord).await;
    gateway.start().await;

    // Wait for shutdown signal
    shutdown_signal().await;

    tracing::info!("shutdown signal received");
    gateway.shutdown().await;
    tracing::info!("stubert stopped");
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler");

        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    ctrl_c.await.ok();
}
