use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use uuid::Uuid;

use stubert::adapters::discord::DiscordAdapter;
use stubert::adapters::telegram::TelegramAdapter;
use stubert::config::load_config;
use stubert::gateway::claude_cli::{call_claude, format_context_summary, resolve_model, ClaudeCallParams};
use stubert::gateway::commands::HeartbeatTrigger;
use stubert::gateway::core::{Gateway, RealClaudeCaller};
use stubert::gateway::heartbeat::HeartbeatRunner;
use stubert::gateway::history::HistoryWriter;
use stubert::gateway::scheduler::{format_schedule_list, load_schedules};
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
        /// Path to the runtime directory (auto-detected from binary location if omitted)
        #[arg(long)]
        runtime_dir: Option<PathBuf>,
    },
    /// Restart the running service
    Restart,
    /// Show service status
    Status,
    /// Rebuild (cargo build --release) then restart the service
    Rebuild,
    /// Show configured scheduled tasks
    Schedules {
        /// Path to the runtime directory (auto-detected from binary location if omitted)
        #[arg(long)]
        runtime_dir: Option<PathBuf>,
    },
    /// Query context window usage for a session
    Context {
        /// The Claude session ID to query
        session_id: String,

        /// Path to the runtime directory (auto-detected from binary location if omitted)
        #[arg(long)]
        runtime_dir: Option<PathBuf>,

        /// Timeout in seconds
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },
    /// Search the web using an isolated Claude agent
    Search {
        /// The search query
        #[arg(trailing_var_arg = true, required = true)]
        query: Vec<String>,

        /// Model to use (sonnet, opus, haiku, or full model ID)
        #[arg(long, default_value = "sonnet")]
        model: String,

        /// Timeout in seconds
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Command::Run { runtime_dir: None }) {
        Command::Run { runtime_dir } => match resolve_runtime_dir(runtime_dir) {
            Ok(dir) => {
                run(dir).await;
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
        Command::Restart => restart(),
        Command::Status => status().await,
        Command::Rebuild => rebuild(),
        Command::Schedules { runtime_dir } => match resolve_runtime_dir(runtime_dir) {
            Ok(dir) => schedules(dir),
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
        Command::Context {
            session_id,
            runtime_dir,
            timeout,
        } => match resolve_runtime_dir(runtime_dir) {
            Ok(dir) => context(dir, session_id, timeout).await,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
        Command::Search {
            query,
            model,
            timeout,
        } => search(query, model, timeout).await,
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

fn resolve_runtime_dir(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(dir) = explicit {
        return Ok(dir);
    }
    // Auto-detect: <repo>/config/
    let dir = repo_dir()
        .map(|r| r.join("config"))
        .ok_or_else(|| "could not locate runtime dir from binary path".to_string())?;
    if dir.join("config.yaml").exists() {
        Ok(dir)
    } else {
        Err(format!(
            "auto-detected runtime dir {} has no config.yaml — pass --runtime-dir explicitly",
            dir.display()
        ))
    }
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

fn schedules(runtime_dir: PathBuf) -> ExitCode {
    std::env::set_current_dir(&runtime_dir).expect("failed to set working directory");
    dotenvy::dotenv().ok();

    let config = match load_config(Path::new("config.yaml")) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to load config: {e}");
            return ExitCode::FAILURE;
        }
    };

    let sched_config = match &config.scheduler {
        Some(sc) => sc,
        None => {
            eprintln!("No scheduler configured.");
            return ExitCode::SUCCESS;
        }
    };

    let schedules_path =
        PathBuf::from(&config.claude.working_directory).join(&sched_config.schedules_file);

    let tasks = match load_schedules(&schedules_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("failed to load schedules: {e}");
            return ExitCode::FAILURE;
        }
    };

    eprintln!("{}", format_schedule_list(&tasks));
    ExitCode::SUCCESS
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

const SEARCH_CLAUDE_MD: &str = "\
# Search Agent

You are a focused web search assistant. Your only job is to find information
on the web and present it clearly.

## Instructions

- Search the web for the user's query using WebSearch
- If a search result looks promising, use WebFetch to get more details
- Present a detailed, well-organized summary of your findings
- Always cite your sources with URLs
- If the query is ambiguous, search for the most likely interpretation
- Provide a direct answer — no preamble about what you're doing
";

async fn context(runtime_dir: PathBuf, session_id: String, timeout: u64) -> ExitCode {
    std::env::set_current_dir(&runtime_dir).expect("failed to set working directory");
    dotenvy::dotenv().ok();

    let config = match load_config(Path::new("config.yaml")) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to load config: {e}");
            return ExitCode::FAILURE;
        }
    };

    let params = ClaudeCallParams {
        prompt: "/context".to_string(),
        session_id,
        is_new_session: false,
        allowed_tools: None,
        add_dirs: None,
        model: None,
        append_system_prompt: None,
        env_file_path: config.claude.env_file_path.clone(),
        timeout_secs: timeout,
        working_directory: config.claude.working_directory.clone(),
        cli_path: config.claude.cli_path.clone(),
    };

    match call_claude(&params).await {
        Ok(response) => {
            eprintln!("{}", format_context_summary(&response.result));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("failed: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn search(query: Vec<String>, model: String, timeout: u64) -> ExitCode {
    let prompt = query.join(" ");
    eprintln!("stubert search: query={prompt:?} model={model} timeout={timeout}s");
    let session_id = Uuid::new_v4().to_string();
    let resolved_model = resolve_model(&model);

    // Create isolated temp directory with .claude/ to prevent inheriting parent settings
    let tmp_dir = std::env::temp_dir().join(format!("stubert-search-{session_id}"));
    if let Err(e) = std::fs::create_dir_all(tmp_dir.join(".claude")) {
        eprintln!("failed to create temp directory: {e}");
        return ExitCode::FAILURE;
    }
    if let Err(e) = std::fs::write(tmp_dir.join("CLAUDE.md"), SEARCH_CLAUDE_MD) {
        eprintln!("failed to write CLAUDE.md: {e}");
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return ExitCode::FAILURE;
    }

    let params = ClaudeCallParams {
        prompt,
        session_id,
        is_new_session: true,
        allowed_tools: Some(vec!["WebSearch".to_string(), "WebFetch".to_string()]),
        add_dirs: None,
        model: Some(resolved_model),
        append_system_prompt: None,
        env_file_path: String::new(),
        timeout_secs: timeout,
        working_directory: tmp_dir.to_str().unwrap_or("/tmp").to_string(),
        cli_path: "claude".to_string(),
    };

    let exit_code = match call_claude(&params).await {
        Ok(response) => {
            eprintln!(
                "stubert search: done (${:.4}, {}ms)",
                response.cost_usd, response.duration_ms
            );
            println!("{}", response.result);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("stubert search: failed: {e}");
            ExitCode::FAILURE
        }
    };

    let _ = std::fs::remove_dir_all(&tmp_dir);
    exit_code
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
