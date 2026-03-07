# Stubert

Stubert (a nickname for my cat Stu) is an Openclaw-inspired wrapper for Claude Code. 

Message Claude remotely, trigger cron jobs, and continuously run checks via Heartbeats.

- **Respects your intelligence**  
  - Setup isn't over-engineered
  - Configuration is entirely yaml-based
- **Fewer security nightmares** 
  - No direct internet access or high-risk skill storefront
  - All risky features are opt-in
  - Allows unique permissions for each chat platform and cron job
- **Uses your Claude Code subscription without breaking ToS** 
  - Works headlessly through Claude CLI using `claude -p`
  - Doesn't circumvent API restrictions
- **Doesn't try to reinvent the wheel** 
  - All your user-level Claude Code features/skills/MCPs are enabled
  - Passively gains more functionality as Claude Code is updated

## Why Stubert do?

I made this with Opus 4.6 after being frustrated by the awful design of Openclaw. Security issues aside, almost everything about that program is poorly-made and very sloppy. The WebUI was nearly unusable, the JSON configuration was janky, the docs were innacurate slop. I found it was constantly bricking itself unless I overpaid in API usage for Opus 4.5+.

I didn't need every conceivable chat platform and model integration, I didn't want on-by-default web access, and I wanted it to use all the skills I already built for my Claude Code installation. I then learned that Claude Code can be run headless, so all I needed was an 'Openclaw' that worked specifically through Claude. So, Stubert was born!

> [!IMPORTANT]
> This program is ToS-compliant because it talks to the `claude` binary directly. This comes with many downsides, mainly a small hit to response time. However, as of time of writing, this is allowed and [well-documented by Anthropic](https://code.claude.com/docs/en/headless) (assuming personal + private usage). Other programs like Openclaw use an API proxy to circumvent the binary entirely, which much faster, but is very much _not_ allowed.
>
> All that being said, I'm not a lawyer, and cannot 100% guarantee that this will be kosher in perpetuity.

## What Stubert do?

Stubert can do anything you'd typically do with Claude Code (file browsing, skills, MCPs, subagents), but also:

- Respond to messages via Telegram or Discord
- Perform scheduled tasks
- Act periodically based on a Hearbeat configuration
- Restrict permissions for each chat platform & user
- Browse the web using an isolated agent with **zero** local permissions

## What Stubert *not* do?

- Passively browse the internet with your personal info in its context
- Rack up API usage costs (unless you specifically turn that on in your Claude account settings)
- Open your system to a number of security vulnerabilities (unless you want it to, I guess?)
- Access your entire filesystem (unless you specifically configure it to)
- Be configured by an awful webUI 

# Installation

> [!CAUTION]
> This software might be safer than Openclaw, but that is a very low bar. Please use caution and care when running agents on your filesystem!

### Prerequisites

The Claude CLI subprocess needs these at runtime:
- **Claude Code CLI** — For this env I installed via `npm install -g @anthropic-ai/claude-code`, you may need to adjust your $PATH below for other intall routes. 
- **Claude Code authenticated** — run `claude login` as the user that will run the service
- **Discord or Telegram Bot** ready for integration – You'll need the OAuth token from either. The Openclaw docs ([Discord](https://docs.openclaw.ai/channels/discord#discord) / [Telegram](https://docs.openclaw.ai/channels/telegram)) are sufficient.

### 1. Clone the repository
 
```bash
git clone git@github.com:ThatCooperLewis/stubert.git stubert
cd stubert
```

### 2. Create your runtime directory

The runtime directory holds all configuration, memory files, logs, and session state.

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

You are a helpful assistant named Stubert.
EOF
```

Optional additional context files (Claude CLI reads these if they exist):
- `SOUL.md` — personality and behavioral guidelines
- `USER.md` — concrete information about the user
- `MEMORY.md` — persistent memory across sessions (Claude keeps its own memory file in ~./claude/projects, it's recommened to symlink it, or instruct Claude to write to this one instead).

You can use `@import` in `CLAUDE.md` to chain these files together.

### 5. Build the binary

Requires a Rust toolchain and a C compiler (`gcc` + `pkg-config`).

```bash
cargo build --release
```

The binary is at `target/release/stubert`.

### 6. Run the service

You can run directly for testing:

```bash
./target/release/stubert --runtime-dir ./config
```

For production, set up a systemd service — see the [Systemd Service](#systemd-service) section below.

### 7. Verify

```bash
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

```bash
cargo build --release && sudo systemctl restart stubert
```

## Systemd Service

Stubert runs as a native systemd service with journal integration, automatic restarts, and standard service management.

### Create the unit file

Create `/etc/systemd/system/stubert.service`:

```ini
[Unit]
Description=Stubert AI Agent Service
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/path/to/stubert/target/release/stubert --runtime-dir /path/to/stubert/config
Restart=on-failure
RestartSec=5
TimeoutStopSec=30

# Run as your user — needs access to ~/.claude auth
User=youruser
Group=yourgroup

# Claude CLI needs HOME and PATH to find node/claude
Environment=HOME=/home/youruser
Environment=PATH=/usr/local/bin:/usr/bin:/bin

# Load bot tokens and secrets from .env
EnvironmentFile=/path/to/stubert/config/.env

[Install]
WantedBy=multi-user.target
```

Adjust the paths, user/group, and `PATH` to match your system. The `PATH` must include wherever `claude` and `node` are installed.

### Enable and start

```bash
sudo systemctl daemon-reload
sudo systemctl enable stubert
sudo systemctl start stubert
```

### Managing the service

```bash
systemctl status stubert          # check status
journalctl -u stubert -f          # tail logs
sudo systemctl restart stubert    # restart after rebuilding
```

### NixOS

On NixOS, declare the service in your configuration instead of writing a unit file manually. Example module:

```nix
{ lib, pkgs, ... }:

let
  repoDir = "/home/youruser/stubert";
  configDir = "${repoDir}/config";
  binary = "${repoDir}/target/release/stubert";
in
{
  systemd.services.stubert = {
    description = "Stubert AI Agent Service";
    after = [ "network-online.target" ];
    wants = [ "network-online.target" ];
    wantedBy = [ "multi-user.target" ];

    serviceConfig = {
      Type = "simple";
      ExecStart = "${binary} --runtime-dir ${configDir}";
      Restart = "on-failure";
      RestartSec = 5;
      User = "youruser";
      Group = "users";
      Environment = [
        "HOME=/home/youruser"
        "PATH=${lib.makeBinPath [ pkgs.claude-code pkgs.nodejs ]}:/run/current-system/sw/bin"
      ];
      EnvironmentFile = "${configDir}/.env";
    };
  };
}
```

Import this module in your flake/configuration and run `nixos-rebuild switch`.

# Usage + Configuration

## CLI Commands

Address the binary directly on the host

| Command | Description |
|---------|-------------|
| `stubert run` | Start the service (default if no subcommand given) |
| `stubert chat` | Open an interactive Claude Code session in the runtime directory |
| `stubert status` | Show service status (uptime, sessions, in-flight calls) |
| `stubert restart` | Send SIGTERM to the running service (systemd restarts it) |
| `stubert rebuild` | `cargo build --release` then restart the service |
| `stubert schedules` | Show configured scheduled tasks |
| `stubert context <session_id>` | Query context window usage for a session |
| `stubert search <query>` | Search the web using an isolated Claude agent |

Options available on most subcommands:
- `--runtime-dir <path>` — path to runtime directory (auto-detected from binary location if omitted)
- `--model <alias>` — model to use: `sonnet`, `opus`, `haiku`, or a full model ID (available on `chat` and `search`)

## Slash Commands

Available when chatting with Stubert in Discord or Telegram

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
    model: haiku
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
- **Model:** Per-task `model` field accepts an alias (`sonnet`, `opus`, `haiku`) or full model ID. Defaults to `sonnet`.
- **Notifications:** `notify` sets the destination for announcing task output — when present, Claude's response is always sent on success. Failures are only announced when `on_failure: notify` (default is `log`, which only writes to the job log).
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

## Running Tests

```bash
# All unit + integration tests
cargo test

# Specific module
cargo test --lib gateway::session

# Integration tests (mocked Claude CLI, full Gateway pipeline)
cargo test --test gateway_integration

# Live CLI tests (real Claude CLI, requires auth)
cargo test --test live_cli -- --ignored
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
