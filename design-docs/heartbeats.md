# Heartbeats

Heartbeats are Stubert's dynamic monitoring loop. Every 30 minutes (configurable), the gateway reads `HEARTBEAT.md` fresh from disk and executes whatever instructions it contains via an ephemeral Claude CLI session. Unlike scheduled tasks, which are fixed YAML, heartbeats are flexible — edit the file at any time and the next tick picks up the changes.

Heartbeats are read-only by design. They monitor, check, and report — they never modify state.

## Configuration

```yaml
heartbeat:
  interval_minutes: 30
  file: "HEARTBEAT.md"
  allowed_tools: ["Bash(read-only)", "Read", "Glob", "Grep"]
  log_file: "logs/heartbeat.log"
  log_max_bytes: 5242880      # 5 MB
  log_backup_count: 3
```

## HeartbeatRunner

```rust
pub struct HeartbeatRunner {
    config: HeartbeatConfig,
    claude_config: ClaudeConfig,
    logger: HeartbeatLogger,
    lock: tokio::sync::Mutex<()>,
    last_execution: Option<Instant>,
    running: bool,
}

impl HeartbeatRunner {
    pub async fn start(&mut self);
    pub async fn stop(&mut self);
    pub async fn trigger(&self) -> Result<String, HeartbeatError>;
}
```

## Loop Behavior

The heartbeat loop runs as a spawned task:

```rust
async fn run_loop(&self, cancel: CancellationToken) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {
                if let Err(e) = self.execute_tick().await {
                    tracing::error!("Heartbeat tick failed: {}", e);
                }
            }
            _ = cancel.cancelled() => break,
        }
    }
}
```

Each tick:

1. **Acquire the mutex** (non-blocking `try_lock`). If locked, skip — a previous tick or manual `/heartbeat` is still running.
2. **Read `HEARTBEAT.md`** from disk.
3. **Filter comments and blanks.** Lines starting with `#` are comments. If all lines are comments or the file is empty/missing, skip execution (no CLI call).
4. **Create ephemeral session** — new UUID, never resumed.
5. **Call Claude** with the file contents as the prompt, using read-only tools.
6. **Log the result** via `HeartbeatLogger`.
7. **Update `last_execution`** timestamp (used by health endpoint and `/status`).
8. **Release the mutex.**

## Overlap Protection

A `tokio::sync::Mutex` prevents concurrent heartbeat executions. This mutex is shared between the loop and the `/heartbeat` command:

- **Loop tick:** Uses `try_lock()`. If the lock is held, the tick is skipped (logged as "skipped — already running").
- **`/heartbeat` command:** Uses `try_lock()`. If the lock is held, returns `"A heartbeat is already in progress."` to the user.

This means a manual trigger during a loop execution is rejected (and vice versa), but the loop will resume on the next interval.

## File Reading

```rust
fn read_heartbeat_file(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = content
        .lines()
        .filter(|line| !line.trim_start().starts_with('#') && !line.trim().is_empty())
        .collect();

    if lines.is_empty() {
        None  // Skip execution
    } else {
        Some(lines.join("\n"))
    }
}
```

This means heartbeats can be disabled by commenting out all instructions with `#`, without deleting the file.

## Ephemeral Sessions

Each heartbeat tick creates a fresh UUID session:

```rust
let session_id = Uuid::new_v4();
call_claude(ClaudeCallParams {
    prompt: heartbeat_content,
    session_id: session_id.to_string(),
    is_new_session: true,  // always --session-id, never --resume
    allowed_tools: Some(config.allowed_tools.clone()),
    model: None,  // uses default from claude config
    ..defaults_from_claude_config
}).await?;
```

Heartbeat sessions are never persisted in `sessions.json` and never resumed. Each tick is fully independent.

## Logging

The `HeartbeatLogger` writes to a dedicated log file (default `logs/heartbeat.log`):

```
[2026-02-23 12:00:05] heartbeat | OK | 12.3s
    System status: All services healthy. Disk usage at 45%.
[2026-02-23 12:30:00] heartbeat | SKIPPED | 0.0s
[2026-02-23 13:00:03] heartbeat | FAIL | 5.1s
    ClaudeCLIError: exit code 1
```

Format: `[timestamp] heartbeat | {OK|FAIL|SKIPPED} | {duration}s`

Response text is indented below the status line. Log rotation is configured by `log_max_bytes` and `log_backup_count`.

## HEARTBEAT.md Example

```markdown
# Heartbeat Instructions

Check the systemd journal for any stubert errors in the last hour.
Report the current disk usage of /home/cooper.
Summarize any new git commits in /home/cooper/stubert since the last check.
```

To disable temporarily:
```markdown
# Check the systemd journal for any stubert errors in the last hour.
# Report the current disk usage of /home/cooper.
# Summarize any new git commits in /home/cooper/stubert since the last check.
```

## Error Handling

- **File not found:** Skip (log info, not error). The file is optional.
- **CLI failure:** Log the error via `HeartbeatLogger`. Don't propagate — the loop continues on the next interval.
- **CLI timeout:** Same as CLI failure — logged and skipped.
- **Mutex contention:** Skip the tick, log as "SKIPPED".

Heartbeat failures never affect the main chat flow or other background tasks.

## Manual Trigger

The `/heartbeat` command calls `trigger()`, which:

1. Attempts `try_lock()` on the mutex
2. If locked: returns error (`"A heartbeat is already in progress."`)
3. If unlocked: executes the tick immediately and returns Claude's response text
4. Updates `last_execution` timestamp

This shares the same code path as the automatic loop tick, just initiated by the user.
