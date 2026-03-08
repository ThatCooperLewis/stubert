use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::adapters::markdown::to_imessage;
use crate::adapters::message_split::split_message;
use crate::adapters::sanitize::sanitize_filename;
use crate::adapters::{AdapterError, IncomingMessage, MessageHandler, PlatformAdapter};
use crate::config::BlueBubblesConfig;

// ---------------------------------------------------------------------------
// BlueBubblesApi trait — mockable abstraction over BB REST API
// ---------------------------------------------------------------------------

#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait BlueBubblesApi: Send + Sync {
    async fn send_text(
        &self,
        chat_guid: &str,
        text: &str,
        method: &str,
    ) -> Result<(), String>;
    async fn send_typing(&self, chat_guid: &str) -> Result<(), String>;
    async fn get_messages(
        &self,
        chat_guid: &str,
        after: i64,
        limit: u32,
    ) -> Result<Vec<BbMessage>, String>;
    async fn download_attachment(
        &self,
        attachment_guid: &str,
        destination: &Path,
    ) -> Result<(), String>;
    async fn ping(&self) -> Result<(), String>;
}

// ---------------------------------------------------------------------------
// BB API response types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct BbApiResponse<T> {
    pub status: i32,
    #[allow(dead_code)]
    pub message: String,
    pub data: T,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BbMessage {
    #[allow(dead_code)]
    pub guid: String,
    pub text: Option<String>,
    pub is_from_me: bool,
    pub date_created: i64,
    pub handle: Option<BbHandle>,
    pub attachments: Option<Vec<BbAttachment>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BbHandle {
    pub address: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BbAttachment {
    pub guid: String,
    pub mime_type: Option<String>,
    pub transfer_name: Option<String>,
    #[allow(dead_code)]
    pub total_bytes: Option<i64>,
}

// ---------------------------------------------------------------------------
// RealBlueBubblesApi — wraps reqwest::Client
// ---------------------------------------------------------------------------

pub struct RealBlueBubblesApi {
    client: reqwest::Client,
    server_url: String,
    password: String,
}

impl RealBlueBubblesApi {
    pub fn new(server_url: String, password: String) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("Failed to build HTTP client for BlueBubbles");

        Self {
            client,
            server_url: server_url.trim_end_matches('/').to_string(),
            password,
        }
    }
}

#[async_trait]
impl BlueBubblesApi for RealBlueBubblesApi {
    async fn send_text(
        &self,
        chat_guid: &str,
        text: &str,
        method: &str,
    ) -> Result<(), String> {
        let url = format!("{}/api/v1/message/text", self.server_url);
        let body = serde_json::json!({
            "chatGuid": chat_guid,
            "message": text,
            "method": method,
        });

        self.client
            .post(&url)
            .query(&[("password", &self.password)])
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("HTTP error: {e}"))?;

        Ok(())
    }

    async fn send_typing(&self, chat_guid: &str) -> Result<(), String> {
        let url = format!(
            "{}/api/v1/chat/{}/typing",
            self.server_url,
            urlencoding::encode(chat_guid)
        );

        self.client
            .post(&url)
            .query(&[("password", &self.password)])
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("HTTP error: {e}"))?;

        Ok(())
    }

    async fn get_messages(
        &self,
        chat_guid: &str,
        after: i64,
        limit: u32,
    ) -> Result<Vec<BbMessage>, String> {
        let url = format!(
            "{}/api/v1/chat/{}/message",
            self.server_url,
            urlencoding::encode(chat_guid)
        );

        let resp = self
            .client
            .get(&url)
            .query(&[
                ("password", self.password.as_str()),
                ("after", &after.to_string()),
                ("sort", "ASC"),
                ("limit", &limit.to_string()),
                ("with[]", "attachment"),
                ("with[]", "handle"),
            ])
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("HTTP error: {e}"))?;

        let api_resp: BbApiResponse<Vec<BbMessage>> = resp
            .json()
            .await
            .map_err(|e| format!("JSON parse failed: {e}"))?;

        Ok(api_resp.data)
    }

    async fn download_attachment(
        &self,
        attachment_guid: &str,
        destination: &Path,
    ) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;

        let url = format!(
            "{}/api/v1/attachment/{}/download",
            self.server_url,
            urlencoding::encode(attachment_guid)
        );

        let response = self
            .client
            .get(&url)
            .query(&[("password", &self.password)])
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("HTTP error: {e}"))?;

        let bytes = response
            .bytes()
            .await
            .map_err(|e| format!("Failed to read response: {e}"))?;

        let mut file = tokio::fs::File::create(destination)
            .await
            .map_err(|e| format!("Failed to create file: {e}"))?;

        file.write_all(&bytes)
            .await
            .map_err(|e| format!("Failed to write file: {e}"))?;

        Ok(())
    }

    async fn ping(&self) -> Result<(), String> {
        let url = format!("{}/api/v1/ping", self.server_url);

        self.client
            .get(&url)
            .query(&[("password", &self.password)])
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("HTTP error: {e}"))?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Attachment helpers
// ---------------------------------------------------------------------------

fn categorize_mime(mime: Option<&str>) -> AttachmentCategory {
    match mime {
        Some(m) if m.starts_with("image/") => AttachmentCategory::Image,
        Some(m) if m.starts_with("audio/") => AttachmentCategory::Audio,
        _ => AttachmentCategory::Other,
    }
}

#[derive(Debug, Clone, PartialEq)]
enum AttachmentCategory {
    Image,
    Audio,
    Other,
}

fn extension_from_mime(mime: Option<&str>) -> &str {
    match mime {
        Some("image/jpeg") => "jpg",
        Some("image/png") => "png",
        Some("image/gif") => "gif",
        Some("image/heic") => "heic",
        Some("image/heif") => "heif",
        Some("image/webp") => "webp",
        Some("audio/mpeg") => "mp3",
        Some("audio/mp4") => "m4a",
        Some("audio/aac") => "aac",
        Some("audio/x-caf") => "caf",
        _ => "bin",
    }
}

async fn download_bb_attachment(
    api: &dyn BlueBubblesApi,
    attachment: &BbAttachment,
    dir: &Path,
    existing_files: &[String],
) -> Option<(PathBuf, String, AttachmentCategory)> {
    let category = categorize_mime(attachment.mime_type.as_deref());

    let filename = match &attachment.transfer_name {
        Some(name) if !name.is_empty() => sanitize_filename(name, existing_files),
        _ => {
            // Use guid + extension from mime type
            let ext = extension_from_mime(attachment.mime_type.as_deref());
            let safe_guid = attachment.guid.replace(['/', '\\', ':'], "_");
            format!("{safe_guid}.{ext}")
        }
    };

    let dest = dir.join(&filename);

    match api.download_attachment(&attachment.guid, &dest).await {
        Ok(()) => Some((dest, filename, category)),
        Err(e) => {
            warn!("Failed to download BB attachment {}: {e}", attachment.guid);
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Core message processing
// ---------------------------------------------------------------------------

async fn process_message(
    msg: &BbMessage,
    chat_guid: &str,
    api: &dyn BlueBubblesApi,
    handler: &MessageHandler,
    files_dir: &Path,
    contacts: &std::collections::HashMap<String, String>,
) {
    // Skip our own messages
    if msg.is_from_me {
        debug!("Skipping isFromMe message in {chat_guid}");
        return;
    }

    // Extract sender info, resolving phone number to contact name if configured
    let (user_id, username) = match &msg.handle {
        Some(handle) => {
            let name = contacts.get(&handle.address).cloned();
            (handle.address.clone(), name.or_else(|| Some(handle.address.clone())))
        }
        None => ("unknown".to_string(), None),
    };

    // Download attachments
    let mut image_paths = Vec::new();
    let mut audio_paths = Vec::new();
    let mut file_paths = Vec::new();
    let mut file_names = Vec::new();

    if let Some(attachments) = &msg.attachments {
        // Sanitize chat_guid for directory name
        let safe_chat = chat_guid.replace(['/', '\\', ':', ';', '+'], "_");
        let dir = files_dir.join(format!("submitted-files/bluebubbles-{safe_chat}"));
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            error!("Failed to create BB attachment directory: {e}");
        } else {
            for attachment in attachments {
                if let Some((path, name, category)) =
                    download_bb_attachment(api, attachment, &dir, &file_names).await
                {
                    match category {
                        AttachmentCategory::Image => image_paths.push(path),
                        AttachmentCategory::Audio => audio_paths.push(path),
                        AttachmentCategory::Other => {
                            file_paths.push(path);
                            file_names.push(name);
                        }
                    }
                }
            }
        }
    }

    let incoming = IncomingMessage {
        platform: "bluebubbles".to_string(),
        user_id,
        username,
        chat_id: chat_guid.to_string(),
        text: msg.text.clone(),
        image_paths,
        audio_paths,
        file_paths,
        file_names,
    };

    handler(incoming).await;
}

// ---------------------------------------------------------------------------
// BlueBubblesAdapter
// ---------------------------------------------------------------------------

pub struct BlueBubblesAdapter {
    config: BlueBubblesConfig,
    api: Option<Arc<dyn BlueBubblesApi>>,
    handler: Option<MessageHandler>,
    files_dir: PathBuf,
    running: bool,
    poll_handle: Option<JoinHandle<()>>,
}

impl BlueBubblesAdapter {
    pub fn new(config: BlueBubblesConfig, files_dir: PathBuf) -> Self {
        Self {
            config,
            api: None,
            handler: None,
            files_dir,
            running: false,
            poll_handle: None,
        }
    }

    #[cfg(test)]
    fn with_api(
        config: BlueBubblesConfig,
        files_dir: PathBuf,
        api: Arc<dyn BlueBubblesApi>,
    ) -> Self {
        Self {
            config,
            api: Some(api),
            handler: None,
            files_dir,
            running: false,
            poll_handle: None,
        }
    }
}

#[async_trait]
impl PlatformAdapter for BlueBubblesAdapter {
    async fn start(&mut self) -> Result<(), AdapterError> {
        if self.running {
            return Err(AdapterError::AlreadyStarted);
        }

        // Create API if not injected (test vs production)
        if self.api.is_none() {
            self.api = Some(Arc::new(RealBlueBubblesApi::new(
                self.config.server_url.clone(),
                self.config.password.clone(),
            )));
        }

        let api = self.api.as_ref().unwrap().clone();

        // Validate connection
        api.ping()
            .await
            .map_err(|e| AdapterError::PlatformError(format!("BB ping failed: {e}")))?;

        info!("BlueBubbles server connection verified");

        // Spawn polling task
        let config = self.config.clone();
        let handler = self
            .handler
            .clone()
            .expect("message handler must be set before start");
        let files_dir = self.files_dir.clone();

        let handle = tokio::spawn(async move {
            poll_loop(config, api, handler, files_dir).await;
        });

        self.poll_handle = Some(handle);
        self.running = true;
        info!(
            "BlueBubbles adapter started, monitoring {} chat(s)",
            self.config.chat_guids.len()
        );
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), AdapterError> {
        if !self.running {
            return Err(AdapterError::NotStarted);
        }

        if let Some(handle) = self.poll_handle.take() {
            handle.abort();
        }

        self.running = false;
        info!("BlueBubbles adapter stopped");
        Ok(())
    }

    async fn send_message(&self, chat_id: &str, text: &str) -> Result<(), AdapterError> {
        let api = self.api.as_ref().ok_or(AdapterError::NotStarted)?;

        let converted = to_imessage(text);
        let chunks = split_message(&converted, 4000);

        for chunk in &chunks {
            api.send_text(chat_id, chunk, &self.config.send_method)
                .await
                .map_err(|e| AdapterError::SendFailed(e))?;
        }

        Ok(())
    }

    async fn send_typing(&self, chat_id: &str) -> Result<(), AdapterError> {
        let api = self.api.as_ref().ok_or(AdapterError::NotStarted)?;

        api.send_typing(chat_id)
            .await
            .map_err(|e| AdapterError::PlatformError(e))
    }

    fn set_message_handler(&mut self, handler: MessageHandler) {
        self.handler = Some(handler);
    }
}

// ---------------------------------------------------------------------------
// Polling loop
// ---------------------------------------------------------------------------

async fn poll_loop(
    config: BlueBubblesConfig,
    api: Arc<dyn BlueBubblesApi>,
    handler: MessageHandler,
    files_dir: PathBuf,
) {
    let poll_interval = std::time::Duration::from_secs(config.poll_interval_secs);

    // Initialize last_timestamp to current time in milliseconds
    let mut last_timestamp =
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

    let mut consecutive_errors: u32 = 0;

    info!("BlueBubbles polling loop started");

    loop {
        tokio::time::sleep(poll_interval).await;

        let mut max_timestamp = last_timestamp;
        let mut had_error = false;

        for chat_guid in &config.chat_guids {
            match api.get_messages(chat_guid, last_timestamp, 100).await {
                Ok(messages) => {
                    for msg in &messages {
                        if msg.date_created > max_timestamp {
                            max_timestamp = msg.date_created;
                        }
                        process_message(msg, chat_guid, api.as_ref(), &handler, &files_dir, &config.contacts)
                            .await;
                    }
                }
                Err(e) => {
                    had_error = true;
                    if consecutive_errors == 0 {
                        warn!("BlueBubbles polling error for {chat_guid}: {e}");
                    } else if consecutive_errors.is_power_of_two() {
                        warn!(
                            "BlueBubbles polling error ({} consecutive) for {chat_guid}: {e}",
                            consecutive_errors
                        );
                    }
                }
            }
        }

        if had_error {
            consecutive_errors += 1;
            let backoff = std::cmp::min(5 * consecutive_errors as u64, 60);
            tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
        } else {
            if consecutive_errors > 0 {
                info!(
                    "BlueBubbles polling recovered after {} consecutive error(s)",
                    consecutive_errors
                );
            }
            consecutive_errors = 0;
            last_timestamp = max_timestamp + 1;
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex as StdMutex;

    fn make_config() -> BlueBubblesConfig {
        BlueBubblesConfig {
            server_url: "http://localhost:1234".to_string(),
            password: "test-pass".to_string(),
            chat_guids: vec!["iMessage;+;chat123".to_string()],
            poll_interval_secs: 3,
            send_method: "private-api".to_string(),
            contacts: std::collections::HashMap::new(),
        }
    }

    fn make_message(text: Option<&str>, is_from_me: bool) -> BbMessage {
        BbMessage {
            guid: "msg-guid-1".to_string(),
            text: text.map(|s| s.to_string()),
            is_from_me,
            date_created: 1700000000000,
            handle: Some(BbHandle {
                address: "+15551234567".to_string(),
            }),
            attachments: None,
        }
    }

    fn noop_handler() -> MessageHandler {
        Arc::new(|_msg| Box::pin(async {}))
    }

    fn capturing_handler() -> (MessageHandler, Arc<StdMutex<Vec<IncomingMessage>>>) {
        let captured: Arc<StdMutex<Vec<IncomingMessage>>> = Arc::new(StdMutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let handler: MessageHandler = Arc::new(move |msg| {
            let cap = captured_clone.clone();
            Box::pin(async move {
                cap.lock().unwrap().push(msg);
            })
        });
        (handler, captured)
    }

    // -----------------------------------------------------------------------
    // process_message tests
    // -----------------------------------------------------------------------

    mod test_process_message {
        use super::*;
        use tempfile::TempDir;

        #[tokio::test]
        async fn text_message_produces_correct_incoming() {
            let mock_api = MockBlueBubblesApi::new();
            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let msg = make_message(Some("hello world"), false);
            process_message(&msg, "iMessage;+;chat123", &mock_api, &handler, tmp.path(), &std::collections::HashMap::new()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].platform, "bluebubbles");
            assert_eq!(msgs[0].user_id, "+15551234567");
            assert_eq!(msgs[0].username.as_deref(), Some("+15551234567"));
            assert_eq!(msgs[0].chat_id, "iMessage;+;chat123");
            assert_eq!(msgs[0].text.as_deref(), Some("hello world"));
            assert!(msgs[0].image_paths.is_empty());
            assert!(msgs[0].audio_paths.is_empty());
            assert!(msgs[0].file_paths.is_empty());
        }

        #[tokio::test]
        async fn skips_is_from_me() {
            let mock_api = MockBlueBubblesApi::new();
            let called = Arc::new(AtomicBool::new(false));
            let called_clone = called.clone();
            let handler: MessageHandler = Arc::new(move |_| {
                called_clone.store(true, Ordering::SeqCst);
                Box::pin(async {})
            });
            let tmp = TempDir::new().unwrap();

            let msg = make_message(Some("my own message"), true);
            process_message(&msg, "iMessage;+;chat123", &mock_api, &handler, tmp.path(), &std::collections::HashMap::new()).await;

            assert!(!called.load(Ordering::SeqCst));
        }

        #[tokio::test]
        async fn handles_missing_handle() {
            let mock_api = MockBlueBubblesApi::new();
            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let msg = BbMessage {
                guid: "msg-1".to_string(),
                text: Some("no handle".to_string()),
                is_from_me: false,
                date_created: 1700000000000,
                handle: None,
                attachments: None,
            };
            process_message(&msg, "iMessage;+;chat123", &mock_api, &handler, tmp.path(), &std::collections::HashMap::new()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs[0].user_id, "unknown");
            assert!(msgs[0].username.is_none());
        }

        #[tokio::test]
        async fn resolves_contact_name() {
            let mock_api = MockBlueBubblesApi::new();
            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let mut contacts = std::collections::HashMap::new();
            contacts.insert("+15551234567".to_string(), "Cooper".to_string());

            let msg = make_message(Some("hi"), false);
            process_message(&msg, "iMessage;+;chat123", &mock_api, &handler, tmp.path(), &contacts).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs[0].user_id, "+15551234567");
            assert_eq!(msgs[0].username.as_deref(), Some("Cooper"));
        }

        #[tokio::test]
        async fn falls_back_to_address_without_contact() {
            let mock_api = MockBlueBubblesApi::new();
            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let msg = make_message(Some("hi"), false);
            process_message(&msg, "iMessage;+;chat123", &mock_api, &handler, tmp.path(), &std::collections::HashMap::new()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs[0].username.as_deref(), Some("+15551234567"));
        }

        #[tokio::test]
        async fn handles_image_attachment() {
            let mut mock_api = MockBlueBubblesApi::new();
            mock_api
                .expect_download_attachment()
                .returning(|_, dest| {
                    std::fs::create_dir_all(dest.parent().unwrap()).ok();
                    std::fs::write(dest, b"fake image data").unwrap();
                    Ok(())
                });

            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let mut msg = make_message(None, false);
            msg.attachments = Some(vec![BbAttachment {
                guid: "att-1".to_string(),
                mime_type: Some("image/jpeg".to_string()),
                transfer_name: Some("photo.jpg".to_string()),
                total_bytes: Some(12345),
            }]);

            process_message(&msg, "iMessage;+;chat123", &mock_api, &handler, tmp.path(), &std::collections::HashMap::new()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs[0].image_paths.len(), 1);
            assert!(msgs[0].image_paths[0].to_str().unwrap().contains("photo.jpg"));
        }

        #[tokio::test]
        async fn handles_audio_attachment() {
            let mut mock_api = MockBlueBubblesApi::new();
            mock_api
                .expect_download_attachment()
                .returning(|_, dest| {
                    std::fs::create_dir_all(dest.parent().unwrap()).ok();
                    std::fs::write(dest, b"fake audio data").unwrap();
                    Ok(())
                });

            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let mut msg = make_message(None, false);
            msg.attachments = Some(vec![BbAttachment {
                guid: "att-2".to_string(),
                mime_type: Some("audio/mp4".to_string()),
                transfer_name: Some("voice.m4a".to_string()),
                total_bytes: Some(5000),
            }]);

            process_message(&msg, "iMessage;+;chat123", &mock_api, &handler, tmp.path(), &std::collections::HashMap::new()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs[0].audio_paths.len(), 1);
            assert!(msgs[0].audio_paths[0].to_str().unwrap().contains("voice.m4a"));
        }

        #[tokio::test]
        async fn handles_other_attachment() {
            let mut mock_api = MockBlueBubblesApi::new();
            mock_api
                .expect_download_attachment()
                .returning(|_, dest| {
                    std::fs::create_dir_all(dest.parent().unwrap()).ok();
                    std::fs::write(dest, b"fake pdf data").unwrap();
                    Ok(())
                });

            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let mut msg = make_message(None, false);
            msg.attachments = Some(vec![BbAttachment {
                guid: "att-3".to_string(),
                mime_type: Some("application/pdf".to_string()),
                transfer_name: Some("doc.pdf".to_string()),
                total_bytes: Some(8000),
            }]);

            process_message(&msg, "iMessage;+;chat123", &mock_api, &handler, tmp.path(), &std::collections::HashMap::new()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs[0].file_paths.len(), 1);
            assert_eq!(msgs[0].file_names, vec!["doc.pdf"]);
        }

        #[tokio::test]
        async fn attachment_download_failure_skipped() {
            let mut mock_api = MockBlueBubblesApi::new();
            mock_api
                .expect_download_attachment()
                .returning(|_, _| Err("download failed".to_string()));

            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let mut msg = make_message(Some("with failed attachment"), false);
            msg.attachments = Some(vec![BbAttachment {
                guid: "att-fail".to_string(),
                mime_type: Some("image/png".to_string()),
                transfer_name: Some("photo.png".to_string()),
                total_bytes: None,
            }]);

            process_message(&msg, "iMessage;+;chat123", &mock_api, &handler, tmp.path(), &std::collections::HashMap::new()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs.len(), 1);
            assert!(msgs[0].image_paths.is_empty());
            assert_eq!(msgs[0].text.as_deref(), Some("with failed attachment"));
        }

        #[tokio::test]
        async fn attachment_without_transfer_name_uses_guid() {
            let mut mock_api = MockBlueBubblesApi::new();
            mock_api
                .expect_download_attachment()
                .returning(|_, dest| {
                    std::fs::create_dir_all(dest.parent().unwrap()).ok();
                    std::fs::write(dest, b"data").unwrap();
                    Ok(())
                });

            let (handler, captured) = capturing_handler();
            let tmp = TempDir::new().unwrap();

            let mut msg = make_message(None, false);
            msg.attachments = Some(vec![BbAttachment {
                guid: "att/guid:special".to_string(),
                mime_type: Some("image/png".to_string()),
                transfer_name: None,
                total_bytes: None,
            }]);

            process_message(&msg, "iMessage;+;chat123", &mock_api, &handler, tmp.path(), &std::collections::HashMap::new()).await;

            let msgs = captured.lock().unwrap();
            assert_eq!(msgs[0].image_paths.len(), 1);
            let path_str = msgs[0].image_paths[0].to_str().unwrap();
            assert!(path_str.contains("att_guid_special.png"));
        }
    }

    // -----------------------------------------------------------------------
    // send_message tests
    // -----------------------------------------------------------------------

    mod test_send_message {
        use super::*;
        use tempfile::TempDir;

        #[tokio::test]
        async fn strips_markdown_and_sends() {
            let config = make_config();
            let mut mock_api = MockBlueBubblesApi::new();
            mock_api
                .expect_send_text()
                .withf(|chat_id, text, method| {
                    chat_id == "iMessage;+;chat123"
                        && text.contains("bold")
                        && !text.contains("**")
                        && method == "private-api"
                })
                .times(1)
                .returning(|_, _, _| Ok(()));

            let tmp = TempDir::new().unwrap();
            let mut adapter =
                BlueBubblesAdapter::with_api(config, tmp.path().to_path_buf(), Arc::new(mock_api));
            adapter.running = true;
            adapter.set_message_handler(noop_handler());

            let result = adapter
                .send_message("iMessage;+;chat123", "**bold**")
                .await;
            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn splits_long_messages() {
            let config = make_config();
            let mut mock_api = MockBlueBubblesApi::new();
            mock_api
                .expect_send_text()
                .returning(|_, _, _| Ok(()));

            let tmp = TempDir::new().unwrap();
            let mut adapter =
                BlueBubblesAdapter::with_api(config, tmp.path().to_path_buf(), Arc::new(mock_api));
            adapter.running = true;
            adapter.set_message_handler(noop_handler());

            let long_text = "a".repeat(5000);
            let result = adapter
                .send_message("iMessage;+;chat123", &long_text)
                .await;
            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn api_error_returns_send_failed() {
            let config = make_config();
            let mut mock_api = MockBlueBubblesApi::new();
            mock_api
                .expect_send_text()
                .returning(|_, _, _| Err("API timeout".to_string()));

            let tmp = TempDir::new().unwrap();
            let mut adapter =
                BlueBubblesAdapter::with_api(config, tmp.path().to_path_buf(), Arc::new(mock_api));
            adapter.running = true;
            adapter.set_message_handler(noop_handler());

            let result = adapter
                .send_message("iMessage;+;chat123", "hello")
                .await;
            assert!(matches!(result, Err(AdapterError::SendFailed(_))));
        }
    }

    // -----------------------------------------------------------------------
    // send_typing tests
    // -----------------------------------------------------------------------

    mod test_send_typing {
        use super::*;
        use tempfile::TempDir;

        #[tokio::test]
        async fn calls_typing_endpoint() {
            let config = make_config();
            let mut mock_api = MockBlueBubblesApi::new();
            mock_api
                .expect_send_typing()
                .withf(|chat_id| chat_id == "iMessage;+;chat123")
                .times(1)
                .returning(|_| Ok(()));

            let tmp = TempDir::new().unwrap();
            let mut adapter =
                BlueBubblesAdapter::with_api(config, tmp.path().to_path_buf(), Arc::new(mock_api));
            adapter.running = true;
            adapter.set_message_handler(noop_handler());

            let result = adapter.send_typing("iMessage;+;chat123").await;
            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn api_error_returns_platform_error() {
            let config = make_config();
            let mut mock_api = MockBlueBubblesApi::new();
            mock_api
                .expect_send_typing()
                .returning(|_| Err("connection error".to_string()));

            let tmp = TempDir::new().unwrap();
            let mut adapter =
                BlueBubblesAdapter::with_api(config, tmp.path().to_path_buf(), Arc::new(mock_api));
            adapter.running = true;
            adapter.set_message_handler(noop_handler());

            let result = adapter.send_typing("iMessage;+;chat123").await;
            assert!(matches!(result, Err(AdapterError::PlatformError(_))));
        }
    }

    // -----------------------------------------------------------------------
    // Lifecycle tests
    // -----------------------------------------------------------------------

    mod test_lifecycle {
        use super::*;
        use tempfile::TempDir;

        #[tokio::test]
        async fn start_when_already_started_returns_already_started() {
            let config = make_config();
            let tmp = TempDir::new().unwrap();
            let mock_api = MockBlueBubblesApi::new();
            let mut adapter =
                BlueBubblesAdapter::with_api(config, tmp.path().to_path_buf(), Arc::new(mock_api));
            adapter.running = true;
            adapter.set_message_handler(noop_handler());

            let result = adapter.start().await;
            assert!(matches!(result, Err(AdapterError::AlreadyStarted)));
        }

        #[tokio::test]
        async fn stop_when_not_started_returns_not_started() {
            let config = make_config();
            let tmp = TempDir::new().unwrap();
            let mut adapter = BlueBubblesAdapter::new(config, tmp.path().to_path_buf());

            let result = adapter.stop().await;
            assert!(matches!(result, Err(AdapterError::NotStarted)));
        }

        #[tokio::test]
        async fn send_message_before_start_returns_not_started() {
            let config = make_config();
            let tmp = TempDir::new().unwrap();
            let adapter = BlueBubblesAdapter::new(config, tmp.path().to_path_buf());

            let result = adapter
                .send_message("iMessage;+;chat123", "hello")
                .await;
            assert!(matches!(result, Err(AdapterError::NotStarted)));
        }

        #[tokio::test]
        async fn send_typing_before_start_returns_not_started() {
            let config = make_config();
            let tmp = TempDir::new().unwrap();
            let adapter = BlueBubblesAdapter::new(config, tmp.path().to_path_buf());

            let result = adapter.send_typing("iMessage;+;chat123").await;
            assert!(matches!(result, Err(AdapterError::NotStarted)));
        }

        #[tokio::test]
        async fn start_pings_and_spawns_poll_loop() {
            let config = make_config();
            let mut mock_api = MockBlueBubblesApi::new();
            mock_api.expect_ping().returning(|| Ok(()));
            mock_api
                .expect_get_messages()
                .returning(|_, _, _| Ok(vec![]));

            let tmp = TempDir::new().unwrap();
            let mut adapter =
                BlueBubblesAdapter::with_api(config, tmp.path().to_path_buf(), Arc::new(mock_api));
            adapter.set_message_handler(noop_handler());

            let result = adapter.start().await;
            assert!(result.is_ok());
            assert!(adapter.running);
            assert!(adapter.poll_handle.is_some());

            // Clean up
            adapter.stop().await.unwrap();
        }

        #[tokio::test]
        async fn start_fails_if_ping_fails() {
            let config = make_config();
            let mut mock_api = MockBlueBubblesApi::new();
            mock_api
                .expect_ping()
                .returning(|| Err("connection refused".to_string()));

            let tmp = TempDir::new().unwrap();
            let mut adapter =
                BlueBubblesAdapter::with_api(config, tmp.path().to_path_buf(), Arc::new(mock_api));
            adapter.set_message_handler(noop_handler());

            let result = adapter.start().await;
            assert!(matches!(result, Err(AdapterError::PlatformError(_))));
            assert!(!adapter.running);
        }
    }

    // -----------------------------------------------------------------------
    // Helper function tests
    // -----------------------------------------------------------------------

    mod test_helpers {
        use super::*;

        #[test]
        fn categorize_mime_image() {
            assert_eq!(
                categorize_mime(Some("image/jpeg")),
                AttachmentCategory::Image
            );
            assert_eq!(
                categorize_mime(Some("image/png")),
                AttachmentCategory::Image
            );
            assert_eq!(
                categorize_mime(Some("image/heic")),
                AttachmentCategory::Image
            );
        }

        #[test]
        fn categorize_mime_audio() {
            assert_eq!(
                categorize_mime(Some("audio/mp4")),
                AttachmentCategory::Audio
            );
            assert_eq!(
                categorize_mime(Some("audio/mpeg")),
                AttachmentCategory::Audio
            );
        }

        #[test]
        fn categorize_mime_other() {
            assert_eq!(
                categorize_mime(Some("application/pdf")),
                AttachmentCategory::Other
            );
            assert_eq!(categorize_mime(None), AttachmentCategory::Other);
        }

        #[test]
        fn extension_from_mime_known() {
            assert_eq!(extension_from_mime(Some("image/jpeg")), "jpg");
            assert_eq!(extension_from_mime(Some("image/png")), "png");
            assert_eq!(extension_from_mime(Some("audio/mpeg")), "mp3");
            assert_eq!(extension_from_mime(Some("audio/mp4")), "m4a");
        }

        #[test]
        fn extension_from_mime_unknown() {
            assert_eq!(extension_from_mime(Some("application/octet-stream")), "bin");
            assert_eq!(extension_from_mime(None), "bin");
        }
    }
}
