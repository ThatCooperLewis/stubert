pub mod markdown;
pub mod message_split;
pub mod sanitize;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub platform: String,
    pub user_id: String,
    pub chat_id: String,
    pub text: Option<String>,
    pub image_paths: Vec<PathBuf>,
    pub audio_paths: Vec<PathBuf>,
    pub file_paths: Vec<PathBuf>,
    pub file_names: Vec<String>,
}

pub type MessageHandler = Arc<
    dyn Fn(IncomingMessage) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync,
>;

#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("adapter not started")]
    NotStarted,
    #[error("adapter already started")]
    AlreadyStarted,
    #[error("send failed: {0}")]
    SendFailed(String),
    #[error("platform error: {0}")]
    PlatformError(String),
}

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait PlatformAdapter: Send + Sync {
    async fn start(&mut self) -> Result<(), AdapterError>;
    async fn stop(&mut self) -> Result<(), AdapterError>;
    async fn send_message(&self, chat_id: &str, text: &str) -> Result<(), AdapterError>;
    async fn send_typing(&self, chat_id: &str) -> Result<(), AdapterError>;
    fn set_message_handler(&mut self, handler: MessageHandler);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incoming_message_can_be_constructed() {
        let msg = IncomingMessage {
            platform: "telegram".to_string(),
            user_id: "123".to_string(),
            chat_id: "456".to_string(),
            text: Some("hello".to_string()),
            image_paths: vec![],
            audio_paths: vec![],
            file_paths: vec![],
            file_names: vec![],
        };
        assert_eq!(msg.platform, "telegram");
        assert_eq!(msg.text.as_deref(), Some("hello"));
    }

    #[test]
    fn incoming_message_clone_is_independent() {
        let msg = IncomingMessage {
            platform: "discord".to_string(),
            user_id: "u1".to_string(),
            chat_id: "c1".to_string(),
            text: None,
            image_paths: vec![PathBuf::from("/tmp/img.jpg")],
            audio_paths: vec![],
            file_paths: vec![],
            file_names: vec![],
        };
        let cloned = msg.clone();
        assert_eq!(cloned.image_paths, vec![PathBuf::from("/tmp/img.jpg")]);
        assert!(cloned.text.is_none());
    }

    #[test]
    fn adapter_error_display() {
        assert_eq!(AdapterError::NotStarted.to_string(), "adapter not started");
        assert_eq!(
            AdapterError::AlreadyStarted.to_string(),
            "adapter already started"
        );
        assert_eq!(
            AdapterError::SendFailed("timeout".to_string()).to_string(),
            "send failed: timeout"
        );
        assert_eq!(
            AdapterError::PlatformError("api down".to_string()).to_string(),
            "platform error: api down"
        );
    }

    #[tokio::test]
    async fn mock_adapter_can_be_created() {
        let mut mock = MockPlatformAdapter::new();
        mock.expect_start().returning(|| Ok(()));
        mock.expect_send_message()
            .returning(|_chat_id, _text| Ok(()));
        mock.expect_send_typing().returning(|_chat_id| Ok(()));
        mock.expect_stop().returning(|| Ok(()));
        mock.expect_set_message_handler().returning(|_handler| ());

        assert!(mock.start().await.is_ok());
        assert!(mock.send_message("chat1", "hi").await.is_ok());
        assert!(mock.send_typing("chat1").await.is_ok());
        assert!(mock.stop().await.is_ok());
    }
}
