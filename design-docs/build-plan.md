# Build Plan

Test-driven development throughout. Every phase writes tests first, then implements to pass them. The Rust test framework (`#[test]`, `#[tokio::test]`) is used directly — no external test runner.

## Toolchain

- **Language:** Rust (latest stable)
- **Build system:** Cargo (single crate, no workspace needed unless it grows)
- **Async runtime:** tokio (multi-threaded)
- **Test mocking:** `mockall` for trait-based mocks
- **Serialization:** `serde` + `serde_yaml` + `serde_json`
- **Logging:** `tracing` + `tracing-subscriber` + `tracing-appender`

## Phase Dependencies

```
Phase 1 (Config + CLI + Logging)
  ├── Phase 2 (Sessions)
  │     └── Phase 7 (Gateway)
  │           ├── Phase 8 (Commands)
  │           ├── Phase 9 (Heartbeat)
  │           └── Phase 10 (Scheduler)
  ├── Phase 3 (History)
  │     └── Phase 7 (Gateway)
  ├── Phase 4 (Adapter Base)
  │     ├── Phase 5 (Telegram)
  │     └── Phase 6 (Discord)
  │           └── Phase 7 (Gateway)
  └── Phase 11 (Health)
        └── Phase 7 (Gateway)

Phase 12 (Integration + Polish) — after all phases
```

---

## Phase 1: Config, CLI Wrapper, Logging

**Depends on:** Nothing (leaf dependency)

**Design docs:** [architecture.md](architecture.md), [claude-cli.md](claude-cli.md)

### Config (`config/`)

- `load_config(path)` — Read YAML, interpolate `${ENV_VAR}`, deserialize into typed structs
- Frozen config structs via `serde::Deserialize` (no `Default` needed — explicit construction)
- Env var interpolation: walk the YAML tree, replace `${VAR}` patterns from `std::env::var`
- Error on missing env vars (not silent empty string)

**Tests:**
- Load valid config → all fields populated correctly
- Missing env var → error
- Nested env var in lists/maps → interpolated
- Unknown fields → ignored (forward compatibility)
- Config structs are immutable (no `&mut` after load)

### CLI Wrapper (`gateway/claude_cli.rs`)

- `call_claude(params)` → `Result<ClaudeResponse, ClaudeError>`
- Command argument assembly: base → session flag → tools → dirs → model
- JSON stdout parsing into `ClaudeResponse`
- Timeout via `tokio::time::timeout` → kill subprocess
- Error variants: `ExitError`, `ParseError`, `CliFailure`, `Timeout`, `SpawnError`
- `resolve_model(alias)` and `display_model(model_id)` functions

**Tests:**
- Successful call → response parsed correctly
- Non-zero exit → `ExitError` with stderr
- Invalid JSON stdout → `ParseError`
- Subtype != "success" → `CliFailure`
- Timeout → subprocess killed, `Timeout` error
- `--session-id` vs `--resume` flag selection
- Model alias resolution (sonnet → claude-sonnet-4-6)
- Display name mapping (claude-sonnet-4-6 → Sonnet 4.6)
- Unknown alias → passthrough
- CLI path override in args

### Logging (`logging.rs`)

- `setup_logging(config)` — Initialize tracing with file + console output
- File appender with size-based rotation
- `TelegramTransientFilter` — downgrade ERROR → WARN for transient keywords
- Format: `[timestamp] [LEVEL] target: message`

**Tests:**
- Transient keywords ("Bad Gateway", "NetworkError", "TimedOut", "ServerError") downgraded
- Non-transient errors pass through at ERROR level
- Setup is idempotent (second call is no-op)

---

## Phase 2: Session Management

**Depends on:** Phase 1 (config types, model resolution)

**Design doc:** [sessions.md](sessions.md)

### Session

- `Session::new(platform, model)` — generate UUID, set `initiated = false`
- `cli_flags()` → `("--session-id", uuid)` or `("--resume", uuid)`
- `mark_initiated()` — sets `initiated = true`
- `reset()` — new UUID, `initiated = false`, cancel timer
- `enqueue(msg)` — send to mpsc channel, update `last_activity`
- `drain_queue(rx)` — wait for first, batch remaining with "Batched messages from user:" prefix

### SessionManager

- `conversation_key(platform, chat_id)` → `"{platform}-{chat_id}"`
- `get_or_create(platform, chat_id)` → `&mut Session`
- `reset_session(key)`
- `save()` — atomic write (temp file + rename)
- `load()` — read JSON, regenerate UUIDs, set `initiated = false`
- `start_inactivity_timer(key)` — spawn timeout task
- `active_session_count` → `usize`

**Tests:**
- Get or create returns same session for same key
- Get or create returns different sessions for different keys
- Reset generates new UUID
- CLI flags: `--session-id` when not initiated, `--resume` when initiated
- Message batching: single message → plain text, multiple → prefixed
- Save + load round-trip preserves platform and model
- Load regenerates UUIDs (not same as saved)
- Load sets initiated = false
- Inactivity timer fires after configured timeout
- Inactivity timer resets on new activity
- Conversation key format: `"telegram-12345"`
- Atomic save (temp file exists during write)

---

## Phase 3: Chat History

**Depends on:** Phase 1 (config types)

**Design doc:** [gateway.md](gateway.md) (history writer section)

### HistoryWriter

- `write(platform, role, text)` — append to `history/{YYYY-MM-DD}-{platform}.md`
- `search(platform, query, max_results)` → `Vec<SearchResult>`
- Entry format: `[YYYY-MM-DD HH:MM:SS] {role}: {text}`

**Tests:**
- Write creates file with correct name and format
- Multiple writes append to same file
- Date rollover creates new file
- Search finds substring matches (case-insensitive)
- Search returns context lines
- Search respects max_results cap (20)
- Search across multiple date files
- Empty query or no matches → empty results
- Write failure (permissions) → logged, not propagated

---

## Phase 4: Adapter Base, Markdown, Splitting, Sanitization

**Depends on:** Phase 1 (config types)

**Design doc:** [messaging.md](messaging.md)

### PlatformAdapter Trait + IncomingMessage

- `IncomingMessage` struct with all fields
- `PlatformAdapter` trait with start/stop/send_message/send_typing/set_message_handler

### Markdown Conversion

- `to_telegram(text)` — GitHub MD → Telegram MarkdownV2
- `to_discord(text)` — GitHub MD → Discord markdown
- Code blocks preserved (no conversion inside fenced blocks)
- Headers → bold
- Tables → code blocks
- Special character escaping (Telegram)

**Tests:**
- Bold, italic, strikethrough conversion
- Code block passthrough (no escaping inside)
- Header → bold
- Table → code block
- Special character escaping (Telegram)
- Link syntax preserved
- Nested formatting
- Empty input → empty output

### Message Splitting

- `split_message(text, max_length)` → `Vec<String>`
- Paragraph split → line split → hard split fallback
- Code block continuation across splits

**Tests:**
- Short message → single chunk
- Long message → split at paragraph boundary
- Long message without paragraphs → split at line boundary
- No line breaks → hard split at max_length
- Code block split: close at break, reopen with language tag
- Nested code blocks
- Exactly max_length → no split

### Filename Sanitization

- `sanitize_filename(name, existing_files)` → `String`
- Strip path separators, replace unsafe chars, numeric suffix on collision

**Tests:**
- Path traversal stripped (`../../../etc/passwd` → `etc_passwd`)
- Unsafe characters replaced
- Collision → numeric suffix (`file.txt`, `file-1.txt`, `file-2.txt`)
- Windows path separators handled

---

## Phase 5: Telegram Adapter

**Depends on:** Phase 4 (adapter trait, markdown, splitting)

**Design doc:** [messaging.md](messaging.md)

### TelegramAdapter

- Implements `PlatformAdapter`
- Long polling via teloxide
- Inbound: message processing, media download, allowlist check, @botname stripping
- Outbound: markdown conversion, message splitting, send via bot API
- Bot commands menu registration

**Tests (all mocked — no real Telegram API):**
- Text message → correct IncomingMessage
- Photo message → downloaded, path in image_paths
- Voice message → downloaded, path in audio_paths
- Document → downloaded, sanitized filename
- Caption extracted from media messages
- @botname suffix stripped from commands
- Unauthorized user → silently dropped
- Unauthorized user with configured response → response sent
- Send message → markdown converted, split if needed
- Typing indicator sent
- Start/stop lifecycle

---

## Phase 6: Discord Adapter

**Depends on:** Phase 4 (adapter trait, markdown, splitting)

**Design doc:** [messaging.md](messaging.md)

### DiscordAdapter

- Implements `PlatformAdapter`
- WebSocket via serenity
- Activation rules: DM, slash command, mention, reply-to-bot
- Native slash command registration
- Inbound: message processing, attachment download, allowlist check, mention stripping
- Outbound: markdown conversion, splitting, interaction followup + channel fallback

**Tests (all mocked — no real Discord API):**
- DM activates bot
- Mention activates bot
- Reply to bot activates bot
- Regular channel message → ignored
- Slash command → correct IncomingMessage with command text
- Parameterized slash commands (models, skill, history)
- Bot mention stripped from content
- Unauthorized user → dropped
- Own messages → ignored
- Image/audio attachment → downloaded to correct paths
- Send via interaction followup
- Fallback to channel.send on expired interaction

---

## Phase 7: Gateway Core

**Depends on:** Phase 1 (config, CLI, logging), Phase 2 (sessions), Phase 3 (history), Phase 4-6 (adapters)

**Design doc:** [gateway.md](gateway.md)

### Gateway

- `register_adapter(platform, adapter)`
- `start()` — full startup sequence
- `shutdown()` — graceful shutdown with restart message to processing sessions
- `handle_message(incoming)` — main entry point
- Prompt building (text + files + images + audio transcription)
- Consumer task per session (ensure_consumer / consume_queue)
- Processing flow: Claude call → history → send response → inactivity timer
- Resume failure recovery → fresh session retry
- Timeout handling → user notification
- Typing indicator loop (5-second interval)
- File cleanup (30 days)
- Post-restart greeting (restart_origin.json)

### Whisper Integration

- `WhisperTranscriber` — load model once, transcribe via `spawn_blocking`
- Optional — startup failure disables transcription (not fatal)

**Tests:**
- Message routes to correct adapter
- Command detected → handled before session queue
- Non-command → queued → consumer processes → Claude called → response sent
- Multiple messages batched
- Resume failure → fresh session retry → success
- Timeout → user gets timeout message
- Typing indicator sent during Claude call
- Typing indicator cancelled after response
- Session inactivity timer started after response
- History written for user prompt and assistant response
- File cleanup deletes old files, preserves recent ones
- File cleanup removes empty directories
- Restart greeting: reads restart_origin.json, calls Claude, sends response, deletes file
- Shutdown sends restart message to processing sessions
- Unknown platform in message → logged, not crashed
- Empty prompt (no text, no files) → skipped

---

## Phase 8: Slash Commands + Skills

**Depends on:** Phase 7 (gateway)

**Design docs:** [commands.md](commands.md)

### CommandHandler

All 9 commands implemented (see [commands.md](commands.md) for details):

- `/new` — reset + greeting
- `/context` — CLI call for context usage
- `/restart` — write origin, SIGTERM
- `/models` — list or switch
- `/skill` — list or invoke
- `/history` — search transcripts
- `/status` — uptime, sessions, inflight, model
- `/heartbeat` — manual trigger
- `/help` — command listing

### SkillRegistry

- `discover()` — scan `.claude/skills/*.md`
- `get(name)` → skill info
- `list_skills()` → all skills
- `read_prompt(name)` → body after frontmatter
- YAML frontmatter parsing (name, description, allowed_tools, add_dirs)

**Tests:**
- Parse command: recognized → (name, args)
- Parse command: unrecognized → None
- Parse command: @botname suffix stripped
- /new resets session and calls Claude
- /new CLI failure → "Session started but greeting failed."
- /context with active session → CLI response
- /context without active session → "No active session."
- /restart writes restart_origin.json
- /models no args → lists with active marked
- /models with valid alias → switches model
- /models with invalid alias → error message
- /skill no args → lists available
- /skill with name → invokes with skill prompt
- /skill with name + args → prompt appended
- /skill unknown → error message
- /skill with tool overrides → uses skill's tools
- /history with query → search results
- /history no query → usage message
- /status → formatted status string
- /heartbeat → triggers heartbeat
- /heartbeat while running → "already in progress"
- /help → all commands listed
- Skill discovery: valid frontmatter → loaded
- Skill discovery: missing name → skipped
- Skill discovery: no frontmatter → skipped
- Skill read_prompt → body only (no frontmatter)

---

## Phase 9: Heartbeat

**Depends on:** Phase 1 (config, CLI), Phase 7 (gateway lifecycle)

**Design doc:** [heartbeats.md](heartbeats.md)

### HeartbeatRunner

- Loop: sleep interval → read file → call Claude → log
- Overlap protection: `tokio::sync::Mutex` with `try_lock`
- File reading: filter comments (#) and empty lines
- Ephemeral sessions (new UUID each tick)
- Manual trigger via `trigger()`
- HeartbeatLogger with rotation

**Tests:**
- Tick executes Claude call
- Empty file → skip (no CLI call)
- All-comment file → skip
- Missing file → skip
- Overlap: locked → skip
- Manual trigger while loop running → error
- Manual trigger when idle → executes and returns result
- Logger formats correctly (OK/FAIL/SKIPPED with duration)
- last_execution updated after tick
- Stop cancels the loop

---

## Phase 10: Scheduler

**Depends on:** Phase 1 (config, CLI), Phase 7 (gateway lifecycle for adapters)

**Design doc:** [schedule.md](schedule.md)

### TaskScheduler

- Load tasks from schedules.yaml
- Cron parsing and validation
- Ephemeral session per execution
- Per-task logging via JobLogger
- Success notifications (if configured)
- Failure notifications (if `on_failure: notify`)
- Max 1 concurrent instance per task

**Tests:**
- Valid cron parsed correctly
- Invalid cron → error at startup
- Task executes at scheduled time
- Ephemeral session (new UUID each run)
- Success → logged, notification sent if configured
- Failure + on_failure=notify → error notification sent
- Failure + on_failure=log → logged only
- Notification send failure → logged, not propagated
- Max instances: second trigger while running → skipped
- Per-task log files created lazily
- Stop shuts down all jobs

---

## Phase 11: Health Endpoint

**Depends on:** Phase 7 (gateway metrics)

**Design doc:** [gateway.md](gateway.md) (health section)

### HealthServer

- axum HTTP server on configured port (default 8484)
- `GET /health` → JSON response
- Metrics from gateway state: uptime, active sessions, inflight calls, last heartbeat, last cron

**Tests:**
- GET /health returns 200 with correct JSON
- Metrics reflect current state
- Start/stop lifecycle
- Port configuration

---

## Phase 12: Integration + Polish

**Depends on:** All phases

- End-to-end integration tests (mocked adapters, real CLI wrapper with mocked subprocess)
- Live integration tests (real Claude CLI, marked for separate execution)
- Example config files (`example-config/`)
- NixOS deployment config
- Verify all features are covered:
  - [x] Discord native slash commands
  - [x] Skills system
  - [x] Runtime config directory
  - [x] Post-restart greeting
  - [x] Live integration tests
  - [x] Model aliasing + display names
  - [x] CLI binary path config
  - [x] File cleanup (30 days)
  - [x] Discord DM support
  - [x] Telegram @botname stripping
  - [x] TelegramTransientFilter
  - [ ] Whisper in blocking task (deferred — structural support exists, runtime implementation pending)

## Test Strategy

### Unit Tests

Every public function and method gets unit tests. Mocking via `mockall`:

- `PlatformAdapter` trait → mockable
- CLI subprocess → mock `Command` execution
- File I/O → temp directories via `tempfile` crate
- Time → `tokio::time::pause()` for deterministic timer tests

### Integration Tests

Located in `tests/` (Rust integration test directory):

- Full gateway flow: message in → CLI called → response out
- Command routing end-to-end
- Session persistence across simulated restart
- Heartbeat and scheduler execution

### Live Tests

Separate test binary or feature-gated:

- Require Claude Code auth mounts
- Call real Claude CLI
- Verify JSON response parsing with actual output
- Marked with `#[ignore]` (run with `cargo test -- --ignored`)

### Test Conventions

- Async tests use `#[tokio::test]`
- Descriptive module and function names: `mod test_handle_new { fn resets_session() ... }`
- Helper functions for common setup: `make_config()`, `make_incoming()`, `claude_success()`
- No real API calls in default test run
- Temp directories for all file-based tests
