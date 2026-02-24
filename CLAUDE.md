# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Stubert is a personal AI agent service written in Rust that bridges messaging platforms (Telegram, Discord) to Claude Code CLI sessions. It runs as a single async process on a homelab server. The full architecture and design are documented in `design-docs/`.

## Build & Test Commands

### Rust

```bash
# Build
cargo build

# Run all unit + integration tests
cargo test

# Run tests for a specific module
cargo test --lib adapters::telegram
cargo test --lib gateway::session

# Integration tests (mocked Claude CLI)
cargo test --test gateway_integration

# Live tests (real Claude CLI, requires auth)
cargo test --test live_cli -- --ignored
```

### Docker

```bash
# Build image (only needed when Cargo.toml/Cargo.lock change)
docker build -t stubert:local .

# Run all unit tests
docker run --rm -v ./src:/app/src stubert:local test

# Run a specific test
docker run --rm -v ./src:/app/src stubert:local test --test test_session

# Run integration tests
docker run --rm -v ./src:/app/src -v ./tests:/app/tests stubert:local test --test gateway_integration

# Run live CLI tests (real Claude CLI, needs auth mounts)
docker run --rm \
  -v ./src:/app/src \
  -v ./tests:/app/tests \
  -v "$HOME/.claude":/root/.claude \
  -v "$HOME/.claude.json":/root/.claude.json \
  stubert:local test --test live_cli -- --ignored

# Start the service
docker run --rm \
  -v ./src:/app/src \
  -v ./config:/data \
  -v "$HOME/.claude":/root/.claude \
  -v "$HOME/.claude.json":/root/.claude.json \
  stubert:local
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

### Module Layout

```
stubert/
├── main.rs                    # Entry point, signal handling, wiring
├── config/                    # YAML loading with ${ENV_VAR} interpolation
├── gateway/
│   ├── core.rs                # Central Gateway orchestrator
│   ├── session.rs             # Session state + SessionManager
│   ├── claude_cli.rs          # Subprocess wrapper (--output-format json)
│   ├── commands.rs            # 9 slash commands
│   ├── skills.rs              # Skill discovery from .claude/skills/
│   ├── health.rs              # HTTP health endpoint (axum, port 8484)
│   ├── heartbeat.rs           # Periodic monitoring loop (reads HEARTBEAT.md)
│   ├── scheduler.rs           # Cron task execution (schedules.yaml)
│   └── history.rs             # Daily transcript writer + search
├── adapters/
│   ├── mod.rs                 # PlatformAdapter trait, IncomingMessage
│   ├── telegram.rs            # teloxide long-polling adapter
│   ├── discord.rs             # serenity WebSocket adapter
│   ├── markdown.rs            # Platform-specific markdown conversion
│   ├── message_split.rs       # Code-block-aware message chunking (2000 char limit)
│   └── sanitize.rs            # Filename sanitization
└── logging.rs                 # tracing setup, TelegramTransientFilter
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

### Runtime Directory (`/data` in Docker)

The service operates from a runtime directory containing config, memory files, history, logs, and sessions. All paths in `config.yaml` are relative to this directory.

## Development Approach

The project follows a 13-phase build plan (see `design-docs/build-plan.md`) with test-driven development throughout. Tests are written first using `#[tokio::test]` and `mockall` for trait mocking. Key test conventions:

- Descriptive module/function names: `mod test_handle_new { fn resets_session() }`
- Helper functions: `make_config()`, `make_incoming()`, `claude_success()`
- All file tests use `tempfile` crate temp directories
- Time-dependent tests use `tokio::time::pause()`
- No real API calls in default test run — live tests are `#[ignore]`

## Docker & Deployment

- Single-stage Dockerfile: Rust toolchain + system deps + Node.js 20 + Claude CLI + pre-compiled dependencies
- Source mounted at runtime (`./src:/app/src`), compiled on container startup
- Image rebuild only needed for dependency changes (`Cargo.toml`/`Cargo.lock`)
- NixOS deployment: `docker-stubert.service` with `--network=host`
- Rootless Docker: container UID 0 maps to host UID 1000 (no privilege escalation)
- Container runs as root because rootless Docker maps it to the host user
