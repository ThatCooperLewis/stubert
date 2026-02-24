use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use uuid::Uuid;

use crate::config::types::{ClaudeConfig, HeartbeatConfig};
use crate::gateway::claude_cli::ClaudeCallParams;
use crate::gateway::commands::HeartbeatTrigger;
use crate::gateway::core::ClaudeCaller;

// ---- HeartbeatLogger ----

pub struct HeartbeatLogger {
    log_file: Option<PathBuf>,
    max_bytes: u64,
    backup_count: u32,
}

impl HeartbeatLogger {
    pub fn new(log_file: Option<PathBuf>, max_bytes: u64, backup_count: u32) -> Self {
        Self {
            log_file,
            max_bytes,
            backup_count,
        }
    }

    pub fn log(&self, status: &str, duration_secs: f64, detail: Option<&str>) {
        let path = match &self.log_file {
            Some(p) => p,
            None => return,
        };

        self.rotate_if_needed(path);

        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let mut entry = format!("[{now}] heartbeat | {status} | {duration_secs:.1}s\n");

        if let Some(text) = detail {
            for line in text.lines() {
                entry.push_str(&format!("    {line}\n"));
            }
        }

        if let Some(parent) = path.parent() {
            if !parent.exists() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    tracing::warn!(error = %e, "failed to create heartbeat log directory");
                    return;
                }
            }
        }

        if let Err(e) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(entry.as_bytes())
            })
        {
            tracing::warn!(error = %e, "failed to write heartbeat log");
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

        // Delete oldest backup if it exists
        let oldest = format!("{}.{}", path.display(), self.backup_count);
        let _ = std::fs::remove_file(&oldest);

        // Shift existing backups: file.N -> file.N+1
        for n in (1..self.backup_count).rev() {
            let from = format!("{}.{n}", path.display());
            let to = format!("{}.{}", path.display(), n + 1);
            let _ = std::fs::rename(&from, &to);
        }

        // Rename current file to .1
        let backup_one = format!("{}.1", path.display());
        let _ = std::fs::rename(path, &backup_one);
    }
}

// ---- read_heartbeat_file ----

fn read_heartbeat_file(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;

    let lines: Vec<&str> = content
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with('#')
        })
        .collect();

    if lines.is_empty() {
        return None;
    }

    Some(lines.join("\n"))
}

// ---- HeartbeatRunner ----

pub struct HeartbeatRunner {
    heartbeat_config: HeartbeatConfig,
    claude_caller: Arc<dyn ClaudeCaller>,
    working_directory: String,
    env_file_path: String,
    timeout_secs: u64,
    cli_path: String,
    logger: HeartbeatLogger,
    lock: tokio::sync::Mutex<()>,
    last_execution: std::sync::Mutex<Option<Instant>>,
    shutdown_tx: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl HeartbeatRunner {
    pub fn new(
        heartbeat_config: HeartbeatConfig,
        claude_config: &ClaudeConfig,
        claude_caller: Arc<dyn ClaudeCaller>,
    ) -> Self {
        let log_file = heartbeat_config
            .log_file
            .as_ref()
            .map(|f| PathBuf::from(&claude_config.working_directory).join(f));

        let max_bytes = heartbeat_config.log_max_bytes.unwrap_or(5_242_880);
        let backup_count = heartbeat_config.log_backup_count.unwrap_or(3);

        let logger = HeartbeatLogger::new(log_file, max_bytes, backup_count);

        Self {
            heartbeat_config,
            claude_caller,
            working_directory: claude_config.working_directory.clone(),
            env_file_path: claude_config.env_file_path.clone(),
            timeout_secs: claude_config.timeout_secs,
            cli_path: claude_config.cli_path.clone(),
            logger,
            lock: tokio::sync::Mutex::new(()),
            last_execution: std::sync::Mutex::new(None),
            shutdown_tx: std::sync::Mutex::new(None),
        }
    }

    pub fn start(self: &Arc<Self>) {
        self.stop(); // cancel any existing loop before starting a new one
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut shutdown = self.shutdown_tx.lock().unwrap();
            *shutdown = Some(tx);
        }

        let runner = Arc::clone(self);
        tokio::spawn(async move {
            runner.run_loop(rx).await;
        });
    }

    pub fn stop(&self) {
        let tx = self.shutdown_tx.lock().unwrap().take();
        if let Some(tx) = tx {
            let _ = tx.send(());
        }
    }

    async fn run_loop(self: &Arc<Self>, mut shutdown_rx: tokio::sync::oneshot::Receiver<()>) {
        let interval = std::time::Duration::from_secs(self.heartbeat_config.interval_minutes * 60);

        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {
                    self.execute_tick().await;
                }
                _ = &mut shutdown_rx => {
                    tracing::info!("heartbeat loop shutting down");
                    break;
                }
            }
        }
    }

    async fn execute_tick(&self) {
        let _guard = match self.lock.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                tracing::info!("heartbeat tick skipped: already running");
                self.logger.log("SKIPPED", 0.0, Some("overlap"));
                return;
            }
        };

        let start = Instant::now();
        let result = self.execute_tick_inner().await;
        let duration_secs = start.elapsed().as_secs_f64();

        match &result {
            Ok(response) => {
                tracing::info!(duration_secs, "heartbeat tick OK");
                self.logger.log("OK", duration_secs, Some(response));
                *self.last_execution.lock().unwrap() = Some(Instant::now());
            }
            Err(e) if e == "skipped" => {
                tracing::info!("heartbeat tick skipped: empty/missing file");
                self.logger.log("SKIPPED", duration_secs, None);
            }
            Err(e) => {
                tracing::warn!(error = %e, duration_secs, "heartbeat tick FAIL");
                self.logger.log("FAIL", duration_secs, Some(e));
            }
        }
    }

    async fn execute_tick_inner(&self) -> Result<String, String> {
        let heartbeat_path =
            PathBuf::from(&self.working_directory).join(&self.heartbeat_config.file);

        let prompt = read_heartbeat_file(&heartbeat_path).ok_or_else(|| "skipped".to_string())?;

        let session_id = Uuid::new_v4().to_string();

        let allowed_tools = if self.heartbeat_config.allowed_tools.is_empty() {
            None
        } else {
            Some(self.heartbeat_config.allowed_tools.clone())
        };

        let params = ClaudeCallParams {
            prompt,
            session_id,
            is_new_session: true,
            allowed_tools,
            add_dirs: None,
            model: None,
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

    pub fn last_execution(&self) -> Option<Instant> {
        *self.last_execution.lock().unwrap()
    }
}

#[async_trait]
impl HeartbeatTrigger for HeartbeatRunner {
    async fn trigger(&self) -> Result<String, String> {
        let _guard = self
            .lock
            .try_lock()
            .map_err(|_| "A heartbeat is already in progress.".to_string())?;

        let start = Instant::now();
        let result = self.execute_tick_inner().await;
        let duration_secs = start.elapsed().as_secs_f64();

        match &result {
            Ok(response) => {
                self.logger.log("OK", duration_secs, Some(response));
                *self.last_execution.lock().unwrap() = Some(Instant::now());
            }
            Err(e) if e == "skipped" => {
                self.logger.log("SKIPPED", duration_secs, None);
            }
            Err(e) => {
                self.logger.log("FAIL", duration_secs, Some(e));
            }
        }

        result
    }

    fn is_running(&self) -> bool {
        self.lock.try_lock().is_err()
    }

    fn last_execution(&self) -> Option<Instant> {
        *self.last_execution.lock().unwrap()
    }
}

// ---- Tests ----

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{ClaudeConfig, HeartbeatConfig};
    use crate::gateway::claude_cli::{ClaudeError, ClaudeResponse};
    use crate::gateway::core::MockClaudeCaller;
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn make_heartbeat_config() -> HeartbeatConfig {
        HeartbeatConfig {
            interval_minutes: 5,
            file: "HEARTBEAT.md".to_string(),
            allowed_tools: vec![
                "Bash(read-only)".into(),
                "Read".into(),
            ],
            log_file: None,
            log_max_bytes: None,
            log_backup_count: None,
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

    fn make_runner(
        dir: &Path,
        mock_claude: MockClaudeCaller,
    ) -> Arc<HeartbeatRunner> {
        let hb_config = make_heartbeat_config();
        let claude_config = make_claude_config(dir);
        Arc::new(HeartbeatRunner::new(
            hb_config,
            &claude_config,
            Arc::new(mock_claude),
        ))
    }

    fn make_runner_with_config(
        dir: &Path,
        hb_config: HeartbeatConfig,
        mock_claude: MockClaudeCaller,
    ) -> Arc<HeartbeatRunner> {
        let claude_config = make_claude_config(dir);
        Arc::new(HeartbeatRunner::new(
            hb_config,
            &claude_config,
            Arc::new(mock_claude),
        ))
    }

    // ---- HeartbeatLogger tests ----

    mod test_heartbeat_logger {
        use super::*;

        #[test]
        fn log_ok_formats_correctly() {
            let dir = TempDir::new().unwrap();
            let log_path = dir.path().join("heartbeat.log");
            let logger = HeartbeatLogger::new(Some(log_path.clone()), 1_000_000, 3);

            logger.log("OK", 12.3, Some("All services healthy."));

            let content = std::fs::read_to_string(&log_path).unwrap();
            assert!(content.contains("heartbeat | OK | 12.3s"));
            assert!(content.contains("    All services healthy."));
        }

        #[test]
        fn log_fail_formats_correctly() {
            let dir = TempDir::new().unwrap();
            let log_path = dir.path().join("heartbeat.log");
            let logger = HeartbeatLogger::new(Some(log_path.clone()), 1_000_000, 3);

            logger.log("FAIL", 5.1, Some("ClaudeCLIError: exit code 1"));

            let content = std::fs::read_to_string(&log_path).unwrap();
            assert!(content.contains("heartbeat | FAIL | 5.1s"));
            assert!(content.contains("    ClaudeCLIError: exit code 1"));
        }

        #[test]
        fn log_skipped_formats_correctly() {
            let dir = TempDir::new().unwrap();
            let log_path = dir.path().join("heartbeat.log");
            let logger = HeartbeatLogger::new(Some(log_path.clone()), 1_000_000, 3);

            logger.log("SKIPPED", 0.0, None);

            let content = std::fs::read_to_string(&log_path).unwrap();
            assert!(content.contains("heartbeat | SKIPPED | 0.0s"));
            // No detail line
            let lines: Vec<&str> = content.lines().collect();
            assert_eq!(lines.len(), 1);
        }

        #[test]
        fn rotation_shifts_files() {
            let dir = TempDir::new().unwrap();
            let log_path = dir.path().join("heartbeat.log");
            let logger = HeartbeatLogger::new(Some(log_path.clone()), 50, 3);

            // Write enough to exceed 50 bytes
            logger.log("OK", 1.0, Some("First entry with enough content to exceed limit"));

            // This write should trigger rotation of the first file
            logger.log("OK", 2.0, Some("Second entry"));

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
            let log_path = dir.path().join("heartbeat.log");
            let logger = HeartbeatLogger::new(Some(log_path.clone()), 50, 2);

            // Write three entries to trigger multiple rotations
            logger.log("OK", 1.0, Some("Entry one with enough content to fill"));
            logger.log("OK", 2.0, Some("Entry two with enough content to fill"));
            logger.log("OK", 3.0, Some("Entry three with enough content to fill"));

            // .2 should exist (backup_count=2)
            let backup_2 = format!("{}.2", log_path.display());
            assert!(Path::new(&backup_2).exists(), "backup .2 should exist");

            // .3 should NOT exist (beyond backup_count)
            let backup_3 = format!("{}.3", log_path.display());
            assert!(!Path::new(&backup_3).exists(), "backup .3 should not exist");
        }
    }

    // ---- read_heartbeat_file tests ----

    mod test_read_heartbeat_file {
        use super::*;

        #[test]
        fn valid_file_returns_content() {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("HEARTBEAT.md");
            std::fs::write(&path, "Check CPU usage\nCheck disk space").unwrap();

            let result = read_heartbeat_file(&path);
            assert_eq!(result, Some("Check CPU usage\nCheck disk space".to_string()));
        }

        #[test]
        fn all_comments_returns_none() {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("HEARTBEAT.md");
            std::fs::write(&path, "# This is a comment\n# Another comment").unwrap();

            assert_eq!(read_heartbeat_file(&path), None);
        }

        #[test]
        fn empty_file_returns_none() {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("HEARTBEAT.md");
            std::fs::write(&path, "").unwrap();

            assert_eq!(read_heartbeat_file(&path), None);
        }

        #[test]
        fn missing_file_returns_none() {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("NONEXISTENT.md");

            assert_eq!(read_heartbeat_file(&path), None);
        }

        #[test]
        fn mixed_comments_and_content() {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("HEARTBEAT.md");
            std::fs::write(
                &path,
                "# Heartbeat instructions\nCheck services\n# More comments\nReport status",
            )
            .unwrap();

            let result = read_heartbeat_file(&path);
            assert_eq!(result, Some("Check services\nReport status".to_string()));
        }
    }

    // ---- HeartbeatRunner tests ----

    mod test_heartbeat_runner {
        use super::*;

        #[tokio::test]
        async fn tick_executes_claude_call() {
            let dir = TempDir::new().unwrap();
            std::fs::write(dir.path().join("HEARTBEAT.md"), "Check all services").unwrap();

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
                claude_success("All healthy")
            });

            let runner = make_runner(dir.path(), mock);
            runner.execute_tick().await;

            let params = params_seen.lock().unwrap();
            assert_eq!(params.len(), 1);
            assert_eq!(params[0].prompt, "Check all services");
            assert!(params[0].is_new_session);
            assert_eq!(
                params[0].allowed_tools,
                Some(vec!["Bash(read-only)".to_string(), "Read".to_string()])
            );
        }

        #[tokio::test]
        async fn empty_file_skips_cli_call() {
            let dir = TempDir::new().unwrap();
            std::fs::write(dir.path().join("HEARTBEAT.md"), "").unwrap();

            let mut mock = MockClaudeCaller::new();
            mock.expect_call().never();

            let runner = make_runner(dir.path(), mock);
            runner.execute_tick().await;
        }

        #[tokio::test]
        async fn all_comment_file_skips_cli_call() {
            let dir = TempDir::new().unwrap();
            std::fs::write(dir.path().join("HEARTBEAT.md"), "# just a comment\n# another").unwrap();

            let mut mock = MockClaudeCaller::new();
            mock.expect_call().never();

            let runner = make_runner(dir.path(), mock);
            runner.execute_tick().await;
        }

        #[tokio::test]
        async fn missing_file_skips_cli_call() {
            let dir = TempDir::new().unwrap();
            // Don't create HEARTBEAT.md

            let mut mock = MockClaudeCaller::new();
            mock.expect_call().never();

            let runner = make_runner(dir.path(), mock);
            runner.execute_tick().await;
        }

        #[tokio::test]
        async fn overlap_locked_skips_tick() {
            let dir = TempDir::new().unwrap();
            std::fs::write(dir.path().join("HEARTBEAT.md"), "Check services").unwrap();

            let mut mock = MockClaudeCaller::new();
            mock.expect_call().never();

            let runner = make_runner(dir.path(), mock);

            // Acquire the lock to simulate an in-progress heartbeat
            let _guard = runner.lock.lock().await;
            runner.execute_tick().await;
            // Mock's .never() expectation verifies no call was made
        }

        #[tokio::test]
        async fn manual_trigger_when_idle() {
            let dir = TempDir::new().unwrap();
            std::fs::write(dir.path().join("HEARTBEAT.md"), "Check services").unwrap();

            let mut mock = MockClaudeCaller::new();
            mock.expect_call()
                .returning(|_| claude_success("All good"));

            let runner = make_runner(dir.path(), mock);
            let result = runner.trigger().await;

            assert_eq!(result, Ok("All good".to_string()));
        }

        #[tokio::test]
        async fn manual_trigger_while_locked() {
            let dir = TempDir::new().unwrap();
            std::fs::write(dir.path().join("HEARTBEAT.md"), "Check services").unwrap();

            let mut mock = MockClaudeCaller::new();
            mock.expect_call().never();

            let runner = make_runner(dir.path(), mock);

            let _guard = runner.lock.lock().await;
            let result = runner.trigger().await;

            assert_eq!(
                result,
                Err("A heartbeat is already in progress.".to_string())
            );
        }

        #[tokio::test]
        async fn is_running_reflects_lock_state() {
            let dir = TempDir::new().unwrap();
            let mut mock = MockClaudeCaller::new();
            mock.expect_call().never();

            let runner = make_runner(dir.path(), mock);

            assert!(!runner.is_running());

            let _guard = runner.lock.lock().await;
            assert!(runner.is_running());
        }

        #[tokio::test]
        async fn last_execution_updated_after_tick() {
            let dir = TempDir::new().unwrap();
            std::fs::write(dir.path().join("HEARTBEAT.md"), "Check services").unwrap();

            let mut mock = MockClaudeCaller::new();
            mock.expect_call()
                .returning(|_| claude_success("OK"));

            let runner = make_runner(dir.path(), mock);

            assert!(runner.last_execution().is_none());
            runner.execute_tick().await;
            assert!(runner.last_execution().is_some());
        }

        #[tokio::test]
        async fn stop_cancels_loop() {
            tokio::time::pause();

            let dir = TempDir::new().unwrap();
            // No heartbeat file — ticks will be no-ops
            let mut mock = MockClaudeCaller::new();
            mock.expect_call().never();

            let runner = make_runner(dir.path(), mock);
            runner.start();

            // Advance time less than one interval
            tokio::time::advance(std::time::Duration::from_secs(60)).await;

            runner.stop();

            // Advance past what would be the next tick — loop should be stopped
            tokio::time::advance(std::time::Duration::from_secs(600)).await;
            tokio::task::yield_now().await;
        }

        #[tokio::test]
        async fn tick_uses_ephemeral_session() {
            let dir = TempDir::new().unwrap();
            std::fs::write(dir.path().join("HEARTBEAT.md"), "Check services").unwrap();

            let session_ids = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let si = session_ids.clone();

            let mut mock = MockClaudeCaller::new();
            mock.expect_call().times(2).returning(move |params| {
                si.lock().unwrap().push(params.session_id.clone());
                claude_success("OK")
            });

            let runner = make_runner(dir.path(), mock);
            runner.execute_tick().await;
            runner.execute_tick().await;

            let ids = session_ids.lock().unwrap();
            assert_eq!(ids.len(), 2);
            assert_ne!(ids[0], ids[1], "each tick should get a unique session ID");
            // Both should be valid UUIDs
            assert!(Uuid::parse_str(&ids[0]).is_ok());
            assert!(Uuid::parse_str(&ids[1]).is_ok());
        }

        #[tokio::test]
        async fn cli_failure_logged_not_propagated() {
            let dir = TempDir::new().unwrap();
            std::fs::write(dir.path().join("HEARTBEAT.md"), "Check services").unwrap();

            let mut hb_config = make_heartbeat_config();
            hb_config.log_file = Some("heartbeat.log".to_string());

            let mut mock = MockClaudeCaller::new();
            mock.expect_call().returning(|_| claude_error());

            let runner = make_runner_with_config(dir.path(), hb_config, mock);
            runner.execute_tick().await;

            // Should not panic; log file should have FAIL entry
            let log_content =
                std::fs::read_to_string(dir.path().join("heartbeat.log")).unwrap();
            assert!(log_content.contains("FAIL"));
            assert!(log_content.contains("cli error"));
        }
    }
}
