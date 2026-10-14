//! Fire-and-forget ntfy pushes. Sending runs on its own task with a short
//! timeout, so a slow or broken ntfy server can never delay or fail the
//! request that triggered it. Phase 2 uses it for throttle alerts; the
//! new-request message arrives in Phase 4.

use std::{sync::Arc, time::Duration};

use serde::Serialize;

use crate::config::NtfyConfig;

const SEND_TIMEOUT: Duration = Duration::from_secs(3);

/// One push. Sent as ntfy's JSON publish format, so titles with
/// non-ASCII characters need no header encoding.
#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub title: String,
    pub message: String,
    pub tags: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub click: Option<String>,
    /// ntfy priority, 1 (min) to 5 (max); 3 is the default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<u8>,
}

#[derive(Serialize)]
struct Publish<'a> {
    topic: &'a str,
    #[serde(flatten)]
    message: &'a Message,
}

#[derive(Clone)]
pub struct Notifier {
    client: reqwest::Client,
    config: Arc<NtfyConfig>,
}

impl Notifier {
    pub fn new(client: reqwest::Client, config: NtfyConfig) -> Self {
        Self {
            client,
            config: Arc::new(config),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.config.url.is_some()
    }

    /// Queues `message` for sending and returns immediately. Failures are
    /// logged and otherwise ignored. A no-op when ntfy is not configured.
    pub fn send(&self, message: Message) {
        if !self.is_enabled() {
            return;
        }
        let notifier = self.clone();
        tokio::spawn(async move {
            if let Err(error) = notifier.deliver(&message).await {
                // reqwest errors carry the URL, not the topic or token.
                tracing::warn!(error = %error.without_url(), "ntfy notification failed");
            }
        });
    }

    async fn deliver(&self, message: &Message) -> Result<(), reqwest::Error> {
        let Some(url) = &self.config.url else {
            return Ok(());
        };
        let body = Publish {
            topic: &self.config.topic,
            message,
        };
        let mut request = self
            .client
            .post(url.as_str())
            .timeout(SEND_TIMEOUT)
            .json(&body);
        if let Some(token) = &self.config.token {
            request = request.bearer_auth(token);
        }
        request.send().await?.error_for_status()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_body_flattens_message_and_skips_empty_fields() {
        let message = Message {
            title: "Throttle on".into(),
            message: "details".into(),
            tags: vec!["warning"],
            click: None,
            priority: Some(4),
        };
        let json = serde_json::to_value(Publish {
            topic: "t",
            message: &message,
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "topic": "t",
                "title": "Throttle on",
                "message": "details",
                "tags": ["warning"],
                "priority": 4,
            })
        );
    }

    #[test]
    fn disabled_without_url() {
        let notifier = Notifier::new(reqwest::Client::new(), NtfyConfig::default());
        assert!(!notifier.is_enabled());
    }
}
