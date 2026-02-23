use crate::config::LoggingConfig;
use rolling_file::{BasicRollingFileAppender, RollingConditionBasic};
use std::io::Write;
use std::sync::Once;
use thiserror::Error;
use tracing::Level;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

static INIT: Once = Once::new();

const TRANSIENT_KEYWORDS: &[&str] = &["Bad Gateway", "NetworkError", "TimedOut", "ServerError"];

#[derive(Debug, Error)]
pub enum LogError {
    #[error("failed to create log directory: {0}")]
    CreateDir(std::io::Error),
    #[error("failed to create log file: {0}")]
    CreateFile(std::io::Error),
}

pub fn setup_logging(config: &LoggingConfig) -> Result<(), LogError> {
    let mut result = Ok(());

    INIT.call_once(|| {
        match setup_logging_inner(config) {
            Ok(()) => {}
            Err(e) => result = Err(e),
        }
    });

    result
}

fn setup_logging_inner(config: &LoggingConfig) -> Result<(), LogError> {
    if let Some(parent) = std::path::Path::new(&config.log_file).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(LogError::CreateDir)?;
        }
    }

    let condition = RollingConditionBasic::new().max_size(config.log_max_bytes);
    let appender = BasicRollingFileAppender::new(
        &config.log_file,
        condition,
        config.log_backup_count as usize,
    )
    .map_err(LogError::CreateFile)?;

    let (non_blocking, _guard) = tracing_appender::non_blocking(appender);
    // Leak the guard so the writer stays alive for the process lifetime
    std::mem::forget(_guard);

    let env_filter =
        EnvFilter::try_new(&config.level).unwrap_or_else(|_| EnvFilter::new("info"));

    let console_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_filter(env_filter);

    // The transient filter is intentionally applied to file output only.
    // Console retains original severity levels for development/debugging visibility.
    let file_env_filter =
        EnvFilter::try_new(&config.level).unwrap_or_else(|_| EnvFilter::new("info"));
    let file_layer = TelegramTransientFilter::new(non_blocking).with_filter(file_env_filter);

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .init();

    Ok(())
}

/// A custom tracing Layer that writes events to a file writer, downgrading
/// ERROR-level events containing transient Telegram error keywords to WARN.
pub struct TelegramTransientFilter<W: for<'a> MakeWriter<'a> + 'static> {
    writer: W,
}

impl<W: for<'a> MakeWriter<'a> + 'static> TelegramTransientFilter<W> {
    pub fn new(writer: W) -> Self {
        Self { writer }
    }
}

impl<S, W> Layer<S> for TelegramTransientFilter<W>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    W: for<'a> MakeWriter<'a> + 'static,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let metadata = event.metadata();
        let original_level = *metadata.level();

        // Collect the message from the event
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let message = visitor.message;

        // Determine effective level
        let effective_level = if original_level == Level::ERROR && is_transient(&message) {
            Level::WARN
        } else {
            original_level
        };

        // Format and write
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
        let target = metadata.target();
        let line = format!("[{now}] [{effective_level}] {target}: {message}\n");

        let mut writer = self.writer.make_writer();
        let _ = writer.write_all(line.as_bytes());
    }
}

fn is_transient(message: &str) -> bool {
    TRANSIENT_KEYWORDS
        .iter()
        .any(|keyword| message.contains(keyword))
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;

    /// A writer that captures all output into a shared buffer.
    #[derive(Clone)]
    struct CaptureWriter {
        buf: Arc<Mutex<Vec<u8>>>,
    }

    impl CaptureWriter {
        fn new() -> Self {
            Self {
                buf: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn output(&self) -> String {
            String::from_utf8(self.buf.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.buf.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Run a closure with a scoped subscriber that includes our filter layer,
    /// returning the captured output.
    fn run_with_filter(f: impl FnOnce()) -> String {
        let capture = CaptureWriter::new();
        let filter_layer = TelegramTransientFilter::new(capture.clone());
        let subscriber = tracing_subscriber::registry().with(filter_layer);
        tracing::subscriber::with_default(subscriber, f);
        capture.output()
    }

    mod test_telegram_transient_filter {
        use super::*;

        #[test]
        fn downgrades_bad_gateway_to_warn() {
            let output = run_with_filter(|| {
                tracing::error!("Bad Gateway from Telegram API");
            });
            assert!(output.contains("[WARN]"), "output was: {output}");
            assert!(output.contains("Bad Gateway"));
        }

        #[test]
        fn downgrades_network_error_to_warn() {
            let output = run_with_filter(|| {
                tracing::error!("NetworkError: connection reset");
            });
            assert!(output.contains("[WARN]"), "output was: {output}");
        }

        #[test]
        fn downgrades_timed_out_to_warn() {
            let output = run_with_filter(|| {
                tracing::error!("request TimedOut");
            });
            assert!(output.contains("[WARN]"), "output was: {output}");
        }

        #[test]
        fn downgrades_server_error_to_warn() {
            let output = run_with_filter(|| {
                tracing::error!("ServerError: 502");
            });
            assert!(output.contains("[WARN]"), "output was: {output}");
        }

        #[test]
        fn passes_non_transient_errors_at_error() {
            let output = run_with_filter(|| {
                tracing::error!("Database failed");
            });
            assert!(output.contains("[ERROR]"), "output was: {output}");
        }

        #[test]
        fn does_not_affect_non_error_levels() {
            let output = run_with_filter(|| {
                tracing::warn!("Bad Gateway but only warn");
            });
            assert!(output.contains("[WARN]"), "output was: {output}");
            // Should NOT be upgraded to ERROR or anything else — stays WARN
            assert!(!output.contains("[ERROR]"), "output was: {output}");
        }
    }

    mod test_setup_logging {
        use super::*;
        use tempfile::TempDir;

        #[test]
        fn setup_is_idempotent() {
            // Note: since Once is static, this test can only meaningfully test
            // that the second call doesn't panic. In a real test run the first
            // call may have already happened from another test, so we just
            // verify no panic.
            let dir = TempDir::new().unwrap();
            let config = LoggingConfig {
                log_file: dir
                    .path()
                    .join("logs/test.log")
                    .to_str()
                    .unwrap()
                    .to_string(),
                log_max_bytes: 1_000_000,
                log_backup_count: 3,
                level: "INFO".to_string(),
            };
            // First call may or may not succeed depending on test ordering
            // (static Once), but second call should always return Ok.
            let _ = setup_logging(&config);
            let result = setup_logging(&config);
            assert!(result.is_ok());
        }
    }
}
