# Gateway

The Gateway is the central orchestrator. It receives messages from platform adapters, manages Claude CLI sessions, runs background tasks (heartbeat, scheduler), writes chat history, and exposes health metrics. Everything runs on a single tokio runtime.

## Struct

```rust
pub struct Gateway {
    config: StubbertConfig,
    session_manager: SessionManager,
    history_writer: HistoryWriter,
    whisper: Option<WhisperTranscriber>,
    submitted_files_dir: PathBuf,

    // Runtime state
    adapters: HashMap<String, Box<dyn PlatformAdapter>>,
    consumer_tasks: HashMap<String, JoinHandle<()>>,
    skill_registry: SkillRegistry,
    command_handler: CommandHandler,
    heartbeat_runner: HeartbeatRunner,
    task_scheduler: TaskScheduler,
    health_server: HealthServer,
    start_time: Instant,
    running: bool,
    cancellation_token: CancellationToken,
}
```

## Lifecycle

### Startup (`start()`)

1. Load sessions from `sessions.json` (regenerate UUIDs, set `initiated = false`)
2. Clean up submitted files older than 30 days
3. Discover skills from `.claude/skills/`
4. Start the health server (axum on port 8484)
5. Start all registered platform adapters
6. Start the heartbeat runner
7. Start the task scheduler
8. Post restart greeting (if `restart_origin.json` exists)

### Shutdown (`shutdown()`)

1. Set `running = false`
2. Cancel the cancellation token (propagates to all background tasks)
3. For each session currently processing: send "Bot is restarting, one moment." via its adapter
4. Stop the task scheduler
5. Stop the heartbeat runner
6. Stop all adapters
7. Stop the health server
8. Cancel and await all consumer tasks

Shutdown is triggered by `SIGTERM` or `SIGINT`. Signal handlers set a stop flag that the main loop checks.

## Message Flow

```
handle_message(incoming: IncomingMessage)
  │
  ├─ Parse as slash command? ──YES──→ CommandHandler.handle() → send response → return
  │
  └─ NO
     │
     ├─ Build prompt (text + files + images + audio transcription)
     │
     ├─ session_manager.get_or_create(platform, chat_id)
     │
     ├─ session.enqueue(prompt)
     │
     └─ ensure_consumer(session_key)
           │
           └─ Spawns _consume_queue task (if not already running)
```

### Prompt Building

The gateway assembles a prompt string from the `IncomingMessage` fields:

1. **Audio transcription** — If `audio_paths` is non-empty and whisper is available, transcribe each file (blocking task). Prepend `"[Voice transcription]: {text}"` to the prompt. If there's also a caption, combine them.
2. **File references** — For each file in `file_paths`, append `` `{filename}`: {absolute_path} `` to the prompt.
3. **Image references** — Image paths are included for Claude to view directly.
4. **Text** — The user's message text, if any.

If the result is empty (e.g., unsupported media with no text), return `None` and skip processing.

### Consumer Loop

Each active session gets one consumer task. The consumer runs until the queue is empty:

```rust
async fn consume_queue(session_key: String, ...) {
    loop {
        let prompt = session.drain_queue().await;  // blocks until message available

        process_prompt(session, prompt, adapter, ...).await;

        // Check if more messages arrived during processing
        if session.message_queue_is_empty() {
            session.processing = false;
            break;
        }
    }
}
```

The gateway tracks consumer tasks by session key and only spawns a new one if none exists for that session.

### Processing a Prompt

```rust
async fn process_prompt(session, prompt, adapter, chat_id, ...) {
    // Start typing indicator loop
    let typing_handle = tokio::spawn(typing_loop(adapter, chat_id));

    match call_claude(params).await {
        Ok(response) => {
            session.mark_initiated();
            session_manager.save();
            history_writer.write(platform, "user", &prompt);
            history_writer.write(platform, "assistant", &response.result);
            adapter.send_message(chat_id, &response.result).await;
            session_manager.start_inactivity_timer(session_key);
        }
        Err(ClaudeError::ExitError { .. }) if session.initiated => {
            // Resume failure — try fresh session
            adapter.send_message(chat_id, SESSION_FAILURE_MESSAGE).await;
            session.reset();
            // Retry with --session-id instead of --resume
            retry_with_fresh_session(session, prompt, adapter, chat_id).await;
        }
        Err(ClaudeError::Timeout { timeout_secs }) => {
            let msg = format!("Claude timed out after {}s. Try a shorter request.", timeout_secs);
            adapter.send_message(chat_id, &msg).await;
        }
        Err(e) => {
            log::error!("Claude call failed: {}", e);
            adapter.send_message(chat_id, ERROR_MESSAGE).await;
        }
    }

    typing_handle.abort();  // Cancel typing indicator
}
```

### Typing Indicator Loop

While waiting for Claude, the gateway sends typing indicators every 5 seconds:

```rust
async fn typing_loop(adapter: &dyn PlatformAdapter, chat_id: &str) {
    loop {
        adapter.send_typing(chat_id).await.ok();
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
```

This runs as a spawned task and is aborted when the Claude call completes.

## Constants

```rust
const RESTART_MESSAGE: &str = "Bot is restarting, one moment.";
const SESSION_FAILURE_MESSAGE: &str = "Session restore failure, starting fresh.";
const ERROR_MESSAGE: &str = "Something went wrong, try again.";
const TIMEOUT_MESSAGE_TEMPLATE: &str = "Claude timed out after {}s. Try a shorter request.";
const FILES_CLEANUP_DAYS: u64 = 30;
const RESTART_ORIGIN_FILE: &str = "restart_origin.json";
```

## Post-Restart Greeting

When the user issues `/restart`, the command handler writes `restart_origin.json`:

```json
{
  "platform": "telegram",
  "chat_id": "12345"
}
```

Then sends `SIGTERM` to the process. On the next startup, `start()` checks for this file:

1. Read `restart_origin.json`
2. Create an ephemeral session (new UUID)
3. Call Claude with a greeting prompt
4. Send the response to the originating platform and chat
5. Delete `restart_origin.json`

This gives the user a confirmation that the restart completed successfully, with Claude generating a natural greeting rather than a canned message.

## File Cleanup

On startup, the gateway scans `submitted-files/` and deletes:

- Files with a modification time older than 30 days
- Empty directories left after file deletion

This prevents unbounded disk growth from downloaded user media.

```rust
fn cleanup_old_files(submitted_files_dir: &Path) {
    let cutoff = SystemTime::now() - Duration::from_secs(FILES_CLEANUP_DAYS * 86400);
    // Walk submitted-files/*, delete files older than cutoff
    // Remove empty directories
}
```

## Chat History

The `HistoryWriter` appends transcripts to daily files:

```rust
pub struct HistoryWriter {
    base_dir: PathBuf,
}

impl HistoryWriter {
    pub fn write(&self, platform: &str, role: &str, text: &str);
    pub fn search(&self, platform: &str, query: &str, max_results: usize) -> Vec<SearchResult>;
}
```

**File format:** `history/{YYYY-MM-DD}-{platform}.md`

**Entry format:** `[YYYY-MM-DD HH:MM:SS] {role}: {text}`

**Search:** Case-insensitive substring match across all history files for the platform. Returns up to `max_results` (default 20) matches with surrounding context lines.

```rust
pub struct SearchResult {
    pub date: String,
    pub line_number: usize,
    pub context: Vec<String>,  // [previous_line, matching_line, next_line]
}
```

## Health Endpoint

An axum HTTP server on port 8484:

```
GET /health → 200 OK
{
  "status": "ok",
  "uptime_seconds": 3600,
  "active_sessions": 2,
  "inflight_calls": 1,
  "last_heartbeat": "2026-02-23T12:00:00Z",
  "last_cron_execution": "2026-02-23T08:00:00Z"
}
```

The metrics function is passed to the health server at construction time. It reads from gateway state (start time, session manager, heartbeat runner, task scheduler).

## Adapter Registration

```rust
impl Gateway {
    pub fn register_adapter(&mut self, platform: &str, adapter: Box<dyn PlatformAdapter>) {
        self.adapters.insert(platform.to_string(), adapter);
    }
}
```

Adapters are registered before `start()`. The gateway sets itself as the message handler on each adapter.

## Logging

### TelegramTransientFilter

A tracing filter that downgrades transient Telegram errors from ERROR to WARN:

**Trigger keywords:** "Bad Gateway", "NetworkError", "TimedOut", "ServerError"

When a log event at ERROR level contains any of these keywords, the filter:
1. Downgrades the level to WARN
2. Removes the stack trace (backtrace)

This prevents log noise from routine Telegram API hiccups.

### Log Configuration

```rust
fn setup_logging(config: &LoggingConfig) {
    // tracing-subscriber with:
    //   - File appender (tracing-appender) with rotation
    //   - Console/stdout layer (for Docker logs / journald)
    //   - Format: [timestamp] [LEVEL] target: message
    //   - TelegramTransientFilter on the file layer
}
```

**Rotation:** File size-based (default 10 MB per file, 5 backups).

## Entry Point

```rust
#[tokio::main]
async fn main() {
    // 1. Parse --runtime-dir argument
    // 2. Change to runtime directory
    // 3. Load config.yaml
    // 4. Set up logging (tracing)
    // 5. Create SessionManager
    // 6. Create HistoryWriter
    // 7. Attempt WhisperTranscriber init (optional — failure logged, not fatal)
    // 8. Create Gateway
    // 9. Register TelegramAdapter and DiscordAdapter
    // 10. Install signal handlers (SIGTERM, SIGINT → graceful shutdown)
    // 11. gateway.start().await
    // 12. Wait for shutdown signal
    // 13. gateway.shutdown().await
}
```
