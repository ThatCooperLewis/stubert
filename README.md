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
cargo test --lib adapters::telegram
cargo test --lib adapters
cargo test --lib gateway::core
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

health:
  port: 8484
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

## Docker

Until Phase 12 is complete, all development runs through `cargo` locally. See `design-docs/docker.md` for the planned Docker setup.
