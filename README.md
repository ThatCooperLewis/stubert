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
cargo test --lib adapters::telegram
cargo test --lib adapters
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
│   ├── markdown.rs          # to_telegram() (MarkdownV2), to_discord()
│   ├── message_split.rs     # split_message() (code-block-aware chunking)
│   └── sanitize.rs          # sanitize_filename() (path stripping, collision resolution)
├── gateway/
│   ├── mod.rs               # Module declarations
│   ├── claude_cli.rs        # call_claude(), model aliasing, arg assembly
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

## Docker

Until Phase 12 is complete, all development runs through `cargo` locally. See `design-docs/docker.md` for the planned Docker setup.
