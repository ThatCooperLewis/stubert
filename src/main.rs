use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};

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

const DEFAULT_HEALTH_PORT: u16 = 8484;

#[derive(Parser)]
#[command(name = "stubert", about = "Stubert AI agent service")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start the stubert service (default)
    Run {
        /// Path to the runtime directory
        #[arg(long, default_value = ".")]
        runtime_dir: PathBuf,
    },
    /// Restart the running service
    Restart,
    /// Show service status
    Status,
    /// Rebuild (cargo build --release) then restart the service
    Rebuild,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Command::Run {
        runtime_dir: PathBuf::from("."),
    }) {
        Command::Run { runtime_dir } => {
            run(runtime_dir).await;
            ExitCode::SUCCESS
        }
        Command::Restart => restart(),
        Command::Status => status().await,
        Command::Rebuild => rebuild(),
    }
}

fn find_service_pid() -> Option<u32> {
    let output = std::process::Command::new("pgrep")
        .args(["-f", "stubert run"])
        .output()
        .ok()?;

    let text = String::from_utf8_lossy(&output.stdout);
    let my_pid = std::process::id();

    // Return the first PID that isn't us
    text.lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .find(|&pid| pid != my_pid)
}

fn repo_dir() -> Option<PathBuf> {
    // Binary lives at <repo>/target/release/stubert
    let exe = std::env::current_exe().ok()?;
    let canonical = exe.canonicalize().ok()?;
    canonical.parent()?.parent()?.parent().map(PathBuf::from)
}

fn rebuild() -> ExitCode {
    let repo = match repo_dir() {
        Some(d) if d.join("Cargo.toml").exists() => d,
        _ => {
            eprintln!("could not locate stubert repo from binary path");
            return ExitCode::FAILURE;
        }
    };

    eprintln!("building release in {}...", repo.display());
    let build = std::process::Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(&repo)
        .status();

    match build {
        Ok(s) if s.success() => {
            eprintln!("build succeeded");
            restart()
        }
        Ok(s) => {
            eprintln!("build failed (exit {})", s.code().unwrap_or(1));
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("failed to run cargo: {e}");
            ExitCode::FAILURE
        }
    }
}

fn restart() -> ExitCode {
    match find_service_pid() {
        Some(pid) => {
            eprintln!("sending SIGTERM to stubert (pid {pid})...");
            let ret = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
            if ret == 0 {
                eprintln!("signal sent — systemd will restart the service");
                ExitCode::SUCCESS
            } else {
                eprintln!("failed to send signal to pid {pid}");
                ExitCode::FAILURE
            }
        }
        None => {
            eprintln!("stubert is not running");
            ExitCode::FAILURE
        }
    }
}

async fn status() -> ExitCode {
    let url = format!("http://127.0.0.1:{DEFAULT_HEALTH_PORT}/health");

    match reqwest::get(&url).await {
        Ok(resp) if resp.status().is_success() => {
            match resp.json::<serde_json::Value>().await {
                Ok(json) => {
                    let status = json["status"].as_str().unwrap_or("unknown");
                    let uptime = json["uptime_seconds"].as_u64().unwrap_or(0);
                    let sessions = json["active_sessions"].as_u64().unwrap_or(0);
                    let inflight = json["inflight_calls"].as_u64().unwrap_or(0);

                    let hours = uptime / 3600;
                    let mins = (uptime % 3600) / 60;
                    let secs = uptime % 60;

                    eprintln!("stubert: {status}");
                    eprintln!("  uptime:     {hours}h {mins}m {secs}s");
                    eprintln!("  sessions:   {sessions}");
                    eprintln!("  in-flight:  {inflight}");

                    if let Some(hb) = json["last_heartbeat"].as_str() {
                        eprintln!("  heartbeat:  {hb}");
                    }
                    if let Some(cron) = json["last_cron_execution"].as_str() {
                        eprintln!("  last cron:  {cron}");
                    }

                    ExitCode::SUCCESS
                }
                Err(_) => {
                    eprintln!("stubert: running (could not parse health response)");
                    ExitCode::SUCCESS
                }
            }
        }
        _ => {
            eprintln!("stubert: not running");
            ExitCode::FAILURE
        }
    }
}

async fn run(runtime_dir: PathBuf) {
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
