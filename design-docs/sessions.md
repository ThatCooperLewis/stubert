# Sessions

Every conversation (identified by platform + chat ID) gets a session that tracks its Claude CLI state. Sessions handle message queuing, batching, inactivity timeouts, and persistence across restarts.

## Session State

```rust
pub struct Session {
    pub session_id: Uuid,
    pub initiated: bool,
    pub platform: String,
    pub model: String,
    pub processing: bool,
    pub last_activity: Instant,
    message_tx: mpsc::UnboundedSender<String>,
    message_rx: Option<mpsc::UnboundedReceiver<String>>,  // taken by consumer
    inactivity_handle: Option<JoinHandle<()>>,
}
```

| Field | Purpose |
|-------|---------|
| `session_id` | UUID passed to Claude CLI via `--session-id` or `--resume` |
| `initiated` | Whether the first CLI call has completed (determines which flag to use) |
| `platform` | `"telegram"` or `"discord"` |
| `model` | Full model ID (e.g., `"claude-sonnet-4-6"`) |
| `processing` | Whether a consumer task is actively processing this session's queue |
| `last_activity` | Timestamp of last `enqueue()` call |
| `message_tx` / `message_rx` | MPSC channel for message queueing |
| `inactivity_handle` | Handle to the timeout task (for cancellation on new activity) |

## CLI Flag Selection

```rust
impl Session {
    pub fn cli_flags(&self) -> (&str, &str) {
        if self.initiated {
            ("--resume", &self.session_id.to_string())
        } else {
            ("--session-id", &self.session_id.to_string())
        }
    }

    pub fn mark_initiated(&mut self) {
        self.initiated = true;
    }
}
```

After the first successful CLI call, the session is marked `initiated`. All subsequent calls use `--resume` to continue the conversation.

## Message Queuing

When a user sends a message, it's enqueued into the session's channel:

```rust
impl Session {
    pub fn enqueue(&self, message: String) {
        self.last_activity = Instant::now();
        self.message_tx.send(message).ok();
    }
}
```

The gateway calls `enqueue()` and then ensures a consumer task is running for this session (see [gateway.md](gateway.md)).

## Message Batching

The consumer task drains the queue before each CLI call. If multiple messages arrived while Claude was processing the previous one, they're combined into a single prompt:

```rust
impl Session {
    pub async fn drain_queue(rx: &mut mpsc::UnboundedReceiver<String>) -> String {
        // Wait for at least one message
        let first = rx.recv().await.unwrap();

        // Drain any additional buffered messages (non-blocking)
        let mut messages = vec![first];
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }

        if messages.len() == 1 {
            messages.into_iter().next().unwrap()
        } else {
            format!("Batched messages from user:\n{}", messages.join("\n"))
        }
    }
}
```

This prevents a burst of messages from spawning separate CLI calls. The user sees one response covering all their messages.

## Session Reset

Reset generates a new UUID and clears the initiated flag:

```rust
impl Session {
    pub fn reset(&mut self) {
        self.session_id = Uuid::new_v4();
        self.initiated = false;
        self.cancel_inactivity_timer();
    }
}
```

Triggers:
- `/new` command (explicit user request)
- Inactivity timeout (automatic)
- Resume failure recovery (retry with fresh session)

## SessionManager

```rust
pub struct SessionManager {
    sessions: HashMap<String, Session>,
    sessions_path: PathBuf,
    timeout_minutes: u64,
    default_model: String,
}
```

### Conversation Key

Sessions are identified by `"{platform}-{chat_id}"`:

```rust
impl SessionManager {
    pub fn conversation_key(platform: &str, chat_id: &str) -> String {
        format!("{}-{}", platform, chat_id)
    }
}
```

### Get or Create

```rust
impl SessionManager {
    pub fn get_or_create(&mut self, platform: &str, chat_id: &str) -> &mut Session {
        let key = Self::conversation_key(platform, chat_id);
        self.sessions.entry(key).or_insert_with(|| {
            Session::new(platform.to_string(), self.default_model.clone())
        })
    }
}
```

### Reset Session

```rust
impl SessionManager {
    pub fn reset_session(&mut self, key: &str) {
        if let Some(session) = self.sessions.get_mut(key) {
            session.reset();
        }
    }
}
```

## Inactivity Timeout

Each session has an inactivity timer. When the timer fires (default 60 minutes), the session resets automatically.

```rust
impl SessionManager {
    pub fn start_inactivity_timer(&mut self, key: String) {
        // Cancel existing timer if any
        if let Some(session) = self.sessions.get_mut(&key) {
            session.cancel_inactivity_timer();
        }

        let timeout = Duration::from_secs(self.timeout_minutes * 60);
        // Spawn timer task that resets the session after timeout
        let handle = tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            // Signal session reset (via channel or shared state)
        });

        if let Some(session) = self.sessions.get_mut(&key) {
            session.inactivity_handle = Some(handle);
        }
    }
}
```

The timer is restarted on every successful Claude response. It's cancelled on explicit reset (`/new`).

## Persistence

Sessions are saved to `sessions.json` after every successful CLI call and on startup load.

### File Format

```json
{
  "telegram-12345": {
    "uuid": "a1b2c3d4-...",
    "initiated": false,
    "platform": "telegram",
    "model": "claude-sonnet-4-6"
  },
  "discord-67890": {
    "uuid": "e5f6g7h8-...",
    "initiated": false,
    "platform": "discord",
    "model": "claude-sonnet-4-6"
  }
}
```

### Save (Atomic Write)

```rust
impl SessionManager {
    pub fn save(&self) -> Result<(), io::Error> {
        let tmp_path = self.sessions_path.with_extension("json.tmp");
        let data = serde_json::to_string_pretty(&self.to_serializable())?;
        std::fs::write(&tmp_path, &data)?;
        std::fs::rename(&tmp_path, &self.sessions_path)?;
        Ok(())
    }
}
```

Write to a temp file first, then atomic rename. This prevents corruption if the process is killed mid-write.

### Load (Startup)

```rust
impl SessionManager {
    pub fn load(&mut self) -> Result<(), io::Error> {
        // Read sessions.json
        // For each entry:
        //   - Regenerate UUID (fresh session ID)
        //   - Set initiated = false (will use --session-id on first call)
        //   - Preserve platform and model
    }
}
```

On startup, all UUIDs are regenerated. The CLI session IDs from the previous run are discarded — Stubert does not attempt to resume CLI sessions across process restarts. This avoids stale session edge cases. The `initiated` flag is set to `false` so the first message after restart creates a new CLI session.

## Active Session Count

```rust
impl SessionManager {
    pub fn active_session_count(&self) -> usize {
        self.sessions.len()
    }
}
```

Used by the health endpoint and `/status` command.

## Ephemeral Sessions

Heartbeats and scheduled tasks don't use `SessionManager`. They create a one-off UUID, call the CLI with `--session-id`, and discard the UUID after completion. These sessions are never persisted or resumed.
