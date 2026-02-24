mod common;

use std::sync::Arc;

use stubert::gateway::core::Gateway;
use stubert::gateway::history::HistoryWriter;
use stubert::gateway::session::SessionManager;
use stubert::gateway::skills::SkillRegistry;
use tempfile::TempDir;

use common::{
    make_incoming, make_incoming_empty, make_test_config, wait_for_messages, TestAdapter,
    TestClaudeCaller,
};

fn make_gateway(
    dir: &std::path::Path,
    claude_caller: TestClaudeCaller,
) -> Gateway {
    let config = make_test_config(dir);
    let sm = SessionManager::new(
        dir.join("sessions.json"),
        60,
        "claude-sonnet-4-6".to_string(),
    );
    let hw = HistoryWriter::new(dir.join("history"));
    let sr = SkillRegistry::new(dir.join(".claude").join("skills"));

    Gateway::new(config, sm, hw, Arc::new(claude_caller), None, sr, None)
}

// ---- Message Flow Tests ----

mod test_message_flow {
    use super::*;

    #[tokio::test]
    async fn full_message_round_trip() {
        let dir = TempDir::new().unwrap();
        let caller = TestClaudeCaller::always_success("Test response!");
        let calls = caller.calls.clone();

        let mut gw = make_gateway(dir.path(), caller);
        let adapter = TestAdapter::new();
        let sent = adapter.sent_messages.clone();
        let handler_slot = adapter.handler_slot();
        gw.register_adapter("telegram", adapter).await;
        gw.start().await;

        let handler = handler_slot.lock().unwrap().clone().unwrap();
        handler(make_incoming("telegram", "123", "hello world")).await;

        wait_for_messages(&sent, 1).await;

        // Assert ClaudeCaller received the prompt
        {
            let captured = calls.lock().unwrap();
            assert_eq!(captured.len(), 1);
            assert!(captured[0].prompt.contains("hello world"));
        }

        // Assert response was sent back
        {
            let messages = sent.lock().unwrap();
            assert!(messages.iter().any(|(_, text)| text.contains("Test response!")));
        }

        // Assert history was written
        let history_dir = dir.path().join("history");
        let entries: Vec<_> = std::fs::read_dir(&history_dir)
            .unwrap()
            .flatten()
            .collect();
        assert!(!entries.is_empty());
        let content = std::fs::read_to_string(entries[0].path()).unwrap();
        assert!(content.contains("hello world"));
        assert!(content.contains("Test response!"));

        gw.shutdown().await;
    }

    #[tokio::test]
    async fn empty_message_skipped() {
        let dir = TempDir::new().unwrap();
        let caller = TestClaudeCaller::always_success("shouldn't happen");
        let calls = caller.calls.clone();

        let mut gw = make_gateway(dir.path(), caller);
        let adapter = TestAdapter::new();
        let sent = adapter.sent_messages.clone();
        let handler_slot = adapter.handler_slot();
        gw.register_adapter("telegram", adapter).await;
        gw.start().await;

        let handler = handler_slot.lock().unwrap().clone().unwrap();
        handler(make_incoming_empty("telegram", "123")).await;

        // Give it a moment — nothing should happen
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert!(calls.lock().unwrap().is_empty());
        assert!(sent.lock().unwrap().is_empty());

        gw.shutdown().await;
    }

    #[tokio::test]
    async fn unknown_platform_ignored() {
        let dir = TempDir::new().unwrap();
        let caller = TestClaudeCaller::always_success("shouldn't happen");
        let calls = caller.calls.clone();

        let mut gw = make_gateway(dir.path(), caller);
        let adapter = TestAdapter::new();
        let handler_slot = adapter.handler_slot();
        gw.register_adapter("telegram", adapter).await;
        gw.start().await;

        let handler = handler_slot.lock().unwrap().clone().unwrap();
        // Send from "slack" but only "telegram" is registered
        handler(make_incoming("slack", "123", "hello")).await;

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(calls.lock().unwrap().is_empty());

        gw.shutdown().await;
    }

    #[tokio::test]
    async fn typing_indicator_sent() {
        let dir = TempDir::new().unwrap();
        let caller = TestClaudeCaller::with_delay("response", std::time::Duration::from_millis(50));

        let mut gw = make_gateway(dir.path(), caller);
        let adapter = TestAdapter::new();
        let sent = adapter.sent_messages.clone();
        let typing = adapter.typing_calls.clone();
        let handler_slot = adapter.handler_slot();
        gw.register_adapter("telegram", adapter).await;
        gw.start().await;

        let handler = handler_slot.lock().unwrap().clone().unwrap();
        handler(make_incoming("telegram", "123", "hello")).await;

        wait_for_messages(&sent, 1).await;
        assert!(!typing.lock().unwrap().is_empty());

        gw.shutdown().await;
    }
}

// ---- Command Routing Tests ----

mod test_command_routing {
    use super::*;

    #[tokio::test]
    async fn help_command() {
        let dir = TempDir::new().unwrap();
        let caller = TestClaudeCaller::always_success("shouldn't happen");
        let calls = caller.calls.clone();

        let mut gw = make_gateway(dir.path(), caller);
        let adapter = TestAdapter::new();
        let sent = adapter.sent_messages.clone();
        let handler_slot = adapter.handler_slot();
        gw.register_adapter("telegram", adapter).await;
        gw.start().await;

        let handler = handler_slot.lock().unwrap().clone().unwrap();
        handler(make_incoming("telegram", "123", "/help")).await;

        wait_for_messages(&sent, 1).await;

        // Help should not call Claude
        assert!(calls.lock().unwrap().is_empty());

        // Response should list commands
        let help_text = {
            let messages = sent.lock().unwrap();
            messages[0].1.clone()
        };
        assert!(help_text.contains("/new"));
        assert!(help_text.contains("/status"));
        assert!(help_text.contains("/help"));

        gw.shutdown().await;
    }

    #[tokio::test]
    async fn status_command() {
        let dir = TempDir::new().unwrap();
        let caller = TestClaudeCaller::always_success("shouldn't happen");

        let mut gw = make_gateway(dir.path(), caller);
        let adapter = TestAdapter::new();
        let sent = adapter.sent_messages.clone();
        let handler_slot = adapter.handler_slot();
        gw.register_adapter("telegram", adapter).await;
        gw.start().await;

        let handler = handler_slot.lock().unwrap().clone().unwrap();
        handler(make_incoming("telegram", "123", "/status")).await;

        wait_for_messages(&sent, 1).await;

        let status = {
            let messages = sent.lock().unwrap();
            messages[0].1.clone()
        };
        assert!(status.contains("Uptime"));
        assert!(status.contains("Active sessions"));
        assert!(status.contains("Model"));

        gw.shutdown().await;
    }

    #[tokio::test]
    async fn models_list() {
        let dir = TempDir::new().unwrap();
        let caller = TestClaudeCaller::always_success("shouldn't happen");

        let mut gw = make_gateway(dir.path(), caller);
        let adapter = TestAdapter::new();
        let sent = adapter.sent_messages.clone();
        let handler_slot = adapter.handler_slot();
        gw.register_adapter("telegram", adapter).await;
        gw.start().await;

        let handler = handler_slot.lock().unwrap().clone().unwrap();
        handler(make_incoming("telegram", "123", "/models")).await;

        wait_for_messages(&sent, 1).await;

        let text = {
            let messages = sent.lock().unwrap();
            messages[0].1.to_lowercase()
        };
        assert!(text.contains("sonnet"));
        assert!(text.contains("opus"));
        assert!(text.contains("haiku"));

        gw.shutdown().await;
    }

    #[tokio::test]
    async fn models_switch() {
        let dir = TempDir::new().unwrap();
        let caller = TestClaudeCaller::always_success("shouldn't happen");

        let mut gw = make_gateway(dir.path(), caller);
        let adapter = TestAdapter::new();
        let sent = adapter.sent_messages.clone();
        let handler_slot = adapter.handler_slot();
        gw.register_adapter("telegram", adapter).await;
        gw.start().await;

        let handler = handler_slot.lock().unwrap().clone().unwrap();
        handler(make_incoming("telegram", "123", "/models opus")).await;

        wait_for_messages(&sent, 1).await;

        let text = {
            let messages = sent.lock().unwrap();
            messages[0].1.to_lowercase()
        };
        assert!(text.contains("opus"));

        gw.shutdown().await;
    }

    #[tokio::test]
    async fn new_session() {
        let dir = TempDir::new().unwrap();
        let caller = TestClaudeCaller::always_success("Fresh session greeting!");
        let calls = caller.calls.clone();

        let mut gw = make_gateway(dir.path(), caller);
        let adapter = TestAdapter::new();
        let sent = adapter.sent_messages.clone();
        let handler_slot = adapter.handler_slot();
        gw.register_adapter("telegram", adapter).await;
        gw.start().await;

        let handler = handler_slot.lock().unwrap().clone().unwrap();
        handler(make_incoming("telegram", "123", "/new")).await;

        wait_for_messages(&sent, 1).await;

        // /new should call Claude for a greeting
        assert!(!calls.lock().unwrap().is_empty());

        let has_greeting = {
            let messages = sent.lock().unwrap();
            messages.iter().any(|(_, text)| text.contains("Fresh session greeting!"))
        };
        assert!(has_greeting);

        gw.shutdown().await;
    }

    #[tokio::test]
    async fn history_search() {
        let dir = TempDir::new().unwrap();
        let caller = TestClaudeCaller::always_success("The answer is 42");
        let calls = caller.calls.clone();

        let mut gw = make_gateway(dir.path(), caller);
        let adapter = TestAdapter::new();
        let sent = adapter.sent_messages.clone();
        let handler_slot = adapter.handler_slot();
        gw.register_adapter("telegram", adapter).await;
        gw.start().await;

        let handler = handler_slot.lock().unwrap().clone().unwrap();

        // First send a regular message to generate history
        handler(make_incoming("telegram", "123", "unique_keyword_xyz")).await;
        wait_for_messages(&sent, 1).await;

        // Clear sent messages for cleaner assertion
        sent.lock().unwrap().clear();

        // Now search history
        handler(make_incoming("telegram", "123", "/history unique_keyword_xyz")).await;
        wait_for_messages(&sent, 1).await;

        // History search should NOT call Claude (the first message did though)
        assert_eq!(calls.lock().unwrap().len(), 1);

        let text = {
            let messages = sent.lock().unwrap();
            messages[0].1.clone()
        };
        assert!(text.contains("unique_keyword_xyz"));

        gw.shutdown().await;
    }
}

// ---- Session Persistence Tests ----

mod test_session_persistence {
    use super::*;

    #[tokio::test]
    async fn session_survives_restart() {
        let dir = TempDir::new().unwrap();

        // First gateway: create a session
        {
            let caller = TestClaudeCaller::always_success("hello");
            let mut gw = make_gateway(dir.path(), caller);
            let adapter = TestAdapter::new();
            let sent = adapter.sent_messages.clone();
            let handler_slot = adapter.handler_slot();
            gw.register_adapter("telegram", adapter).await;
            gw.start().await;

            let handler = handler_slot.lock().unwrap().clone().unwrap();
            handler(make_incoming("telegram", "123", "hi")).await;
            wait_for_messages(&sent, 1).await;

            gw.shutdown().await;
        }

        // Second gateway: session should be loaded
        {
            let caller = TestClaudeCaller::always_success("world");
            let mut gw = make_gateway(dir.path(), caller);
            let adapter = TestAdapter::new();
            gw.register_adapter("telegram", adapter).await;
            gw.start().await;

            assert!(gw.active_session_count().await > 0);

            gw.shutdown().await;
        }
    }

    #[tokio::test]
    async fn model_switch_persists() {
        let dir = TempDir::new().unwrap();

        // First gateway: switch model
        {
            let caller = TestClaudeCaller::always_success("ok");
            let mut gw = make_gateway(dir.path(), caller);
            let adapter = TestAdapter::new();
            let sent = adapter.sent_messages.clone();
            let handler_slot = adapter.handler_slot();
            gw.register_adapter("telegram", adapter).await;
            gw.start().await;

            let handler = handler_slot.lock().unwrap().clone().unwrap();
            handler(make_incoming("telegram", "123", "/models opus")).await;
            wait_for_messages(&sent, 1).await;

            gw.shutdown().await;
        }

        // Second gateway: check model is still opus
        {
            let caller = TestClaudeCaller::always_success("ok");
            let mut gw = make_gateway(dir.path(), caller);
            let adapter = TestAdapter::new();
            let sent = adapter.sent_messages.clone();
            let handler_slot = adapter.handler_slot();
            gw.register_adapter("telegram", adapter).await;
            gw.start().await;

            let handler = handler_slot.lock().unwrap().clone().unwrap();
            handler(make_incoming("telegram", "123", "/models")).await;
            wait_for_messages(&sent, 1).await;

            let text = {
                let messages = sent.lock().unwrap();
                messages[0].1.clone()
            };
            // The active model is marked with "* opus" prefix
            assert!(text.contains("* opus"), "expected '* opus' active marker, got: {text}");

            gw.shutdown().await;
        }
    }
}

// ---- Lifecycle Tests ----

mod test_lifecycle {
    use super::*;

    #[tokio::test]
    async fn start_stop() {
        let dir = TempDir::new().unwrap();
        let caller = TestClaudeCaller::always_success("ok");
        let mut gw = make_gateway(dir.path(), caller);

        let adapter = TestAdapter::new();
        gw.register_adapter("telegram", adapter).await;

        gw.start().await;
        assert!(gw.is_running());

        gw.shutdown().await;
        assert!(!gw.is_running());
    }

    #[tokio::test]
    async fn adapter_started_on_gateway_start() {
        let dir = TempDir::new().unwrap();
        let caller = TestClaudeCaller::always_success("ok");
        let mut gw = make_gateway(dir.path(), caller);

        let adapter = TestAdapter::new();
        let handler_slot = adapter.handler_slot();
        gw.register_adapter("telegram", adapter).await;

        gw.start().await;

        // Adapter's start() was called — verify by checking the handler was installed
        assert!(handler_slot.lock().unwrap().is_some());

        gw.shutdown().await;
    }
}
