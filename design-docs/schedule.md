# Scheduled Operations

Stubert runs statically defined tasks on configurable cron schedules. Tasks are defined in `schedules.yaml`, loaded once at gateway startup, and executed by a cron scheduler. Each task spawns an independent Claude CLI call with its own ephemeral session, prompt, tool permissions, and directory access.

## Configuration

### schedules.yaml

```yaml
tasks:
  morning-summary:
    schedule: "0 8 * * *"
    prompt: "Summarize any notable system events from the last 24 hours."
    allowed_tools: ["Bash(read-only)", "Read", "Glob", "Grep"]
    on_failure: notify
    notify:
      platform: telegram
      chat_id: "123456789"

  weekly-cleanup:
    schedule: "0 3 * * 0"
    prompt: "Check disk usage and report any directories over 1GB in /tmp."
    allowed_tools: ["Bash(read-only)", "Read"]
    add_dirs: ["/tmp"]
    on_failure: log
```

### Task Config

```rust
pub struct TaskConfig {
    pub name: String,
    pub schedule: String,             // 5-field cron: minute hour day month day_of_week
    pub prompt: String,
    pub allowed_tools: Vec<String>,
    pub add_dirs: Vec<String>,        // default: empty
    pub notify: Option<NotifyConfig>, // default: None
    pub on_failure: String,           // "log" (default) or "notify"
}

pub struct NotifyConfig {
    pub platform: String,             // "telegram" or "discord"
    pub chat_id: String,
}
```

**Schedule format:** Standard 5-field cron — `minute hour day month day_of_week`. See crontab.guru for reference.

**Validation:** The scheduler validates the cron expression at startup. Invalid field count or syntax raises an error and prevents the task from being registered.

## Scheduler Config

```yaml
scheduler:
  schedules_file: "schedules.yaml"
  job_log_dir: "logs/cron"
  job_log_max_bytes: 5242880    # 5 MB
  job_log_backup_count: 3
```

## TaskScheduler

```rust
pub struct TaskScheduler {
    tasks: Vec<TaskConfig>,
    claude_config: ClaudeConfig,
    adapters: HashMap<String, Box<dyn PlatformAdapter>>,
    job_logger: JobLogger,
    last_execution: Option<Instant>,
}

impl TaskScheduler {
    pub async fn start(&mut self);
    pub async fn stop(&mut self);
}
```

**Rust crate:** `tokio-cron-scheduler` (or a hand-rolled cron loop using `cron` crate for parsing + `tokio::time::sleep_until` for waiting).

## Execution Flow

For each scheduled task when its cron trigger fires:

1. **Create ephemeral session** — new UUID, never resumed
2. **Call Claude** with `task.prompt`, `task.allowed_tools`, `task.add_dirs`
3. **Log result** via `JobLogger`
4. **On success + notify configured:** Send Claude's response to the configured platform/chat
5. **On failure + `on_failure: notify`:** Send error notification to the configured platform/chat
6. **On failure + `on_failure: log` (default):** Log only, no notification
7. **Update `last_execution`** timestamp

```rust
async fn execute_task(&self, task: &TaskConfig) {
    let session_id = Uuid::new_v4();
    let start = Instant::now();

    let result = call_claude(ClaudeCallParams {
        prompt: task.prompt.clone(),
        session_id: session_id.to_string(),
        is_new_session: true,
        allowed_tools: Some(task.allowed_tools.clone()),
        add_dirs: if task.add_dirs.is_empty() { None } else { Some(task.add_dirs.clone()) },
        ..defaults_from_claude_config
    }).await;

    let duration = start.elapsed();

    match result {
        Ok(response) => {
            self.job_logger.log_success(&task.name, &response.result, duration);
            if let Some(notify) = &task.notify {
                self.send_notification(notify, &response.result).await;
            }
        }
        Err(e) => {
            self.job_logger.log_failure(&task.name, &e, duration);
            if task.on_failure == "notify" {
                if let Some(notify) = &task.notify {
                    self.send_notification(notify, &format!("Task failed: {}", e)).await;
                }
            }
        }
    }

    self.last_execution = Some(Instant::now());
}
```

## Concurrency Control

Each task is limited to one concurrent instance. If a task's cron trigger fires while its previous execution is still running, the new execution is skipped. This prevents long-running tasks from stacking up.

## Ephemeral Sessions

Scheduled tasks never reuse sessions. Each execution gets a fresh UUID and uses `--session-id` (not `--resume`). Sessions are not persisted in `sessions.json`.

## Job Logging

The `JobLogger` writes per-task log files:

```
logs/cron/
├── morning-summary.log
├── weekly-cleanup.log
└── ...
```

**Format:**

```
[2026-02-23 08:00:15] morning-summary | OK | 18.4s
    Summary: No notable events in the last 24 hours. All services healthy.
[2026-02-23 08:00:12] morning-summary | FAIL | 5.2s
    ClaudeCLIError: exit code 1 — subprocess timed out
```

Format: `[timestamp] {task_name} | {OK|FAIL} | {duration}s`

Response text is indented below the status line. Log rotation is configured by `job_log_max_bytes` and `job_log_backup_count`.

Loggers are created lazily — the first execution of a task creates its log file.

## Notifications

When `notify` is configured on a task:

```yaml
notify:
  platform: telegram
  chat_id: "123456789"
```

The scheduler looks up the adapter by platform name and calls `adapter.send_message(chat_id, text)`.

**Success notifications:** Sent with Claude's response text.
**Failure notifications:** Only sent when `on_failure: notify`. Includes the error message.

If the adapter is not registered or `send_message` fails, the notification failure is logged but doesn't affect the task result.

## Differences from Heartbeats

| Aspect | Heartbeats | Scheduled Tasks |
|--------|-----------|-----------------|
| Configuration | Free-form markdown file | Structured YAML |
| Schedule | Fixed interval (N minutes) | Cron expressions (minute/hour/day) |
| Hot-reload | Yes (file read each tick) | No (requires restart) |
| Tools | Single set from config | Per-task from schedules.yaml |
| Notifications | None | Optional per-task |
| Overlap protection | Mutex (skip if running) | Max instances = 1 per task |
| Logging | Single heartbeat.log | Per-task files in logs/cron/ |

## Runtime Modification

The user can edit `schedules.yaml` at any time, but changes only take effect after a gateway restart. There is no hot-reload for scheduled tasks — the cron jobs are registered at startup and run until shutdown.
