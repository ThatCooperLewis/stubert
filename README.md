# Stubert

Openclaw-inspired agent manager.

- Fewer security nightmares
- Uses your Claude Code subscription without breaking ToS
- Doesn't try to reinvent the wheel (uses built-in Claude features rather than rewriting them)

## Getting Started

A step-by-step guide from zero to a running Stubert instance in Docker.

### 1. Clone the repository

```bash
git clone <repo-url> stubert
cd stubert
```

### 2. Create your runtime directory

The runtime directory holds all configuration, memory files, logs, and session state. It gets mounted into the container as `/data`.

```bash
mkdir -p config/.claude/skills config/history config/logs
```

### 3. Configure

Copy the example config and edit it with your values:

```bash
cp example-config/config.yaml config/config.yaml
cp example-config/HEARTBEAT.md config/HEARTBEAT.md
```

Create a `.env` file with your bot tokens:

```bash
cat > config/.env << 'EOF'
TELEGRAM_BOT_TOKEN=your-telegram-token-here
DISCORD_BOT_TOKEN=your-discord-token-here
EOF
```

Edit `config/config.yaml` to set your `allowed_users` lists (Telegram/Discord user IDs that are permitted to interact with the bot) and adjust any other settings. See the [Configuration Reference](#configuration-reference) below for all options.

### 4. Set up Claude context files

Stubert sets its working directory to the runtime directory, so the Claude CLI reads context files from there. At minimum, create a `CLAUDE.md`:

```bash
cat > config/CLAUDE.md << 'EOF'
# Agent Instructions

You are a helpful assistant managed by Stubert.
EOF
```

Optional additional context files (Claude CLI reads these if they exist):
- `SOUL.md` — personality and behavioral guidelines
- `USER.md` — information about the user
- `MEMORY.md` — persistent memory across sessions

You can use `@import` in `CLAUDE.md` to chain these files together.

### 5. Build the Docker image

This compiles all Rust dependencies into a cached image layer. You only need to rebuild when `Cargo.toml` or `Cargo.lock` change.

```bash
docker build -t stubert:local .
```

### 6. Authenticate Claude Code

The container needs access to your Claude Code authentication. If you already have Claude Code authenticated on your host machine, skip to step 7 — your `~/.claude` and `~/.claude.json` will be mounted directly.

If you need to authenticate from scratch:

```bash
docker run --rm -it \
  -v "$HOME/.claude":/root/.claude \
  -v "$HOME/.claude.json":/root/.claude.json \
  stubert:local claude login
```

### 7. Run the service

```bash
docker run -d --name stubert \
  --network=host \
  -v ./src:/app/src \
  -v ./config:/data \
  -v "$HOME/.claude":/root/.claude \
  -v "$HOME/.claude.json":/root/.claude.json \
  stubert:local
```

The container compiles the source from the mounted `src/` on startup (a few seconds since dependencies are pre-compiled in the image), then starts the service.

### 8. Verify

```bash
# Check container status
docker logs stubert

# Check health endpoint
curl http://localhost:8484/health
```

You should see a JSON response like:

```json
{
  "status": "ok",
  "uptime_seconds": 12,
  "active_sessions": 0,
  "inflight_calls": 0,
  "last_heartbeat": null,
  "last_cron_execution": null
}
```

Send a message to your bot on Telegram or Discord to confirm it responds.

### Restarting after code changes

Source changes only require a container restart — no image rebuild:

```bash
docker restart stubert
```

Dependency changes (`Cargo.toml`/`Cargo.lock`) require an image rebuild:

```bash
docker build -t stubert:local .
docker rm -f stubert
# Re-run the docker run command from step 7
```

## Running Tests

```bash
# All unit + integration tests
docker run --rm -v ./src:/app/src -v ./tests:/app/tests stubert:local test

# Specific module
docker run --rm -v ./src:/app/src stubert:local test --lib gateway::session

# Integration tests (mocked Claude CLI, full Gateway pipeline)
docker run --rm -v ./src:/app/src -v ./tests:/app/tests stubert:local test --test gateway_integration

# Live CLI tests (real Claude CLI, needs auth mounts)
docker run --rm \
  -v ./src:/app/src \
  -v ./tests:/app/tests \
  -v "$HOME/.claude":/root/.claude \
  -v "$HOME/.claude.json":/root/.claude.json \
  stubert:local test --test live_cli -- --ignored
```

If you have a local Rust toolchain, you can also run tests directly:

```bash
cargo test
cargo test --test gateway_integration
cargo test --test live_cli -- --ignored
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

## Configuration Reference

Stubert loads config from `config.yaml` with `${ENV_VAR}` interpolation. See `example-config/config.yaml` for a fully annotated template.

```yaml
telegram:
  token: "${TELEGRAM_BOT_TOKEN}"
  allowed_users: [123456789]          # Telegram user IDs
  # unauthorized_response: "Not authorized."

discord:
  token: "${DISCORD_BOT_TOKEN}"
  allowed_users: [987654321]          # Discord user IDs
  # unauthorized_response: "Not authorized."

claude:
  cli_path: "claude"                  # Path to Claude CLI binary
  timeout_secs: 300                   # Max seconds per invocation
  default_model: "sonnet"             # sonnet, opus, haiku, or full model ID
  working_directory: "."              # Relative to runtime dir
  env_file_path: ".env"
  allowed_tools:                      # Per-platform tool allowlists
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
  # log_file: "logs/heartbeat.log"
  # log_max_bytes: 5000000
  # log_backup_count: 3

health:
  port: 8484

# scheduler:                            # optional
#   schedules_file: "schedules.yaml"
#   job_log_dir: "logs/cron"
#   job_log_max_bytes: 5242880
#   job_log_backup_count: 3
```

## Skills

Skills are prompt templates discovered from `.claude/skills/*.md` files in the runtime directory. Each file uses YAML frontmatter:

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

## Docker Details

The image contains the Rust toolchain and pre-compiled dependencies but not the application source — `src/` is mounted at runtime. Code changes only require a container restart, not an image rebuild.

### Volume mounts

| Host Path | Container Path | Purpose |
|-----------|---------------|---------|
| `./src` | `/app/src` | Live source code (compiled on startup) |
| `./config` | `/data` | Runtime directory (config, memory, history, logs, sessions) |
| `$HOME/.claude` | `/root/.claude` | Claude Code authentication token |
| `$HOME/.claude.json` | `/root/.claude.json` | Claude Code authentication metadata |

### Entrypoint modes

| Command | What It Does |
|---------|-------------|
| `docker run stubert:local` | Start the service (default: `serve`) |
| `docker run stubert:local test` | Run all tests |
| `docker run stubert:local test --lib gateway::session` | Run specific test module |
| `docker run stubert:local bash` | Interactive shell |

### Networking

The health endpoint listens on port 8484. With `--network=host` (recommended for production), no port mapping is needed. For development without host networking:

```bash
docker run --rm -p 8484:8484 \
  -v ./src:/app/src \
  -v ./config:/data \
  -v "$HOME/.claude":/root/.claude \
  -v "$HOME/.claude.json":/root/.claude.json \
  stubert:local
```

See `design-docs/docker.md` for full details on build caching, rootless Docker, and NixOS deployment.

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

## Local Development

If you have a local Rust toolchain, you can build and test without Docker:

### Prerequisites

- Rust toolchain (rustc, cargo)
- A C compiler (`gcc`) and `pkg-config`

### Build & test

```bash
cargo build
cargo test
```
