use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::gateway::commands::HeartbeatTrigger;
use crate::gateway::scheduler::TaskScheduler;
use crate::gateway::session::SessionManager;

// ---- Types ----

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub uptime_seconds: u64,
    pub active_sessions: usize,
    pub inflight_calls: usize,
    pub last_heartbeat: Option<String>,
    pub last_cron_execution: Option<String>,
}

#[derive(Clone)]
pub struct HealthState {
    pub start_time: Instant,
    pub session_manager: Arc<Mutex<SessionManager>>,
    pub heartbeat_trigger: Option<Arc<dyn HeartbeatTrigger>>,
    pub scheduler: Option<Arc<TaskScheduler>>,
}

// ---- Helpers ----

fn instant_to_iso(inst: Instant, ref_instant: Instant, ref_utc: DateTime<Utc>) -> String {
    let elapsed = ref_instant.saturating_duration_since(inst);
    let wall_time = ref_utc - elapsed;
    wall_time.to_rfc3339()
}

// ---- Handler ----

async fn health_handler(State(state): State<HealthState>) -> Json<HealthResponse> {
    let uptime_seconds = state.start_time.elapsed().as_secs();

    let (active_sessions, inflight_calls) = {
        let sm = state.session_manager.lock().await;
        (sm.active_session_count(), sm.processing_sessions().len())
    };

    let ref_instant = Instant::now();
    let ref_utc = Utc::now();

    let last_heartbeat = state
        .heartbeat_trigger
        .as_ref()
        .and_then(|ht| ht.last_execution())
        .map(|inst| instant_to_iso(inst, ref_instant, ref_utc));

    let last_cron_execution = state
        .scheduler
        .as_ref()
        .and_then(|s| s.last_execution())
        .map(|inst| instant_to_iso(inst, ref_instant, ref_utc));

    Json(HealthResponse {
        status: "ok".to_string(),
        uptime_seconds,
        active_sessions,
        inflight_calls,
        last_heartbeat,
        last_cron_execution,
    })
}

// ---- HealthServer ----

#[derive(Default)]
pub struct HealthServer {
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl HealthServer {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn start(&mut self, port: u16, state: HealthState) -> u16 {
        self.stop();

        let app = Router::new()
            .route("/health", get(health_handler))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
            .await
            .expect("failed to bind health server port");

        let actual_port = listener.local_addr().unwrap().port();

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();

        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .ok();
        });

        self.shutdown_tx = Some(tx);
        self.handle = Some(handle);

        actual_port
    }

    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

// ---- Tests ----

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::commands::MockHeartbeatTrigger;
    use axum::body::Body;
    use axum::http::Request;
    use tempfile::TempDir;
    use tower::ServiceExt;

    fn make_session_manager(dir: &std::path::Path) -> SessionManager {
        SessionManager::new(
            dir.join("sessions.json"),
            60,
            "claude-sonnet-4-6".to_string(),
        )
    }

    fn make_state(dir: &std::path::Path) -> HealthState {
        HealthState {
            start_time: Instant::now(),
            session_manager: Arc::new(Mutex::new(make_session_manager(dir))),
            heartbeat_trigger: None,
            scheduler: None,
        }
    }

    // ---- instant_to_iso tests ----

    #[test]
    fn instant_to_iso_produces_valid_rfc3339() {
        let now = Instant::now();
        let utc_now = Utc::now();

        // An instant from 60 seconds ago
        let past = now - std::time::Duration::from_secs(60);
        let result = instant_to_iso(past, now, utc_now);

        // Should be parseable as RFC 3339
        let parsed = DateTime::parse_from_rfc3339(&result);
        assert!(parsed.is_ok(), "failed to parse: {result}");

        // Should be roughly 60 seconds before ref_utc
        let parsed_utc: DateTime<Utc> = parsed.unwrap().into();
        let diff = (utc_now - parsed_utc).num_seconds();
        assert!(
            (58..=62).contains(&diff),
            "expected ~60s diff, got {diff}s"
        );
    }

    // ---- HealthResponse serialization tests ----

    #[test]
    fn health_response_serializes_correctly() {
        let resp = HealthResponse {
            status: "ok".to_string(),
            uptime_seconds: 3600,
            active_sessions: 2,
            inflight_calls: 1,
            last_heartbeat: Some("2026-01-01T00:00:00+00:00".to_string()),
            last_cron_execution: None,
        };

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["uptime_seconds"], 3600);
        assert_eq!(json["active_sessions"], 2);
        assert_eq!(json["inflight_calls"], 1);
        assert_eq!(json["last_heartbeat"], "2026-01-01T00:00:00+00:00");
        assert!(json["last_cron_execution"].is_null());
    }

    // ---- Handler tests ----

    #[tokio::test]
    async fn health_handler_returns_ok_status() {
        let dir = TempDir::new().unwrap();
        let state = make_state(dir.path());

        let app = Router::new()
            .route("/health", get(health_handler))
            .with_state(state);

        let resp = app
            .oneshot(
                Request::get("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["status"], "ok");
        assert!(json["uptime_seconds"].as_u64().unwrap() < 5);
    }

    #[tokio::test]
    async fn health_handler_reports_session_metrics() {
        let dir = TempDir::new().unwrap();
        let sm = {
            let mut sm = make_session_manager(dir.path());
            sm.get_or_create("telegram", "123");
            sm.get_or_create("discord", "456");
            sm
        };

        let state = HealthState {
            start_time: Instant::now(),
            session_manager: Arc::new(Mutex::new(sm)),
            heartbeat_trigger: None,
            scheduler: None,
        };

        let app = Router::new()
            .route("/health", get(health_handler))
            .with_state(state);

        let resp = app
            .oneshot(
                Request::get("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["active_sessions"], 2);
        assert_eq!(json["inflight_calls"], 0);
    }

    #[tokio::test]
    async fn health_handler_includes_heartbeat_timestamp() {
        let dir = TempDir::new().unwrap();

        let mut mock = MockHeartbeatTrigger::new();
        mock.expect_last_execution()
            .returning(|| Some(Instant::now()));

        let state = HealthState {
            start_time: Instant::now(),
            session_manager: Arc::new(Mutex::new(make_session_manager(dir.path()))),
            heartbeat_trigger: Some(Arc::new(mock)),
            scheduler: None,
        };

        let app = Router::new()
            .route("/health", get(health_handler))
            .with_state(state);

        let resp = app
            .oneshot(
                Request::get("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        let ts = json["last_heartbeat"].as_str().unwrap();
        assert!(DateTime::parse_from_rfc3339(ts).is_ok(), "bad timestamp: {ts}");
    }

    #[tokio::test]
    async fn health_handler_null_when_no_heartbeat() {
        let dir = TempDir::new().unwrap();
        let state = make_state(dir.path());

        let app = Router::new()
            .route("/health", get(health_handler))
            .with_state(state);

        let resp = app
            .oneshot(
                Request::get("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert!(json["last_heartbeat"].is_null());
    }

    #[tokio::test]
    async fn health_handler_null_when_no_scheduler() {
        let dir = TempDir::new().unwrap();
        let state = make_state(dir.path());

        let app = Router::new()
            .route("/health", get(health_handler))
            .with_state(state);

        let resp = app
            .oneshot(
                Request::get("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert!(json["last_cron_execution"].is_null());
    }

    // ---- HealthServer tests ----

    #[tokio::test]
    async fn server_starts_and_responds() {
        let dir = TempDir::new().unwrap();
        let state = make_state(dir.path());

        let mut server = HealthServer::new();
        let port = server.start(0, state).await;

        let resp = reqwest::get(format!("http://127.0.0.1:{port}/health"))
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);

        let json: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(json["status"], "ok");

        server.stop();
    }

    #[tokio::test]
    async fn server_stops_gracefully() {
        let dir = TempDir::new().unwrap();
        let state = make_state(dir.path());

        let mut server = HealthServer::new();
        server.start(0, state).await;

        let handle = server.handle.as_ref().unwrap();
        assert!(!handle.is_finished());

        server.stop();

        // Give the server a moment to shut down
        let handle = server.handle.take().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("server did not shut down in time")
            .expect("server task panicked");
    }

    #[tokio::test]
    async fn server_uses_configured_port() {
        let dir = TempDir::new().unwrap();
        let state = make_state(dir.path());

        // Bind to port 0 first to find an available port
        let tmp_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let available_port = tmp_listener.local_addr().unwrap().port();
        drop(tmp_listener);

        let mut server = HealthServer::new();
        let actual_port = server.start(available_port, state).await;

        assert_eq!(actual_port, available_port);

        let resp = reqwest::get(format!("http://127.0.0.1:{available_port}/health"))
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);

        server.stop();
    }
}
