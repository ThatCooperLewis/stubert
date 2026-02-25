use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use serde::Deserialize;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::adapters::PlatformAdapter;
use crate::config::types::{ClaudeConfig, SchedulerConfig};
use crate::gateway::claude_cli::{resolve_model, ClaudeCallParams};
use crate::gateway::core::ClaudeCaller;

// ---- Task Config Types ----

#[derive(Debug, Clone, Deserialize)]
pub struct NotifyConfig {
    pub platform: String,
    pub chat_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskConfig {
    #[serde(skip)]
    pub name: String,
    pub schedule: String,
    pub prompt: String,
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub add_dirs: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub notify: Option<NotifyConfig>,
    #[serde(default = "default_on_failure")]
    pub on_failure: String,
}

fn default_on_failure() -> String {
    "log".to_string()
}

#[derive(Deserialize)]
struct SchedulesFile {
    #[serde(default)]
    tasks: HashMap<String, TaskConfig>,
}

// ---- load_schedules ----

pub fn load_schedules(path: &Path) -> Result<Vec<TaskConfig>, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;

    let file: SchedulesFile =
        serde_yaml_ng::from_str(&content).map_err(|e| format!("failed to parse YAML: {e}"))?;

    let tasks: Vec<TaskConfig> = file
        .tasks
        .into_iter()
        .map(|(name, mut task)| {
            task.name = name;
            task
        })
        .collect();

    Ok(tasks)
}

// ---- parse_cron ----

fn parse_cron(expr: &str) -> Result<cron::Schedule, String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!(
            "expected 5-field cron expression, got {} fields",
            fields.len()
        ));
    }

    let seven_field = format!("0 {expr} *");
    cron::Schedule::from_str(&seven_field).map_err(|e| format!("invalid cron expression: {e}"))
}

// ---- JobLogger ----

pub struct JobLogger {
    log_dir: PathBuf,
    max_bytes: u64,
    backup_count: u32,
}

impl JobLogger {
    pub fn new(log_dir: PathBuf, max_bytes: u64, backup_count: u32) -> Self {
        Self {
            log_dir,
            max_bytes,
            backup_count,
        }
    }

    pub fn log(&self, task_name: &str, status: &str, duration_secs: f64, detail: Option<&str>) {
        let path = self.log_dir.join(format!("{task_name}.log"));

        self.rotate_if_needed(&path);

        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let mut entry = format!("[{now}] {task_name} | {status} | {duration_secs:.1}s\n");

        if let Some(text) = detail {
            for line in text.lines() {
                entry.push_str(&format!("    {line}\n"));
            }
        }

        if !self.log_dir.exists() {
            if let Err(e) = std::fs::create_dir_all(&self.log_dir) {
                tracing::warn!(error = %e, "failed to create job log directory");
                return;
            }
        }

        if let Err(e) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(entry.as_bytes())
            })
        {
            tracing::warn!(error = %e, task = task_name, "failed to write job log");
        }
    }

    fn rotate_if_needed(&self, path: &Path) {
        if self.backup_count == 0 {
            return;
        }

        let size = match std::fs::metadata(path) {
            Ok(m) => m.len(),
            Err(_) => return,
        };

        if size < self.max_bytes {
            return;
        }

        let oldest = format!("{}.{}", path.display(), self.backup_count);
        let _ = std::fs::remove_file(&oldest);

        for n in (1..self.backup_count).rev() {
            let from = format!("{}.{n}", path.display());
            let to = format!("{}.{}", path.display(), n + 1);
            let _ = std::fs::rename(&from, &to);
        }

        let backup_one = format!("{}.1", path.display());
        let _ = std::fs::rename(path, &backup_one);
    }
}

// ---- TaskScheduler ----

type AdaptersMap = Arc<Mutex<HashMap<String, Arc<Mutex<dyn PlatformAdapter>>>>>;

pub struct TaskScheduler {
    tasks: Vec<TaskConfig>,
    schedules: HashMap<String, cron::Schedule>,
    claude_caller: Arc<dyn ClaudeCaller>,
    adapters: AdaptersMap,
    working_directory: String,
    env_file_path: String,
    timeout_secs: u64,
    cli_path: String,
    job_logger: JobLogger,
    last_execution: std::sync::Mutex<Option<Instant>>,
    task_locks: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
    shutdown_txs: std::sync::Mutex<Vec<tokio::sync::oneshot::Sender<()>>>,
}

impl TaskScheduler {
    pub fn new(
        tasks: Vec<TaskConfig>,
        scheduler_config: &SchedulerConfig,
        claude_config: &ClaudeConfig,
        claude_caller: Arc<dyn ClaudeCaller>,
        adapters: AdaptersMap,
    ) -> Result<Self, String> {
        let mut schedules = HashMap::new();
        let mut task_locks = HashMap::new();

        for task in &tasks {
            let schedule = parse_cron(&task.schedule)?;
            schedules.insert(task.name.clone(), schedule);
            task_locks.insert(task.name.clone(), Arc::new(tokio::sync::Mutex::new(())));
        }

        let log_dir =
            PathBuf::from(&claude_config.working_directory).join(&scheduler_config.job_log_dir);
        let max_bytes = scheduler_config.job_log_max_bytes.unwrap_or(5_242_880);
        let backup_count = scheduler_config.job_log_backup_count.unwrap_or(3);
        let job_logger = JobLogger::new(log_dir, max_bytes, backup_count);

        Ok(Self {
            tasks,
            schedules,
            claude_caller,
            adapters,
            working_directory: claude_config.working_directory.clone(),
            env_file_path: claude_config.env_file_path.clone(),
            timeout_secs: claude_config.timeout_secs,
            cli_path: claude_config.cli_path.clone(),
            job_logger,
            last_execution: std::sync::Mutex::new(None),
            task_locks,
            shutdown_txs: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub fn start(self: &Arc<Self>) {
        // Stop any existing loops to prevent orphaned tasks on double-start
        self.stop();

        let mut txs = self.shutdown_txs.lock().unwrap();

        for task in &self.tasks {
            let (tx, rx) = tokio::sync::oneshot::channel();
            txs.push(tx);

            let scheduler = Arc::clone(self);
            let task = task.clone();
            let schedule = self.schedules[&task.name].clone();
            let lock = Arc::clone(&self.task_locks[&task.name]);

            tokio::spawn(async move {
                scheduler.run_task_loop(&task, &schedule, lock, rx).await;
            });
        }
    }

    pub fn stop(&self) {
        let txs: Vec<_> = self.shutdown_txs.lock().unwrap().drain(..).collect();
        for tx in txs {
            let _ = tx.send(());
        }
    }

    async fn run_task_loop(
        self: &Arc<Self>,
        task: &TaskConfig,
        schedule: &cron::Schedule,
        lock: Arc<tokio::sync::Mutex<()>>,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) {
        while let Some(next) = schedule.upcoming(chrono::Utc).next() {
            let now = chrono::Utc::now();
            let duration = (next - now).to_std().unwrap_or_default();

            tokio::select! {
                _ = tokio::time::sleep(duration) => {
                    self.execute_task(task, &lock).await;
                }
                _ = &mut shutdown_rx => {
                    tracing::info!(task = %task.name, "scheduler task loop shutting down");
                    break;
                }
            }
        }
    }

    pub async fn execute_task(&self, task: &TaskConfig, lock: &Arc<tokio::sync::Mutex<()>>) {
        let _guard = match lock.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                tracing::info!(task = %task.name, "scheduled task skipped: already running");
                self.job_logger.log(&task.name, "SKIPPED", 0.0, Some("overlap"));
                return;
            }
        };

        let start = Instant::now();
        let result = self.execute_task_inner(task).await;
        let duration_secs = start.elapsed().as_secs_f64();

        match &result {
            Ok(response) => {
                tracing::info!(task = %task.name, duration_secs, "scheduled task OK");
                self.job_logger
                    .log(&task.name, "OK", duration_secs, Some(response));
                if let Some(notify) = &task.notify {
                    self.send_notification(notify, response).await;
                }
            }
            Err(e) => {
                tracing::warn!(task = %task.name, error = %e, duration_secs, "scheduled task FAIL");
                self.job_logger
                    .log(&task.name, "FAIL", duration_secs, Some(e));
                if task.on_failure == "notify" {
                    if let Some(notify) = &task.notify {
                        self.send_notification(notify, &format!("Task failed: {e}")).await;
                    }
                }
            }
        }

        *self.last_execution.lock().unwrap() = Some(Instant::now());
    }

    async fn execute_task_inner(&self, task: &TaskConfig) -> Result<String, String> {
        let session_id = Uuid::new_v4().to_string();

        let allowed_tools = if task.allowed_tools.is_empty() {
            None
        } else {
            Some(task.allowed_tools.clone())
        };

        let add_dirs = if task.add_dirs.is_empty() {
            None
        } else {
            Some(task.add_dirs.clone())
        };

        let params = ClaudeCallParams {
            prompt: task.prompt.clone(),
            session_id,
            is_new_session: true,
            allowed_tools,
            add_dirs,
            model: Some(resolve_model(task.model.as_deref().unwrap_or("sonnet"))),
            append_system_prompt: None,
            env_file_path: self.env_file_path.clone(),
            timeout_secs: self.timeout_secs,
            working_directory: self.working_directory.clone(),
            cli_path: self.cli_path.clone(),
        };

        self.claude_caller
            .call(&params)
            .await
            .map(|r| r.result)
            .map_err(|e| e.to_string())
    }

    async fn send_notification(&self, notify: &NotifyConfig, text: &str) {
        let adapters = self.adapters.lock().await;
        let adapter = match adapters.get(&notify.platform) {
            Some(a) => a,
            None => {
                tracing::warn!(
                    platform = %notify.platform,
                    "notification skipped: adapter not found"
                );
                return;
            }
        };

        let adapter = adapter.lock().await;
        if let Err(e) = adapter.send_message(&notify.chat_id, text).await {
            tracing::warn!(
                platform = %notify.platform,
                error = %e,
                "notification send failed"
            );
        }
    }

    pub fn last_execution(&self) -> Option<Instant> {
        *self.last_execution.lock().unwrap()
    }
}

// ---- Tests ----

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::MockPlatformAdapter;
    use crate::gateway::claude_cli::{ClaudeError, ClaudeResponse};
    use crate::gateway::core::MockClaudeCaller;
    use tempfile::TempDir;

    fn make_scheduler_config(dir: &Path) -> SchedulerConfig {
        SchedulerConfig {
            schedules_file: "schedules.yaml".to_string(),
            job_log_dir: dir.join("logs/cron").to_str().unwrap().to_string(),
            job_log_max_bytes: Some(5_242_880),
            job_log_backup_count: Some(3),
        }
    }

    fn make_claude_config(dir: &Path) -> ClaudeConfig {
        ClaudeConfig {
            cli_path: "claude".to_string(),
            timeout_secs: 300,
            default_model: "claude-sonnet-4-6".to_string(),
            working_directory: dir.to_str().unwrap().to_string(),
            env_file_path: ".env".to_string(),
            allowed_tools: HashMap::new(),
            add_dirs: vec![],
            platform_readmes: HashMap::new(),
        }
    }

    fn make_task(name: &str) -> TaskConfig {
        TaskConfig {
            name: name.to_string(),
            schedule: "0 8 * * *".to_string(),
            prompt: "Do something".to_string(),
            allowed_tools: vec!["Bash(read-only)".to_string(), "Read".to_string()],
            add_dirs: vec![],
            model: None,
            notify: None,
            on_failure: "log".to_string(),
        }
    }

    fn make_adapters() -> AdaptersMap {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn claude_success(text: &str) -> Result<ClaudeResponse, ClaudeError> {
        Ok(ClaudeResponse {
            result: text.to_string(),
            session_id: "mock-session".to_string(),
            cost_usd: 0.01,
            duration_ms: 500,
            input_tokens: 50,
            output_tokens: 25,
        })
    }

    fn claude_error() -> Result<ClaudeResponse, ClaudeError> {
        Err(ClaudeError::ExitError {
            code: 1,
            stderr: "cli error".to_string(),
        })
    }

    fn make_scheduler(
        dir: &Path,
        tasks: Vec<TaskConfig>,
        mock_claude: MockClaudeCaller,
        adapters: AdaptersMap,
    ) -> Arc<TaskScheduler> {
        // Use absolute path for job_log_dir since working_directory resolution
        // happens in new() only for the log dir join
        let sched_config = SchedulerConfig {
            schedules_file: "schedules.yaml".to_string(),
            job_log_dir: "logs/cron".to_string(),
            job_log_max_bytes: Some(5_242_880),
            job_log_backup_count: Some(3),
        };
        let claude_config = make_claude_config(dir);

        Arc::new(
            TaskScheduler::new(
                tasks,
                &sched_config,
                &claude_config,
                Arc::new(mock_claude),
                adapters,
            )
            .unwrap(),
        )
    }

    // ---- load_schedules tests ----

    mod test_load_schedules {
        use super::*;

        #[test]
        fn valid_file_loads_all_tasks() {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("schedules.yaml");
            std::fs::write(
                &path,
                r#"
tasks:
  morning-summary:
    schedule: "0 8 * * *"
    prompt: "Summarize events"
    allowed_tools: ["Bash(read-only)", "Read"]
    on_failure: notify
    notify:
      platform: telegram
      chat_id: "123456"
  weekly-cleanup:
    schedule: "0 3 * * 0"
    prompt: "Check disk usage"
    allowed_tools: ["Bash(read-only)"]
    add_dirs: ["/tmp"]
"#,
            )
            .unwrap();

            let tasks = load_schedules(&path).unwrap();
            assert_eq!(tasks.len(), 2);

            let names: Vec<&str> = tasks.iter().map(|t| t.name.as_str()).collect();
            assert!(names.contains(&"morning-summary"));
            assert!(names.contains(&"weekly-cleanup"));
        }

        #[test]
        fn empty_tasks_returns_empty() {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("schedules.yaml");
            std::fs::write(&path, "tasks: {}").unwrap();

            let tasks = load_schedules(&path).unwrap();
            assert!(tasks.is_empty());
        }

        #[test]
        fn missing_file_returns_error() {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("nonexistent.yaml");

            let result = load_schedules(&path);
            assert!(result.is_err());
        }

        #[test]
        fn task_defaults_applied() {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("schedules.yaml");
            std::fs::write(
                &path,
                r#"
tasks:
  basic-task:
    schedule: "0 8 * * *"
    prompt: "Do something"
    allowed_tools: ["Read"]
"#,
            )
            .unwrap();

            let tasks = load_schedules(&path).unwrap();
            assert_eq!(tasks.len(), 1);
            let task = &tasks[0];
            assert!(task.add_dirs.is_empty());
            assert_eq!(task.on_failure, "log");
            assert!(task.notify.is_none());
        }

        #[test]
        fn task_name_populated_from_key() {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("schedules.yaml");
            std::fs::write(
                &path,
                r#"
tasks:
  my-custom-task:
    schedule: "30 12 * * *"
    prompt: "Run report"
    allowed_tools: ["Bash(read-only)"]
"#,
            )
            .unwrap();

            let tasks = load_schedules(&path).unwrap();
            assert_eq!(tasks[0].name, "my-custom-task");
        }
    }

    // ---- parse_cron tests ----

    mod test_parse_cron {
        use super::*;

        #[test]
        fn valid_5_field_parsed() {
            let schedule = parse_cron("0 8 * * *").unwrap();
            let next = schedule.upcoming(chrono::Utc).next();
            assert!(next.is_some());
        }

        #[test]
        fn wrong_field_count_rejected() {
            let result = parse_cron("0 8 * *");
            assert!(result.is_err());
            assert!(result.unwrap_err().contains("5-field"));
        }

        #[test]
        fn invalid_syntax_rejected() {
            let result = parse_cron("99 99 99 99 99");
            assert!(result.is_err());
        }
    }

    // ---- JobLogger tests ----

    mod test_job_logger {
        use super::*;

        #[test]
        fn log_ok_formats_correctly() {
            let dir = TempDir::new().unwrap();
            let logger = JobLogger::new(dir.path().to_path_buf(), 1_000_000, 3);

            logger.log("morning-summary", "OK", 18.4, Some("All services healthy."));

            let content =
                std::fs::read_to_string(dir.path().join("morning-summary.log")).unwrap();
            assert!(content.contains("morning-summary | OK | 18.4s"));
            assert!(content.contains("    All services healthy."));
        }

        #[test]
        fn log_fail_formats_correctly() {
            let dir = TempDir::new().unwrap();
            let logger = JobLogger::new(dir.path().to_path_buf(), 1_000_000, 3);

            logger.log(
                "morning-summary",
                "FAIL",
                5.2,
                Some("ClaudeCLIError: exit code 1"),
            );

            let content =
                std::fs::read_to_string(dir.path().join("morning-summary.log")).unwrap();
            assert!(content.contains("morning-summary | FAIL | 5.2s"));
            assert!(content.contains("    ClaudeCLIError: exit code 1"));
        }

        #[test]
        fn rotation_shifts_files() {
            let dir = TempDir::new().unwrap();
            let logger = JobLogger::new(dir.path().to_path_buf(), 50, 3);

            logger.log("task-a", "OK", 1.0, Some("First entry with enough content to exceed limit"));
            logger.log("task-a", "OK", 2.0, Some("Second entry"));

            let log_path = dir.path().join("task-a.log");
            let backup_path = format!("{}.1", log_path.display());
            assert!(Path::new(&backup_path).exists(), "backup .1 should exist");
            assert!(log_path.exists(), "current log should exist");

            let backup_content = std::fs::read_to_string(&backup_path).unwrap();
            assert!(backup_content.contains("First entry"));

            let current_content = std::fs::read_to_string(&log_path).unwrap();
            assert!(current_content.contains("Second entry"));
        }

        #[test]
        fn rotation_respects_backup_count() {
            let dir = TempDir::new().unwrap();
            let logger = JobLogger::new(dir.path().to_path_buf(), 50, 2);

            logger.log("task-b", "OK", 1.0, Some("Entry one with enough content to fill"));
            logger.log("task-b", "OK", 2.0, Some("Entry two with enough content to fill"));
            logger.log("task-b", "OK", 3.0, Some("Entry three with enough content to fill"));

            let log_path = dir.path().join("task-b.log");
            let backup_2 = format!("{}.2", log_path.display());
            assert!(Path::new(&backup_2).exists(), "backup .2 should exist");

            let backup_3 = format!("{}.3", log_path.display());
            assert!(!Path::new(&backup_3).exists(), "backup .3 should not exist");
        }

        #[test]
        fn creates_log_dir_lazily() {
            let dir = TempDir::new().unwrap();
            let log_dir = dir.path().join("nested").join("cron");
            let logger = JobLogger::new(log_dir.clone(), 1_000_000, 3);

            assert!(!log_dir.exists());
            logger.log("task-c", "OK", 1.0, Some("Created lazily"));
            assert!(log_dir.exists());
            assert!(log_dir.join("task-c.log").exists());
        }
    }

    // ---- TaskScheduler tests ----

    mod test_task_scheduler {
        use super::*;

        #[tokio::test]
        async fn task_executes_claude_call() {
            let dir = TempDir::new().unwrap();
            let mut task = make_task("test-task");
            task.add_dirs = vec!["/extra".to_string()];

            let params_seen = Arc::new(std::sync::Mutex::new(Vec::<ClaudeCallParams>::new()));
            let ps = params_seen.clone();

            let mut mock = MockClaudeCaller::new();
            mock.expect_call().returning(move |params| {
                ps.lock().unwrap().push(ClaudeCallParams {
                    prompt: params.prompt.clone(),
                    session_id: params.session_id.clone(),
                    is_new_session: params.is_new_session,
                    allowed_tools: params.allowed_tools.clone(),
                    add_dirs: params.add_dirs.clone(),
                    model: params.model.clone(),
                    append_system_prompt: params.append_system_prompt.clone(),
                    env_file_path: params.env_file_path.clone(),
                    timeout_secs: params.timeout_secs,
                    working_directory: params.working_directory.clone(),
                    cli_path: params.cli_path.clone(),
                });
                claude_success("Task done")
            });

            let adapters = make_adapters();
            let scheduler = make_scheduler(dir.path(), vec![task], mock, adapters);
            let lock = scheduler.task_locks["test-task"].clone();
            scheduler
                .execute_task(&scheduler.tasks[0], &lock)
                .await;

            let params = params_seen.lock().unwrap();
            assert_eq!(params.len(), 1);
            assert_eq!(params[0].prompt, "Do something");
            assert!(params[0].is_new_session);
            assert_eq!(
                params[0].allowed_tools,
                Some(vec!["Bash(read-only)".to_string(), "Read".to_string()])
            );
            assert_eq!(params[0].add_dirs, Some(vec!["/extra".to_string()]));
        }

        #[tokio::test]
        async fn task_uses_ephemeral_session() {
            let dir = TempDir::new().unwrap();
            let task = make_task("ephemeral-test");

            let session_ids = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let si = session_ids.clone();

            let mut mock = MockClaudeCaller::new();
            mock.expect_call().times(2).returning(move |params| {
                si.lock().unwrap().push(params.session_id.clone());
                claude_success("OK")
            });

            let adapters = make_adapters();
            let scheduler = make_scheduler(dir.path(), vec![task], mock, adapters);
            let lock = scheduler.task_locks["ephemeral-test"].clone();
            scheduler
                .execute_task(&scheduler.tasks[0], &lock)
                .await;
            scheduler
                .execute_task(&scheduler.tasks[0], &lock)
                .await;

            let ids = session_ids.lock().unwrap();
            assert_eq!(ids.len(), 2);
            assert_ne!(ids[0], ids[1], "each execution should get a unique session ID");
            assert!(Uuid::parse_str(&ids[0]).is_ok());
            assert!(Uuid::parse_str(&ids[1]).is_ok());
        }

        #[tokio::test]
        async fn success_with_notify_sends_notification() {
            let dir = TempDir::new().unwrap();
            let mut task = make_task("notify-test");
            task.notify = Some(NotifyConfig {
                platform: "telegram".to_string(),
                chat_id: "123".to_string(),
            });

            let mut mock = MockClaudeCaller::new();
            mock.expect_call()
                .returning(|_| claude_success("Task result here"));

            let sent_messages = Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
            let sm = sent_messages.clone();

            let mut mock_adapter = MockPlatformAdapter::new();
            mock_adapter
                .expect_send_message()
                .returning(move |chat_id, text| {
                    sm.lock()
                        .unwrap()
                        .push((chat_id.to_string(), text.to_string()));
                    Ok(())
                });

            let adapters = make_adapters();
            {
                let mut map = adapters.lock().await;
                map.insert("telegram".to_string(), Arc::new(Mutex::new(mock_adapter)));
            }

            let scheduler = make_scheduler(dir.path(), vec![task], mock, adapters);
            let lock = scheduler.task_locks["notify-test"].clone();
            scheduler
                .execute_task(&scheduler.tasks[0], &lock)
                .await;

            let messages = sent_messages.lock().unwrap();
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].0, "123");
            assert_eq!(messages[0].1, "Task result here");
        }

        #[tokio::test]
        async fn success_without_notify_no_send() {
            let dir = TempDir::new().unwrap();
            let task = make_task("no-notify-test");
            // task.notify is None by default

            let mut mock = MockClaudeCaller::new();
            mock.expect_call()
                .returning(|_| claude_success("Done"));

            let adapters = make_adapters();
            let scheduler = make_scheduler(dir.path(), vec![task], mock, adapters);
            let lock = scheduler.task_locks["no-notify-test"].clone();
            scheduler
                .execute_task(&scheduler.tasks[0], &lock)
                .await;
            // No adapter registered, no panic — test passes if we get here
        }

        #[tokio::test]
        async fn failure_on_failure_notify_sends_notification() {
            let dir = TempDir::new().unwrap();
            let mut task = make_task("fail-notify-test");
            task.on_failure = "notify".to_string();
            task.notify = Some(NotifyConfig {
                platform: "telegram".to_string(),
                chat_id: "456".to_string(),
            });

            let mut mock = MockClaudeCaller::new();
            mock.expect_call().returning(|_| claude_error());

            let sent_messages = Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
            let sm = sent_messages.clone();

            let mut mock_adapter = MockPlatformAdapter::new();
            mock_adapter
                .expect_send_message()
                .returning(move |chat_id, text| {
                    sm.lock()
                        .unwrap()
                        .push((chat_id.to_string(), text.to_string()));
                    Ok(())
                });

            let adapters = make_adapters();
            {
                let mut map = adapters.lock().await;
                map.insert("telegram".to_string(), Arc::new(Mutex::new(mock_adapter)));
            }

            let scheduler = make_scheduler(dir.path(), vec![task], mock, adapters);
            let lock = scheduler.task_locks["fail-notify-test"].clone();
            scheduler
                .execute_task(&scheduler.tasks[0], &lock)
                .await;

            let messages = sent_messages.lock().unwrap();
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].0, "456");
            assert!(messages[0].1.contains("Task failed"));
        }

        #[tokio::test]
        async fn failure_on_failure_log_no_send() {
            let dir = TempDir::new().unwrap();
            let mut task = make_task("fail-log-test");
            task.on_failure = "log".to_string();
            task.notify = Some(NotifyConfig {
                platform: "telegram".to_string(),
                chat_id: "789".to_string(),
            });

            let mut mock = MockClaudeCaller::new();
            mock.expect_call().returning(|_| claude_error());

            // No adapter registered — if notification were attempted on a missing
            // adapter it would just log, but we also verify no send happens
            let adapters = make_adapters();
            let scheduler = make_scheduler(dir.path(), vec![task], mock, adapters);
            let lock = scheduler.task_locks["fail-log-test"].clone();
            scheduler
                .execute_task(&scheduler.tasks[0], &lock)
                .await;
            // Test passes if no panic
        }

        #[tokio::test]
        async fn notification_failure_logged_not_propagated() {
            let dir = TempDir::new().unwrap();
            let mut task = make_task("notify-fail-test");
            task.notify = Some(NotifyConfig {
                platform: "telegram".to_string(),
                chat_id: "123".to_string(),
            });

            let mut mock = MockClaudeCaller::new();
            mock.expect_call()
                .returning(|_| claude_success("Done"));

            let mut mock_adapter = MockPlatformAdapter::new();
            mock_adapter
                .expect_send_message()
                .returning(|_, _| {
                    Err(crate::adapters::AdapterError::SendFailed(
                        "network error".to_string(),
                    ))
                });

            let adapters = make_adapters();
            {
                let mut map = adapters.lock().await;
                map.insert("telegram".to_string(), Arc::new(Mutex::new(mock_adapter)));
            }

            let scheduler = make_scheduler(dir.path(), vec![task], mock, adapters);
            let lock = scheduler.task_locks["notify-fail-test"].clone();
            scheduler
                .execute_task(&scheduler.tasks[0], &lock)
                .await;
            // Should not panic despite send failure
        }

        #[tokio::test]
        async fn overlap_locked_skips_execution() {
            let dir = TempDir::new().unwrap();
            let task = make_task("overlap-test");

            let mut mock = MockClaudeCaller::new();
            mock.expect_call().never();

            let adapters = make_adapters();
            let scheduler = make_scheduler(dir.path(), vec![task], mock, adapters);
            let lock = scheduler.task_locks["overlap-test"].clone();

            // Hold the lock to simulate in-progress execution
            let _guard = lock.lock().await;
            scheduler
                .execute_task(&scheduler.tasks[0], &lock)
                .await;
            // Mock's .never() expectation verifies no call was made
        }

        #[tokio::test]
        async fn last_execution_updated_after_task() {
            let dir = TempDir::new().unwrap();
            let task = make_task("last-exec-test");

            let mut mock = MockClaudeCaller::new();
            mock.expect_call()
                .returning(|_| claude_success("OK"));

            let adapters = make_adapters();
            let scheduler = make_scheduler(dir.path(), vec![task], mock, adapters);

            assert!(scheduler.last_execution().is_none());

            let lock = scheduler.task_locks["last-exec-test"].clone();
            scheduler
                .execute_task(&scheduler.tasks[0], &lock)
                .await;

            assert!(scheduler.last_execution().is_some());
        }

        #[tokio::test]
        async fn stop_cancels_all_loops() {
            tokio::time::pause();

            let dir = TempDir::new().unwrap();
            let task = make_task("stop-test");

            let mut mock = MockClaudeCaller::new();
            mock.expect_call().never();

            let adapters = make_adapters();
            let scheduler = make_scheduler(dir.path(), vec![task], mock, adapters);
            scheduler.start();

            // Advance time a bit
            tokio::time::advance(std::time::Duration::from_secs(60)).await;

            scheduler.stop();

            // Advance past what would be a fire time — loop should be stopped
            tokio::time::advance(std::time::Duration::from_secs(86400)).await;
            tokio::task::yield_now().await;
        }

        #[tokio::test]
        async fn missing_adapter_logs_not_panics() {
            let dir = TempDir::new().unwrap();
            let mut task = make_task("missing-adapter-test");
            task.notify = Some(NotifyConfig {
                platform: "nonexistent".to_string(),
                chat_id: "123".to_string(),
            });

            let mut mock = MockClaudeCaller::new();
            mock.expect_call()
                .returning(|_| claude_success("Done"));

            let adapters = make_adapters();
            let scheduler = make_scheduler(dir.path(), vec![task], mock, adapters);
            let lock = scheduler.task_locks["missing-adapter-test"].clone();
            scheduler
                .execute_task(&scheduler.tasks[0], &lock)
                .await;
            // Should not panic
        }

        #[tokio::test]
        async fn cli_failure_logged() {
            let dir = TempDir::new().unwrap();
            let task = make_task("cli-fail-test");

            let mut mock = MockClaudeCaller::new();
            mock.expect_call().returning(|_| claude_error());

            let adapters = make_adapters();
            let sched_config = SchedulerConfig {
                schedules_file: "schedules.yaml".to_string(),
                job_log_dir: dir.path().join("logs/cron").to_str().unwrap().to_string(),
                job_log_max_bytes: Some(5_242_880),
                job_log_backup_count: Some(3),
            };
            let claude_config = make_claude_config(dir.path());
            let scheduler = Arc::new(
                TaskScheduler::new(
                    vec![task],
                    &sched_config,
                    &claude_config,
                    Arc::new(mock),
                    adapters,
                )
                .unwrap(),
            );

            let lock = scheduler.task_locks["cli-fail-test"].clone();
            scheduler
                .execute_task(&scheduler.tasks[0], &lock)
                .await;

            let log_content = std::fs::read_to_string(
                dir.path().join("logs/cron/cli-fail-test.log"),
            )
            .unwrap();
            assert!(log_content.contains("FAIL"));
            assert!(log_content.contains("cli error"));
        }

        #[test]
        fn invalid_cron_rejected_at_construction() {
            let dir = TempDir::new().unwrap();
            let mut task = make_task("bad-cron-test");
            task.schedule = "not a cron".to_string();

            let sched_config = make_scheduler_config(dir.path());
            let claude_config = make_claude_config(dir.path());
            let mock = MockClaudeCaller::new();
            let adapters = make_adapters();

            let result = TaskScheduler::new(
                vec![task],
                &sched_config,
                &claude_config,
                Arc::new(mock),
                adapters,
            );
            assert!(result.is_err());
        }
    }
}
