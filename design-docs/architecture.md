# Architecture

Stubert is a personal AI agent service that bridges messaging platforms (Telegram, Discord) to Claude Code CLI sessions. It runs as a single async process on a homelab server.

## Core Flow

```
Telegram/Discord message
  → PlatformAdapter.on_message()
    → IncomingMessage (normalized)
      → Gateway.handle_message()
        → CommandHandler (if slash command)
        → SessionManager.get_or_create()
          → Message queued → batch consumed
            → call_claude() (subprocess: claude CLI)
              → Response sent back via adapter
```

Every user message follows this path. The adapter normalizes platform-specific input into an `IncomingMessage` struct. The gateway routes it — slash commands get handled immediately, everything else queues into the session's message buffer. A per-session consumer task drains the queue, batches waiting messages, invokes the Claude CLI as a subprocess, and sends the response back through the adapter.

## Module Layout

```
stubert/
├── main.rs                    # Entry point, signal handling, component wiring
├── config/
│   ├── mod.rs                 # Config loading, env var interpolation
│   └── types.rs               # Frozen config structs (serde)
├── gateway/
│   ├── core.rs                # Central orchestrator (Gateway)
│   ├── session.rs             # Session + SessionManager
│   ├── claude_cli.rs          # Subprocess wrapper for claude CLI
│   ├── commands.rs            # Slash command routing + handlers
│   ├── skills.rs              # Skill discovery from .claude/skills/
│   ├── health.rs              # HTTP health endpoint (axum)
│   ├── history.rs             # Transcript writer
│   └── whisper.rs             # Audio transcription (whisper-rs)
├── adapters/
│   ├── mod.rs                 # PlatformAdapter trait, IncomingMessage
│   ├── telegram.rs            # Telegram adapter (teloxide)
│   ├── discord.rs             # Discord adapter (serenity)
│   ├── markdown.rs            # Platform-specific markdown conversion
│   ├── message_split.rs       # Message chunking with code block awareness
│   └── sanitize.rs            # Filename sanitization
├── heartbeat/
│   ├── runner.rs              # Periodic heartbeat loop
│   └── logger.rs              # Heartbeat-specific log writer
├── scheduler/
│   ├── scheduler.rs           # Cron task executor
│   ├── config.rs              # Task config from schedules.yaml
│   └── logger.rs              # Per-task log writer
└── logging.rs                 # tracing setup, TelegramTransientFilter
```

## Design Principles

**Single async runtime.** Everything runs on one tokio runtime. Background work uses `tokio::spawn`, message batching uses `tokio::sync::mpsc`, overlap protection uses `tokio::sync::Mutex`. No threads are spawned for application logic — only tokio's internal thread pool and blocking tasks (whisper transcription, file I/O).

**CLI subprocess model.** Claude is invoked via `tokio::process::Command`, not an API client. Each call is a fresh process with `--output-format json`. Sessions are identified by `--session-id` (new) or `--resume` (continuing) flags. This means Stubert has no direct dependency on the Claude API — it wraps the CLI binary.

**Configuration-driven.** A single `config.yaml` with `${ENV_VAR}` interpolation for secrets. All config structs are immutable after loading. Platform differences (tool permissions, read-only mode) are expressed in config, not code branches.

**Memory via markdown.** The runtime `CLAUDE.md` uses `@import` to load `SOUL.md`, `USER.md`, and `MEMORY.md` into every CLI invocation automatically. Stubert doesn't assemble context — the CLI does it from the working directory.

**Ephemeral sessions for background work.** Heartbeats and scheduled tasks get fresh UUIDs per execution. They are never resumed. Only chat sessions persist across messages.

**Graceful degradation.** Write failures are logged but never block message delivery. CLI failures notify the user but don't stop queue processing. Transient Telegram errors are downgraded to warnings.

## Concurrency Model

| Mechanism | Purpose |
|-----------|---------|
| `tokio::spawn` | Consumer tasks (one per active session), heartbeat loop, typing indicator |
| `tokio::sync::mpsc` | Per-session message queue (unbounded sender, receiver in consumer) |
| `tokio::sync::Mutex` | Heartbeat overlap protection (shared with `/heartbeat` command) |
| `tokio::process::Command` | Claude CLI subprocess invocation |
| `tokio::task::spawn_blocking` | Whisper transcription, synchronous file I/O |
| `tokio::time::sleep` | Inactivity timers, heartbeat interval, typing indicator cadence |
| `tokio::select!` | Shutdown coordination — race stop signal against work loops |
| `tokio_util::CancellationToken` | Propagate shutdown to all background tasks |

## Key Constants

| Constant | Value | Purpose |
|----------|-------|---------|
| `FILES_CLEANUP_DAYS` | 30 | Delete submitted files older than this |
| `TYPING_INTERVAL` | 5 seconds | Re-send typing indicator during Claude calls |
| `DEFAULT_SESSION_TIMEOUT` | 60 minutes | Inactivity before session reset |
| `DEFAULT_HEARTBEAT_INTERVAL` | 30 minutes | Between heartbeat executions |
| `DEFAULT_CLI_TIMEOUT` | 300 seconds | Max wait for Claude CLI response |
| `HEALTH_PORT` | 8484 | Default health endpoint port |
| `MAX_MESSAGE_LENGTH` | 2000 | Split threshold for outgoing messages |
| `HISTORY_MAX_RESULTS` | 20 | Cap on `/history` search results |

## Runtime Directory

Stubert operates from a runtime directory (`config/` in the repo, passed via `--runtime-dir`). All paths in `config.yaml` are relative to this directory.

```
config/
├── config.yaml                # Main configuration
├── .env                       # Environment variables for Claude CLI Bash commands
├── CLAUDE.md                  # AI instructions (@imports SOUL.md, USER.md, MEMORY.md)
├── SOUL.md                    # Personality definition
├── USER.md                    # Human profile
├── MEMORY.md                  # Long-term memory (written by Claude)
├── HEARTBEAT.md               # Recurring monitoring instructions
├── PUBLIC.md                  # Optional: instructions for public Discord channels
├── schedules.yaml             # Cron task definitions
├── sessions.json              # Persistent session state
├── .claude/
│   └── skills/
│       └── {skill-name}/
│           └── SKILL.md       # Skill with YAML frontmatter
├── submitted-files/
│   └── {platform}-{chat_id}/  # Downloaded user files (auto-cleaned after 30 days)
├── history/
│   └── {YYYY-MM-DD}-{platform}.md  # Daily chat transcripts
└── logs/
    ├── stubert.log            # Service log (rotated)
    ├── heartbeat.log          # Heartbeat execution log
    └── cron/
        └── {task-name}.log    # Per-task scheduler logs
```

## Error Recovery

**Resume failure → fresh session.** If `--resume` fails (stale session), Stubert notifies the user, resets the session with a new UUID, and retries with `--session-id`.

**CLI timeout → user notification.** If Claude doesn't respond within the timeout, the subprocess is killed and the user gets a timeout message. The session remains valid for the next message.

**Transient Telegram errors → warning.** Network errors from Telegram ("Bad Gateway", "NetworkError", "TimedOut", "ServerError") are logged as warnings without stack traces, not errors.

**Restart persistence.** On `/restart`, the originating platform and chat ID are written to `restart_origin.json`. After restart, the gateway reads this file, greets the user in that chat, and deletes the file.

## Crate Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime |
| `teloxide` | Telegram bot API |
| `serenity` | Discord bot API |
| `axum` | HTTP server (health endpoint) |
| `tokio-cron-scheduler` | Cron task scheduling |
| `whisper-rs` | Audio transcription (bindings to whisper.cpp) |
| `serde` + `serde_yaml` | Config deserialization with env var interpolation |
| `serde_json` | CLI output parsing |
| `tokio::process` | Subprocess management (part of tokio) |
| `pulldown-cmark` or `comrak` | Markdown processing |
| `tracing` + `tracing-subscriber` | Structured logging |
| `tracing-appender` | Log file rotation |
| `uuid` | Session ID generation |
| `chrono` | Timestamps for history, metrics |
| `reqwest` | File downloads from platform CDNs |
| `mockall` | Trait mocking in tests |
