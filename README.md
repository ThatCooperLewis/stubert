# Stubert

Openclaw-inspired agent manager.

- Fewer security nightmares
- Uses your Claude Code subscription without breaking ToS
- Doesn't try to reinvent the wheel (uses built-in Claude features rather than rewriting them)

## Prerequisites

- Rust toolchain (rustc, cargo)
- A C compiler (`gcc`) and `pkg-config`
- Optional: `clippy` (`rustup component add clippy`)

## Build

```bash
cargo build
```

## Test

```bash
# Run all unit tests
cargo test

# Run tests for a specific module
cargo test --lib config
cargo test --lib gateway::claude_cli
cargo test --lib gateway::session
cargo test --lib gateway::history
cargo test --lib gateway::commands
cargo test --lib gateway::skills
cargo test --lib gateway::heartbeat
cargo test --lib adapters::telegram
cargo test --lib adapters
cargo test --lib gateway::core
cargo test --lib gateway::scheduler
cargo test --lib gateway::health
cargo test --lib logging
```

## Project Structure

```
src/
├── main.rs                  # Entry point
├── lib.rs                   # Module declarations
├── config/
│   ├── mod.rs               # load_config(), env var interpolation
│   └── types.rs             # Config structs (StubbertConfig + sub-configs)
├── adapters/
│   ├── mod.rs               # PlatformAdapter trait, IncomingMessage, AdapterError
│   ├── telegram.rs          # TelegramAdapter (teloxide long-polling, media downloads)
│   ├── discord.rs           # DiscordAdapter (serenity WebSocket, slash commands, media)
│   ├── markdown.rs          # to_telegram() (MarkdownV2), to_discord()
│   ├── message_split.rs     # split_message() (code-block-aware chunking)
│   └── sanitize.rs          # sanitize_filename() (path stripping, collision resolution)
├── gateway/
│   ├── mod.rs               # Module declarations
│   ├── core.rs              # Gateway orchestrator, message routing, consumer loop, lifecycle
│   ├── claude_cli.rs        # call_claude(), model aliasing, arg assembly
│   ├── commands.rs          # 9 slash commands, parse_command(), dispatch_command()
│   ├── skills.rs            # SkillRegistry, frontmatter parsing from .claude/skills/*.md
│   ├── health.rs            # HealthServer (HTTP health endpoint, runtime metrics)
│   ├── heartbeat.rs         # HeartbeatRunner (periodic monitoring loop, log rotation)
│   ├── scheduler.rs         # TaskScheduler (cron-based task execution, per-task logging)
│   ├── history.rs           # HistoryWriter (daily transcripts, search)
│   └── session.rs           # Session + SessionManager (message queue, persistence, inactivity timers)
└── logging.rs               # setup_logging(), TelegramTransientFilter
```

## Configuration

Stubert loads config from a YAML file with `${ENV_VAR}` interpolation:

```yaml
telegram:
  token: "${TELEGRAM_BOT_TOKEN}"
  allowed_users: [123456789]

discord:
  token: "${DISCORD_BOT_TOKEN}"
  allowed_users: [987654321]

claude:
  cli_path: "claude"
  timeout_secs: 300
  default_model: "sonnet"
  working_directory: "."
  env_file_path: ".env"
  allowed_tools:
    telegram: ["Bash", "Read", "Write", "Edit", "Glob", "Grep"]
    discord: ["Read"]
  add_dirs: []

sessions:
  timeout_minutes: 60
  sessions_file: "sessions.json"

history:
  base_dir: "history"

logging:
  log_file: "logs/stubert.log"
  log_max_bytes: 10000000
  log_backup_count: 5
  level: "INFO"

heartbeat:
  interval_minutes: 30
  file: "HEARTBEAT.md"
  allowed_tools: ["Bash(read-only)", "Read", "Glob", "Grep"]
  log_file: "logs/heartbeat.log"    # optional
  log_max_bytes: 5000000            # optional, default 5MB
  log_backup_count: 3               # optional, default 3

health:
  port: 8484

scheduler:                              # optional
  schedules_file: "schedules.yaml"
  job_log_dir: "logs/cron"              # optional, default "logs/cron"
  job_log_max_bytes: 5242880            # optional, default 5MB
  job_log_backup_count: 3              # optional, default 3
```

## Slash Commands

| Command | Description |
|---------|-------------|
| `/new` | Start a fresh session |
| `/context` | Check context window usage |
| `/restart` | Restart the bot |
| `/models [alias]` | List or switch models (sonnet, opus, haiku) |
| `/skill [name] [args]` | List or invoke a skill |
| `/history <query>` | Search conversation history |
| `/status` | Show bot status (uptime, sessions, model) |
| `/heartbeat` | Trigger a heartbeat check |
| `/help` | Show command listing |

## Skills

Skills are prompt templates discovered from `.claude/skills/*.md` files. Each file uses YAML frontmatter:

```markdown
---
name: trello
description: Manage Trello boards
allowed_tools:
  - Bash
  - Read
add_dirs:
  - /extra/dir
---
Create a Trello card with the given details.
```

- `name` (required) — skill identifier used with `/skill <name>`
- `description` — shown when listing skills with `/skill`
- `allowed_tools` — overrides platform default tools for this skill
- `add_dirs` — additional directories to pass to Claude CLI

## Heartbeat

The heartbeat system runs a periodic monitoring loop. Every `interval_minutes`, Stubert reads the `HEARTBEAT.md` file, filters out `#` comment lines and blank lines, and sends the remaining text as a prompt to an ephemeral Claude CLI session. Each execution gets a fresh UUID session (never resumed).

- **Overlap protection:** If a previous heartbeat is still running, the tick is skipped.
- **Manual trigger:** `/heartbeat` runs an immediate check outside the normal schedule.
- **Logging:** When `log_file` is configured, results are appended with timestamps and status (`OK`/`FAIL`/`SKIPPED`). Log rotation shifts files when `log_max_bytes` is exceeded, keeping up to `log_backup_count` backups.
- **`allowed_tools`:** Controls which tools the heartbeat session can use. Defaults to read-only tools (`Bash(read-only)`, `Read`, `Glob`, `Grep`).

## Scheduler

The scheduler runs statically defined tasks on cron schedules. Tasks are defined in `schedules.yaml`:

```yaml
tasks:
  morning-summary:
    schedule: "0 8 * * *"
    prompt: "Summarize any notable system events from the last 24 hours."
    allowed_tools: ["Bash(read-only)", "Read", "Glob", "Grep"]
    on_failure: notify
    notify:
      platform: telegram
      chat_id: "123456789"

  weekly-cleanup:
    schedule: "0 3 * * 0"
    prompt: "Check disk usage and report any directories over 1GB in /tmp."
    allowed_tools: ["Bash(read-only)", "Read"]
    add_dirs: ["/tmp"]
```

- **Ephemeral sessions:** Each execution gets a fresh UUID (never resumed).
- **Concurrency control:** Max 1 concurrent instance per task — overlapping triggers are skipped.
- **Notifications:** When `notify` is configured, success results are sent to the specified platform/chat. Failure notifications only send when `on_failure: notify` (default is `log`).
- **Per-task logging:** Each task gets its own log file in `job_log_dir` with size-based rotation.
- **Cron format:** Standard 5-field — `minute hour day month day_of_week`. Validated at startup.

## Health Endpoint

Stubert exposes an HTTP health endpoint at `GET /health` on the configured port (default 8484). The endpoint returns JSON with runtime metrics:

```json
{
  "status": "ok",
  "uptime_seconds": 3600,
  "active_sessions": 2,
  "inflight_calls": 1,
  "last_heartbeat": "2026-02-23T12:00:00+00:00",
  "last_cron_execution": "2026-02-23T11:30:00+00:00"
}
```

- **`active_sessions`** — number of tracked conversation sessions
- **`inflight_calls`** — sessions currently waiting on a Claude CLI response
- **`last_heartbeat`** / **`last_cron_execution`** — ISO 8601 timestamps of the most recent successful execution, or `null` if none yet

Useful for Docker HEALTHCHECK, uptime monitors, and NixOS service checks.

## Docker

Stubert runs in Docker. The image contains the Rust toolchain and pre-compiled dependencies but not the application source — `src/` is mounted at runtime. Code changes only require a container restart, not an image rebuild.

```bash
# Build image (only needed when Cargo.toml/Cargo.lock change)
docker build -t stubert:local .

# Run all unit tests
docker run --rm -v ./src:/app/src stubert:local test

# Run a specific test
docker run --rm -v ./src:/app/src stubert:local test --test test_session

# Run live integration tests (real Claude CLI, needs auth mounts)
docker run --rm \
  -v ./src:/app/src \
  -v "$HOME/.claude":/root/.claude \
  -v "$HOME/.claude.json":/root/.claude.json \
  stubert:local test --test live

# Start the service
docker run --rm \
  -v ./src:/app/src \
  -v ./config:/data \
  -v "$HOME/.claude":/root/.claude \
  -v "$HOME/.claude.json":/root/.claude.json \
  stubert:local
```

See `design-docs/docker.md` for full details on volumes, networking, and NixOS deployment.
