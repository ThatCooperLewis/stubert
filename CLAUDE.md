# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Stubert is a personal AI agent service written in Rust that bridges messaging platforms (Telegram, Discord) to Claude Code CLI sessions. It runs as a single async process on a homelab server. See `README.md` for the full user-facing documentation and `design-docs/` for architecture details.

## Build & Test Commands

```bash
# Build
cargo build

# Run all unit + integration tests
cargo test

# Run tests for a specific module
cargo test --lib adapters::telegram
cargo test --lib gateway::session

# Integration tests (mocked Claude CLI, full Gateway pipeline)
cargo test --test gateway_integration

# Live tests (real Claude CLI, requires auth)
cargo test --test live_cli -- --ignored
```

## Architecture

### Core Message Flow

```
Telegram/Discord message
  → PlatformAdapter.on_message()
    → IncomingMessage (normalized struct)
      → Gateway.handle_message()
        → CommandHandler (if slash command) OR
        → SessionManager → message queue → consumer task
          → call_claude() (subprocess: claude CLI)
            → Response sent back via adapter
```

The adapter normalizes platform input into `IncomingMessage`. The gateway routes it — slash commands handled immediately, everything else queues into the session's message buffer. A per-session consumer task drains the queue, batches waiting messages, invokes Claude CLI as a subprocess, and sends the response back.

### Project Structure

```
stubert/
├── src/
│   ├── main.rs                  # Entry point, signal handling, wiring
│   ├── lib.rs                   # Module declarations
│   ├── config/
│   │   ├── mod.rs               # load_config(), env var interpolation
│   │   └── types.rs             # Config structs (StubbertConfig + sub-configs)
│   ├── adapters/
│   │   ├── mod.rs               # PlatformAdapter trait, IncomingMessage, AdapterError
│   │   ├── telegram.rs          # TelegramAdapter (teloxide long-polling, media downloads)
│   │   ├── discord.rs           # DiscordAdapter (serenity WebSocket, slash commands, media)
│   │   ├── markdown.rs          # to_telegram() (MarkdownV2), to_discord()
│   │   ├── message_split.rs     # split_message() (code-block-aware chunking)
│   │   └── sanitize.rs          # sanitize_filename() (path stripping, collision resolution)
│   ├── gateway/
│   │   ├── mod.rs               # Module declarations
│   │   ├── core.rs              # Gateway orchestrator, message routing, consumer loop, lifecycle
│   │   ├── claude_cli.rs        # call_claude(), model aliasing, arg assembly
│   │   ├── commands.rs          # 9 slash commands, parse_command(), dispatch_command()
│   │   ├── skills.rs            # SkillRegistry, frontmatter parsing from .claude/skills/*.md
│   │   ├── health.rs            # HealthServer (HTTP health endpoint, runtime metrics)
│   │   ├── heartbeat.rs         # HeartbeatRunner (periodic monitoring loop, log rotation)
│   │   ├── scheduler.rs         # TaskScheduler (cron-based task execution, per-task logging)
│   │   ├── history.rs           # HistoryWriter (daily transcripts, search)
│   │   └── session.rs           # Session + SessionManager (message queue, persistence, inactivity timers)
│   └── logging.rs               # setup_logging(), TelegramTransientFilter
├── tests/
│   ├── common/mod.rs            # Shared test helpers
│   ├── gateway_integration.rs   # Full Gateway pipeline tests with mocked CLI
│   └── live_cli.rs              # Real Claude CLI tests (#[ignore])
├── design-docs/                 # Architecture and design documentation
├── example-config/              # Git-committed example runtime files (templates)
└── config/                      # Gitignored live runtime directory
```

### Key Design Decisions

- **CLI subprocess model:** Claude is invoked via `tokio::process::Command` with `--output-format json`, not an API client. Sessions use `--session-id` (new) or `--resume` (continuing).
- **Single tokio runtime:** All concurrency via `tokio::spawn`, `mpsc` channels, and `Mutex`. No application threads.
- **Configuration-driven:** Immutable config from `config.yaml` with env var interpolation. Platform differences expressed in config, not code branches.
- **Memory via markdown:** Runtime `CLAUDE.md` uses `@import` to chain `SOUL.md`, `USER.md`, `MEMORY.md`. Stubert sets the working directory; the CLI reads context files directly.
- **Ephemeral background sessions:** Heartbeats and scheduled tasks get fresh UUIDs per execution, never resumed. Only chat sessions persist.
- **Graceful degradation:** Write failures logged but never block message delivery. CLI failures notify users but don't stop queue processing. Transient Telegram errors downgraded to WARN.

### Concurrency Patterns

| Mechanism | Used For |
|-----------|----------|
| `tokio::spawn` | Consumer tasks (one per session), heartbeat loop, typing indicator |
| `tokio::sync::mpsc` | Per-session message queue |
| `tokio::sync::Mutex` | Heartbeat overlap protection |
| `tokio::process::Command` | Claude CLI subprocess |

### Runtime Directory

The service operates from a runtime directory (`config/`) containing config, memory files, history, logs, and sessions. The path is passed via `--runtime-dir`. All paths in `config.yaml` are relative to this directory.

- **`example-config/`** — Git-committed example files serving as templates for the runtime directory. These are checked into the repo so new deployments have a reference starting point.
- **`config/`** — Gitignored live runtime directory that is actively used by the running service. Contains real secrets, session state, logs, and history.

When making changes that affect runtime file structure or config format, update both `example-config/` (committed reference) and `config/` (live runtime).

## Key Dependencies

- **tokio** — async runtime
- **teloxide 0.17** — Telegram bot framework (rustls)
- **serenity 0.12** — Discord bot framework (rustls)
- **axum 0.8** — HTTP server for health endpoint
- **reqwest 0.12** — HTTP client for media downloads (rustls-tls)
- **serde / serde_yaml_ng / serde_json** — config and data serialization
- **tracing / tracing-subscriber** — structured logging
- **cron 0.15** — cron expression parsing for scheduler
- **mockall 0.13** (dev) — trait mocking for tests
- **tempfile** (dev) — temp directories for file tests

Uses `rustls` throughout (not `native-tls`) — NixOS doesn't have OpenSSL dev headers readily available.

## Test Conventions

- `#[tokio::test]` with `mockall` for trait mocking
- Descriptive module/function names: `mod test_handle_new { fn resets_session() }`
- Helper functions: `make_config()`, `make_incoming()`, `claude_success()`
- All file tests use `tempfile` crate temp directories
- Time-dependent tests use `tokio::time::pause()`
- No real API calls in default test run — live tests are `#[ignore]`

## Deployment

- Native systemd service on NixOS (`stubert.service`)
- Build with `cargo build --release`, restart with `sudo systemctl restart stubert`
- NixOS service config: `nixos-config/nixos/services/stubert.nix`
