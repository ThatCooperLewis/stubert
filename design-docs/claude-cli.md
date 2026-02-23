# Claude CLI Wrapper

Stubert invokes the Claude Code CLI as a subprocess for every interaction — chat messages, heartbeats, scheduled tasks, and several slash commands. This module is the single interface between Stubert and Claude.

## Interface

```rust
pub struct ClaudeResponse {
    pub result: String,
    pub session_id: String,
    pub cost_usd: f64,
    pub duration_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

pub struct ClaudeCallParams {
    pub prompt: String,
    pub session_id: String,
    pub is_new_session: bool,
    pub allowed_tools: Option<Vec<String>>,
    pub add_dirs: Option<Vec<String>>,
    pub model: Option<String>,
    pub env_file_path: String,       // default: ".env"
    pub timeout_secs: u64,           // default: 300
    pub working_directory: String,   // default: "."
    pub cli_path: String,            // default: "claude"
}

pub async fn call_claude(params: ClaudeCallParams) -> Result<ClaudeResponse, ClaudeError>;
```

## Command Construction

The CLI is always invoked in non-interactive mode with JSON output:

```
{cli_path} -p {prompt} --output-format json {session_flag} {session_id} [options...]
```

**Argument assembly order:**

1. Base: `[cli_path, "-p", prompt, "--output-format", "json"]`
2. Session flag: `["--session-id", session_id]` if `is_new_session`, else `["--resume", session_id]`
3. Tools: `["--allowedTools"] + tools` if `allowed_tools` is set
4. Directories: `["--add-dir", dir]` for each entry in `add_dirs`
5. Model: `["--model", model]` if `model` is set

**Environment variables set on the subprocess:**

| Variable | Value | Purpose |
|----------|-------|---------|
| `CLAUDE_ENV_FILE` | `env_file_path` | CLI sources this file for Bash commands |
| `SHELL` | `/bin/bash` | Ensures CLI uses bash for Bash tool |

The subprocess inherits all other environment variables from the parent process.

## JSON Response Parsing

The CLI writes a JSON object to stdout. Required fields:

```json
{
  "type": "result",
  "subtype": "success",
  "result": "Claude's response text",
  "session_id": "uuid",
  "cost_usd": 0.042,
  "duration_ms": 12345,
  "usage": {
    "input_tokens": 1500,
    "output_tokens": 800
  }
}
```

**Error conditions (all produce `ClaudeError`):**

- Non-zero exit code — stderr decoded as error message
- stdout is not valid JSON
- `subtype` is not `"success"`
- Any required field is missing

## Session Flags

The CLI uses two mutually exclusive flags for session management:

| Flag | When | Effect |
|------|------|--------|
| `--session-id {uuid}` | First message in a session (`is_new_session = true`) | Creates a new CLI session with this ID |
| `--resume {uuid}` | Subsequent messages (`is_new_session = false`) | Resumes the existing CLI session |

The caller (Gateway, heartbeat, scheduler) is responsible for tracking which flag to use via the `Session.initiated` flag.

## Model Aliasing

Short aliases map to full model IDs. The mapping is maintained alongside the CLI wrapper:

| Alias | Full Model ID |
|-------|---------------|
| `sonnet` | `claude-sonnet-4-6` |
| `opus` | `claude-opus-4-6` |
| `haiku` | `claude-haiku-4-5-20251001` |

**Display names** for user-facing output:

| Full Model ID | Display Name |
|---------------|--------------|
| `claude-sonnet-4-6` | Sonnet 4.6 |
| `claude-opus-4-6` | Opus 4.6 |
| `claude-haiku-4-5-20251001` | Haiku 4.5 |

Functions:

```rust
pub fn resolve_model(alias: &str) -> String;    // "sonnet" → "claude-sonnet-4-6"
pub fn display_model(model_id: &str) -> String;  // "claude-sonnet-4-6" → "Sonnet 4.6"
```

If the input doesn't match a known alias, `resolve_model` returns it unchanged (allowing full model IDs in config). If the input doesn't match a known model ID, `display_model` returns it unchanged.

## CLI Binary Path

The `cli_path` field in config defaults to `"claude"` (found via `$PATH`). It can be overridden with an absolute path for environments where the CLI isn't on `$PATH` — Docker containers, systemd services, NixOS.

```yaml
claude:
  cli_path: "/usr/local/bin/claude"  # absolute path override
```

## Timeout Handling

The subprocess is given `timeout_secs` to complete (default 300). On timeout:

1. Kill the subprocess (`Child::kill()`)
2. Return a timeout-specific error variant

The caller (Gateway) translates this into a user-facing message like "Claude timed out after 300s."

## Error Type

```rust
pub enum ClaudeError {
    /// Non-zero exit code from CLI
    ExitError { code: i32, stderr: String },
    /// stdout was not valid JSON or missing required fields
    ParseError(String),
    /// CLI returned subtype != "success"
    CliFailure(String),
    /// Subprocess exceeded timeout
    Timeout { timeout_secs: u64 },
    /// Failed to spawn subprocess
    SpawnError(std::io::Error),
}
```

## Subprocess Execution

Using `tokio::process::Command`:

```rust
let mut cmd = Command::new(&params.cli_path);
cmd.args(&args)
    .env("CLAUDE_ENV_FILE", &params.env_file_path)
    .env("SHELL", "/bin/bash")
    .current_dir(&params.working_directory)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

let child = cmd.spawn()?;
let output = tokio::time::timeout(
    Duration::from_secs(params.timeout_secs),
    child.wait_with_output(),
).await;
```

## Working Directory

The subprocess runs with `current_dir` set to `working_directory` from config (default `"."`). This is the runtime directory — the same directory containing `CLAUDE.md`, `SOUL.md`, etc. The CLI automatically picks up `CLAUDE.md` from its working directory, which in turn `@import`s the memory files.

## Usage by Other Modules

| Caller | Session Type | Tools | Notes |
|--------|-------------|-------|-------|
| Gateway (chat) | Persistent (resume) | Platform-specific from config | Main chat flow |
| CommandHandler (`/context`) | Persistent (resume) | Platform-specific from config | Reads context window usage |
| CommandHandler (`/skill`) | Persistent (resume) | Skill-specific from frontmatter | May override default tools |
| HeartbeatRunner | Ephemeral (new UUID each tick) | Read-only from heartbeat config | Never resumed |
| TaskScheduler | Ephemeral (new UUID each run) | Task-specific from schedules.yaml | Never resumed |
| Gateway (restart greeting) | Ephemeral (new UUID) | Platform-specific from config | One-shot after restart |
