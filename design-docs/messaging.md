# Messaging

Stubert connects to Telegram and Discord through platform adapters. Each adapter converts platform-specific events into a normalized `IncomingMessage`, handles outbound markdown conversion and message splitting, and manages platform lifecycle (connect, poll, disconnect).

## PlatformAdapter Trait

```rust
pub struct IncomingMessage {
    pub platform: String,        // "telegram" or "discord"
    pub user_id: String,
    pub chat_id: String,
    pub text: Option<String>,
    pub image_paths: Vec<PathBuf>,
    pub audio_paths: Vec<PathBuf>,
    pub file_paths: Vec<PathBuf>,
    pub file_names: Vec<String>,
}

#[async_trait]
pub trait PlatformAdapter: Send + Sync {
    async fn start(&mut self) -> Result<()>;
    async fn stop(&mut self) -> Result<()>;
    async fn send_message(&self, chat_id: &str, text: &str) -> Result<()>;
    async fn send_typing(&self, chat_id: &str) -> Result<()>;
    fn set_message_handler(&mut self, handler: MessageHandler);
}
```

The handler is a callback function (or boxed closure) that the gateway provides. When an adapter receives a message, it normalizes it and calls the handler.

## Access Control

Both adapters enforce an allowlist at the adapter layer, before any message reaches the gateway:

```yaml
telegram:
  allowed_users: [123456789]
discord:
  allowed_users: [261016282877526026]
```

Unauthorized messages are silently dropped (or optionally responded to with `unauthorized_response` if configured). The check happens by user ID, not username.

## Telegram Adapter

**Library:** `teloxide`

### Connection

Telegram uses long polling — the adapter polls Telegram's API for updates. On `start()`:

1. Build the bot client from the token
2. Register the message handler
3. Set the bot commands menu (so users see available commands in the Telegram UI)
4. Begin polling

On `stop()`: stop the polling loop and disconnect.

### Inbound Message Processing

When a message arrives:

1. **Ignore non-message updates** — edits, channel posts, etc. are skipped
2. **Allowlist check** — reject unauthorized `user_id`
3. **Extract text** — from `message.text` or `message.caption` (for media with captions)
4. **Strip @botname suffix** — Telegram appends `@botname` to commands in groups (e.g., `/help@stubert_bot`). The adapter strips this before command parsing.
5. **Download media:**
   - **Photos:** Download highest resolution version, save as `{unique_id}.jpg`
   - **Voice/Audio:** Download as `{unique_id}.ogg`
   - **Documents:** Download with `{file_unique_id}{original_extension}`, filename sanitized
6. **Build `IncomingMessage`** and call the handler

### File Storage

Downloaded files go to `submitted-files/{platform}-{chat_id}/`:

```
submitted-files/
└── telegram-12345/
    ├── abc123.jpg          # Photo
    ├── def456.ogg          # Voice message
    └── ghi789.pdf          # Document
```

Files older than 30 days are automatically cleaned up by the gateway (see [gateway.md](gateway.md)).

### Filename Sanitization

User-submitted filenames are sanitized before saving:

- Strip path components (both `/` and `\`)
- Replace unsafe characters with `_`
- On collision, append numeric suffix (`-1`, `-2`, etc.)

Sanitized filenames are backtick-quoted when included in the prompt to Claude.

### Audio Transcription (Whisper)

Voice messages and audio files are transcribed before being sent to Claude:

1. Download the audio file to a temp path
2. Run whisper transcription in a blocking task (`tokio::task::spawn_blocking`) to avoid blocking the async runtime
3. Join transcribed segments with spaces
4. If the message also has a caption, combine: `"{caption}\n\n[Voice transcription]: {text}"`
5. If transcription-only (no caption): `"[Voice transcription]: {text}"`

The whisper model is loaded once at startup. If loading fails (e.g., model not available), audio transcription is disabled and voice messages are skipped with a log warning.

**Rust implementation:** Use `whisper-rs` (Rust bindings to whisper.cpp). The transcription runs on a blocking thread since whisper.cpp is CPU-bound.

### Outbound Markdown Conversion

Claude's responses use GitHub-flavored markdown. Telegram requires MarkdownV2 format. The conversion:

- **Special characters** — Escape `_`, `*`, `[`, `]`, `(`, `)`, `~`, `>`, `#`, `+`, `-`, `=`, `|`, `{`, `}`, `.`, `!` outside of formatting
- **Headers** (`# Heading`) — Convert to bold text (`**Heading**`)
- **Tables** — Convert to code blocks (Telegram doesn't support tables)
- **Code blocks** — Preserved as-is (no escaping inside fenced blocks)
- **Inline formatting** — Bold, italic, strikethrough preserved with Telegram syntax
- **Links** — Markdown link syntax preserved (`[text](url)`)

### Message Splitting

Telegram has a 2000-character message limit. Long responses are split:

1. **Try paragraph split** — break on double newlines
2. **Try line split** — break on single newlines
3. **Hard split** — break at character limit

**Code block awareness:** If a split occurs inside a fenced code block, the block is closed with ` ``` ` at the split point and reopened with the language tag at the start of the next chunk.

```
# Original response with long code block:
Here's the code:
```python
line 1
line 2
... (exceeds limit)
```

# After splitting:
## Chunk 1:
Here's the code:
```python
line 1
line 2
```

## Chunk 2:
```python
line 3
line 4
```
```

## Discord Adapter

**Library:** `serenity`

### Connection

Discord uses WebSocket (gateway connection). On `start()`:

1. Build the client with the token and intents
2. Register the event handler and command tree
3. Spawn the client connection as a background task

On `stop()`: close the client connection.

### Activation Rules

Unlike Telegram (which responds to every message), Discord requires explicit activation:

| Condition | Activates? |
|-----------|-----------|
| Direct message (DM) | Yes |
| Slash command (`/new`, `/status`, etc.) | Yes |
| @mention the bot | Yes |
| Reply to a bot message | Yes |
| Regular message in a channel | No |

The adapter checks these conditions in order. If none match, the message is ignored.

### Slash Commands

Discord has native slash command support. All commands from the command registry are registered as Discord slash commands on startup:

**Parameterless commands** — `/new`, `/context`, `/restart`, `/status`, `/help`, `/heartbeat`

**Parameterized commands:**
- `/models [model]` — optional model alias
- `/skill [name] [args]` — optional skill name and arguments
- `/history [query]` — optional search query

Slash command flow:
1. Discord sends an interaction event
2. Adapter checks allowlist by `user_id`
3. Defer the interaction (prevents Discord's 3-second timeout)
4. Store the interaction reference (for followup responses)
5. Build `IncomingMessage` with the command text (e.g., `"/models sonnet"`)
6. Call the gateway handler

Responses to slash commands are sent via `interaction.followup.send()`. If the interaction reference is unavailable (expired), the adapter falls back to `channel.send()`.

### DM Support

Direct messages to the bot work without mention or reply. The adapter detects DMs by checking whether the message has a guild (server) — if there's no guild, it's a DM. DMs follow the same processing flow as activated channel messages.

### Inbound Message Processing

1. **Ignore own messages** — skip messages from the bot itself
2. **Check activation rules** — DM, slash command, mention, or reply-to-bot
3. **Allowlist check** — reject unauthorized `user_id`
4. **Strip bot mention** — remove `<@bot_id>` from message content
5. **Download attachments** — by content type:
   - `image/*` → saved to `image_paths`
   - `audio/*` → saved to `audio_paths`
   - Other → saved to `file_paths` with sanitized `file_names`
6. **Build `IncomingMessage`** and call the handler

### Outbound Markdown Conversion

Discord's markdown is close to GitHub-flavored, so less conversion is needed:

- **Horizontal rules** (`---`) — dropped (Discord renders them poorly)
- **Tables** — converted to code blocks
- **Headers** — converted to bold text
- **Links** — preserved as-is
- **Code blocks, bold, italic** — passed through unchanged

### Message Splitting

Same logic as Telegram — 2000-character limit with paragraph/line/hard split and code block awareness.

## Prompt Assembly

The gateway builds the prompt from `IncomingMessage` fields (see [gateway.md](gateway.md) for full details). The adapter's job is just to populate the fields:

| Field | Prompt Contribution |
|-------|-------------------|
| `text` | Used directly as the prompt body |
| `image_paths` | Paths included so Claude can view them |
| `audio_paths` | Transcribed by whisper, text prepended to prompt |
| `file_paths` + `file_names` | Referenced in prompt as `` `filename`: /path/to/file `` |

## Typing Indicators

Both adapters implement `send_typing()`. The gateway calls this every 5 seconds while waiting for a Claude response:

- **Telegram:** Sends "typing" chat action
- **Discord:** Sends channel typing indicator

The typing loop runs as a separate spawned task, cancelled when the Claude call completes.

## Error Handling

**Telegram transient errors:** Network issues with Telegram's API ("Bad Gateway", "NetworkError", "TimedOut", "ServerError") are logged as warnings without stack traces. These are expected and transient — Telegram reconnects automatically via long polling.

**Discord reconnection:** Serenity handles WebSocket reconnection internally. The adapter doesn't need explicit reconnection logic.

**Send failures:** If `send_message()` fails, the error is logged but doesn't propagate to the caller. The user may not see the response, but the gateway continues processing the next message.
