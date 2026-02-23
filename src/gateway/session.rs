use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing;
use uuid::Uuid;

pub struct Session {
    pub session_id: Uuid,
    pub initiated: bool,
    pub platform: String,
    pub model: String,
    pub processing: bool,
    pub last_activity: Instant,
    message_tx: mpsc::UnboundedSender<String>,
    message_rx: Option<mpsc::UnboundedReceiver<String>>,
    inactivity_handle: Option<JoinHandle<()>>,
}

impl Session {
    pub fn new(platform: String, model: String) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            session_id: Uuid::new_v4(),
            initiated: false,
            platform,
            model,
            processing: false,
            last_activity: Instant::now(),
            message_tx: tx,
            message_rx: Some(rx),
            inactivity_handle: None,
        }
    }

    pub fn cli_flags(&self) -> (&'static str, String) {
        if self.initiated {
            ("--resume", self.session_id.to_string())
        } else {
            ("--session-id", self.session_id.to_string())
        }
    }

    pub fn mark_initiated(&mut self) {
        self.initiated = true;
    }

    pub fn reset(&mut self) {
        self.session_id = Uuid::new_v4();
        self.initiated = false;
        self.cancel_inactivity_timer();
    }

    pub fn cancel_inactivity_timer(&mut self) {
        if let Some(handle) = self.inactivity_handle.take() {
            handle.abort();
        }
    }

    pub fn set_inactivity_handle(&mut self, handle: JoinHandle<()>) {
        self.inactivity_handle = Some(handle);
    }

    pub fn enqueue(&mut self, message: String) {
        self.last_activity = Instant::now();
        if self.message_tx.send(message).is_err() {
            tracing::warn!(session_id = %self.session_id, "enqueue failed: channel closed");
        }
    }

    pub fn take_rx(&mut self) -> Option<mpsc::UnboundedReceiver<String>> {
        self.message_rx.take()
    }

    pub async fn drain_queue(rx: &mut mpsc::UnboundedReceiver<String>) -> Option<String> {
        let first = rx.recv().await?;
        let mut messages = vec![first];
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }
        if messages.len() == 1 {
            Some(messages.pop().unwrap())
        } else {
            Some(format!(
                "Batched messages from user:\n{}",
                messages.join("\n")
            ))
        }
    }
}

pub struct SessionManager {
    sessions: HashMap<String, Session>,
    sessions_path: PathBuf,
    timeout_minutes: u64,
    default_model: String,
    timeout_tx: mpsc::UnboundedSender<String>,
    timeout_rx: Option<mpsc::UnboundedReceiver<String>>,
}

impl SessionManager {
    pub fn new(sessions_path: PathBuf, timeout_minutes: u64, default_model: String) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            sessions: HashMap::new(),
            sessions_path,
            timeout_minutes,
            default_model,
            timeout_tx: tx,
            timeout_rx: Some(rx),
        }
    }

    pub fn conversation_key(platform: &str, chat_id: &str) -> String {
        format!("{platform}-{chat_id}")
    }

    pub fn get_or_create(&mut self, platform: &str, chat_id: &str) -> &mut Session {
        let key = Self::conversation_key(platform, chat_id);
        let model = self.default_model.clone();
        self.sessions
            .entry(key)
            .or_insert_with(|| Session::new(platform.to_string(), model))
    }

    pub fn get(&self, key: &str) -> Option<&Session> {
        self.sessions.get(key)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Session> {
        self.sessions.get_mut(key)
    }

    pub fn reset_session(&mut self, key: &str) {
        if let Some(session) = self.sessions.get_mut(key) {
            session.reset();
        }
    }

    pub fn active_session_count(&self) -> usize {
        self.sessions.len()
    }

    pub fn save(&self) -> Result<(), io::Error> {
        let serializable = self.to_serializable();
        let data = serde_json::to_string_pretty(&serializable)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp_path = self.sessions_path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &data)?;
        std::fs::rename(&tmp_path, &self.sessions_path)?;
        Ok(())
    }

    pub fn load(&mut self) -> Result<(), io::Error> {
        let data = std::fs::read_to_string(&self.sessions_path)?;
        let saved: HashMap<String, SavedSession> =
            serde_json::from_str(&data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        for (key, entry) in saved {
            let session = Session::new(entry.platform, entry.model);
            self.sessions.insert(key, session);
        }
        Ok(())
    }

    fn to_serializable(&self) -> HashMap<String, SavedSession> {
        self.sessions
            .iter()
            .map(|(key, session)| {
                (
                    key.clone(),
                    SavedSession {
                        uuid: session.session_id.to_string(),
                        initiated: session.initiated,
                        platform: session.platform.clone(),
                        model: session.model.clone(),
                    },
                )
            })
            .collect()
    }

    pub fn start_inactivity_timer(&mut self, key: String) {
        let Some(session) = self.sessions.get_mut(&key) else {
            return;
        };
        session.cancel_inactivity_timer();

        let timeout = Duration::from_secs(self.timeout_minutes * 60);
        let tx = self.timeout_tx.clone();
        let timer_key = key.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            let _ = tx.send(timer_key);
        });

        // Safe: we confirmed the key exists above and never remove sessions
        self.sessions
            .get_mut(&key)
            .unwrap()
            .set_inactivity_handle(handle);
    }

    pub fn take_timeout_rx(&mut self) -> Option<mpsc::UnboundedReceiver<String>> {
        self.timeout_rx.take()
    }
}

#[derive(Serialize, Deserialize)]
struct SavedSession {
    uuid: String,
    initiated: bool,
    platform: String,
    model: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_session() -> Session {
        Session::new("telegram".to_string(), "claude-sonnet-4-6".to_string())
    }

    fn make_manager(dir: &std::path::Path) -> SessionManager {
        SessionManager::new(
            dir.join("sessions.json"),
            60,
            "claude-sonnet-4-6".to_string(),
        )
    }

    mod test_session {
        use super::*;

        #[test]
        fn new_generates_uuid_and_sets_defaults() {
            let session = make_session();
            assert!(!session.initiated);
            assert_eq!(session.platform, "telegram");
            assert_eq!(session.model, "claude-sonnet-4-6");
            assert!(!session.processing);
        }

        #[test]
        fn cli_flags_session_id_when_not_initiated() {
            let session = make_session();
            let (flag, value) = session.cli_flags();
            assert_eq!(flag, "--session-id");
            assert_eq!(value, session.session_id.to_string());
        }

        #[test]
        fn cli_flags_resume_when_initiated() {
            let mut session = make_session();
            session.mark_initiated();
            let (flag, value) = session.cli_flags();
            assert_eq!(flag, "--resume");
            assert_eq!(value, session.session_id.to_string());
        }

        #[test]
        fn reset_generates_new_uuid() {
            let mut session = make_session();
            let old_uuid = session.session_id;
            session.reset();
            assert_ne!(session.session_id, old_uuid);
        }

        #[test]
        fn reset_clears_initiated() {
            let mut session = make_session();
            session.mark_initiated();
            assert!(session.initiated);
            session.reset();
            assert!(!session.initiated);
        }
    }

    mod test_message_queue {
        use super::*;

        #[tokio::test]
        async fn enqueue_sends_to_channel() {
            let mut session = make_session();
            let mut rx = session.take_rx().unwrap();
            session.enqueue("hello".to_string());
            let msg = rx.recv().await.unwrap();
            assert_eq!(msg, "hello");
        }

        #[tokio::test]
        async fn single_message_returns_plain_text() {
            let mut session = make_session();
            let mut rx = session.take_rx().unwrap();
            session.enqueue("hello".to_string());
            let result = Session::drain_queue(&mut rx).await.unwrap();
            assert_eq!(result, "hello");
        }

        #[tokio::test]
        async fn multiple_messages_batched_with_prefix() {
            let mut session = make_session();
            let mut rx = session.take_rx().unwrap();
            session.enqueue("first".to_string());
            session.enqueue("second".to_string());
            session.enqueue("third".to_string());
            let result = Session::drain_queue(&mut rx).await.unwrap();
            assert_eq!(
                result,
                "Batched messages from user:\nfirst\nsecond\nthird"
            );
        }
    }

    mod test_session_manager {
        use super::*;

        #[test]
        fn conversation_key_format() {
            let key = SessionManager::conversation_key("telegram", "12345");
            assert_eq!(key, "telegram-12345");
        }

        #[test]
        fn get_or_create_returns_same_for_same_key() {
            let dir = TempDir::new().unwrap();
            let mut mgr = make_manager(dir.path());
            let uuid1 = mgr.get_or_create("telegram", "123").session_id;
            let uuid2 = mgr.get_or_create("telegram", "123").session_id;
            assert_eq!(uuid1, uuid2);
        }

        #[test]
        fn get_or_create_returns_different_for_different_keys() {
            let dir = TempDir::new().unwrap();
            let mut mgr = make_manager(dir.path());
            let uuid1 = mgr.get_or_create("telegram", "123").session_id;
            let uuid2 = mgr.get_or_create("telegram", "456").session_id;
            assert_ne!(uuid1, uuid2);
        }

        #[test]
        fn get_or_create_uses_default_model() {
            let dir = TempDir::new().unwrap();
            let mut mgr = make_manager(dir.path());
            let session = mgr.get_or_create("telegram", "123");
            assert_eq!(session.model, "claude-sonnet-4-6");
        }

        #[test]
        fn reset_session_generates_new_uuid() {
            let dir = TempDir::new().unwrap();
            let mut mgr = make_manager(dir.path());
            let old_uuid = mgr.get_or_create("telegram", "123").session_id;
            let key = SessionManager::conversation_key("telegram", "123");
            mgr.reset_session(&key);
            let session = mgr.get(&key).unwrap();
            assert_ne!(session.session_id, old_uuid);
            assert!(!session.initiated);
        }

        #[test]
        fn active_session_count() {
            let dir = TempDir::new().unwrap();
            let mut mgr = make_manager(dir.path());
            assert_eq!(mgr.active_session_count(), 0);
            mgr.get_or_create("telegram", "123");
            assert_eq!(mgr.active_session_count(), 1);
            mgr.get_or_create("discord", "456");
            assert_eq!(mgr.active_session_count(), 2);
        }
    }

    mod test_persistence {
        use super::*;

        #[test]
        fn save_load_round_trip_preserves_platform_and_model() {
            let dir = TempDir::new().unwrap();
            let mut mgr = make_manager(dir.path());
            mgr.get_or_create("telegram", "123");
            mgr.get_or_create("discord", "456");
            mgr.save().unwrap();

            let mut mgr2 = make_manager(dir.path());
            mgr2.load().unwrap();

            let key1 = SessionManager::conversation_key("telegram", "123");
            let key2 = SessionManager::conversation_key("discord", "456");
            let s1 = mgr2.get(&key1).unwrap();
            let s2 = mgr2.get(&key2).unwrap();
            assert_eq!(s1.platform, "telegram");
            assert_eq!(s1.model, "claude-sonnet-4-6");
            assert_eq!(s2.platform, "discord");
            assert_eq!(s2.model, "claude-sonnet-4-6");
        }

        #[test]
        fn load_regenerates_uuids() {
            let dir = TempDir::new().unwrap();
            let mut mgr = make_manager(dir.path());
            let original_uuid = mgr.get_or_create("telegram", "123").session_id;
            mgr.save().unwrap();

            let key = SessionManager::conversation_key("telegram", "123");
            let mut mgr2 = make_manager(dir.path());
            mgr2.load().unwrap();
            let loaded_uuid = mgr2.get(&key).unwrap().session_id;
            assert_ne!(loaded_uuid, original_uuid);
        }

        #[test]
        fn load_sets_initiated_false() {
            let dir = TempDir::new().unwrap();
            let mut mgr = make_manager(dir.path());
            mgr.get_or_create("telegram", "123").mark_initiated();
            mgr.save().unwrap();

            let key = SessionManager::conversation_key("telegram", "123");
            let mut mgr2 = make_manager(dir.path());
            mgr2.load().unwrap();
            assert!(!mgr2.get(&key).unwrap().initiated);
        }

        #[test]
        fn atomic_save_no_leftover_tmp() {
            let dir = TempDir::new().unwrap();
            let mut mgr = make_manager(dir.path());
            mgr.get_or_create("telegram", "123");
            mgr.save().unwrap();

            assert!(dir.path().join("sessions.json").exists());
            assert!(!dir.path().join("sessions.json.tmp").exists());
        }

        #[test]
        fn load_nonexistent_returns_error() {
            let dir = TempDir::new().unwrap();
            let mut mgr = make_manager(dir.path());
            let result = mgr.load();
            assert!(result.is_err());
        }
    }

    mod test_inactivity_timer {
        use super::*;

        #[tokio::test(start_paused = true)]
        async fn fires_after_timeout() {
            let dir = TempDir::new().unwrap();
            let mut mgr = SessionManager::new(
                dir.path().join("sessions.json"),
                1, // 1 minute timeout
                "claude-sonnet-4-6".to_string(),
            );
            mgr.get_or_create("telegram", "123");
            let key = SessionManager::conversation_key("telegram", "123");
            mgr.start_inactivity_timer(key.clone());
            let mut rx = mgr.take_timeout_rx().unwrap();

            tokio::time::advance(Duration::from_secs(61)).await;

            let received = rx.recv().await.unwrap();
            assert_eq!(received, key);
        }

        #[tokio::test(start_paused = true)]
        async fn does_not_fire_before_timeout() {
            let dir = TempDir::new().unwrap();
            let mut mgr = SessionManager::new(
                dir.path().join("sessions.json"),
                1,
                "claude-sonnet-4-6".to_string(),
            );
            mgr.get_or_create("telegram", "123");
            let key = SessionManager::conversation_key("telegram", "123");
            mgr.start_inactivity_timer(key);
            let mut rx = mgr.take_timeout_rx().unwrap();

            tokio::time::advance(Duration::from_secs(30)).await;
            tokio::task::yield_now().await;

            assert!(rx.try_recv().is_err());
        }

        #[tokio::test(start_paused = true)]
        async fn resets_on_new_activity() {
            let dir = TempDir::new().unwrap();
            let mut mgr = SessionManager::new(
                dir.path().join("sessions.json"),
                1,
                "claude-sonnet-4-6".to_string(),
            );
            mgr.get_or_create("telegram", "123");
            let key = SessionManager::conversation_key("telegram", "123");
            mgr.start_inactivity_timer(key.clone());
            let mut rx = mgr.take_timeout_rx().unwrap();

            // Advance 50 seconds — no fire
            tokio::time::advance(Duration::from_secs(50)).await;
            tokio::task::yield_now().await;
            assert!(rx.try_recv().is_err());

            // Restart timer (simulates new activity)
            mgr.start_inactivity_timer(key.clone());

            // Advance 50 more seconds — only 50s since restart, no fire
            tokio::time::advance(Duration::from_secs(50)).await;
            tokio::task::yield_now().await;
            assert!(rx.try_recv().is_err());

            // Advance 11 more — 61s since restart, fires
            tokio::time::advance(Duration::from_secs(11)).await;

            let received = rx.recv().await.unwrap();
            assert_eq!(received, key);
        }

        #[tokio::test(start_paused = true)]
        async fn reset_cancels_timer() {
            let dir = TempDir::new().unwrap();
            let mut mgr = SessionManager::new(
                dir.path().join("sessions.json"),
                1,
                "claude-sonnet-4-6".to_string(),
            );
            mgr.get_or_create("telegram", "123");
            let key = SessionManager::conversation_key("telegram", "123");
            mgr.start_inactivity_timer(key.clone());
            let mut rx = mgr.take_timeout_rx().unwrap();

            mgr.reset_session(&key);

            tokio::time::advance(Duration::from_secs(120)).await;
            tokio::task::yield_now().await;

            assert!(rx.try_recv().is_err());
        }
    }

    mod test_edge_cases {
        use super::*;

        #[test]
        fn reset_nonexistent_key_is_noop() {
            let dir = TempDir::new().unwrap();
            let mut mgr = make_manager(dir.path());
            mgr.reset_session("nonexistent-key"); // should not panic
            assert_eq!(mgr.active_session_count(), 0);
        }

        #[test]
        fn double_take_rx_returns_none() {
            let mut session = make_session();
            assert!(session.take_rx().is_some());
            assert!(session.take_rx().is_none());
        }

        #[tokio::test]
        async fn drain_queue_returns_none_on_closed_channel() {
            let mut session = make_session();
            let mut rx = session.take_rx().unwrap();
            drop(session); // drops the sender
            assert!(Session::drain_queue(&mut rx).await.is_none());
        }

        #[test]
        fn save_empty_sessions() {
            let dir = TempDir::new().unwrap();
            let mgr = make_manager(dir.path());
            mgr.save().unwrap();

            let mut mgr2 = make_manager(dir.path());
            mgr2.load().unwrap();
            assert_eq!(mgr2.active_session_count(), 0);
        }
    }
}
