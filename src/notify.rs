//! Fire-and-forget ntfy pushes. Sending runs on its own task with a short
//! timeout, so a slow or broken ntfy server can never delay or fail the
//! request that triggered it. Used for throttle alerts and for genuinely new
//! requests (never duplicates).

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

/// What the push for a genuinely new request says.
pub struct NewRequestInfo<'a> {
    pub title: &'a str,
    pub year: Option<i64>,
    pub is_movie: bool,
    pub requester: &'a str,
    pub note: Option<&'a str>,
    pub imdb_id: Option<&'a str>,
}

impl Message {
    /// `New request: Dune (2021)`, with type, requester, note and IMDb ID
    /// in the body, and a click-through to the admin Pending tab.
    pub fn new_request(info: &NewRequestInfo<'_>, public_origin: &str) -> Self {
        let title = match info.year {
            Some(year) => format!("New request: {} ({year})", info.title),
            None => format!("New request: {}", info.title),
        };
        let kind = if info.is_movie { "Movie" } else { "Series" };
        let mut lines = vec![format!("{kind}, requested by {}", info.requester)];
        if let Some(note) = info.note {
            lines.push(format!("Note: {note}"));
        }
        lines.push(format!("IMDb: {}", info.imdb_id.unwrap_or("unknown")));
        Message {
            title,
            message: lines.join("\n"),
            tags: vec![if info.is_movie { "movie_camera" } else { "tv" }],
            click: Some(format!("{public_origin}/admin#pending")),
            priority: None,
        }
    }
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
    fn new_request_message_matches_spec() {
        let info = NewRequestInfo {
            title: "Dune",
            year: Some(2021),
            is_movie: true,
            requester: "Alice",
            note: Some("the 2021 one"),
            imdb_id: Some("tt1160419"),
        };
        let m = Message::new_request(&info, "https://requests.example.com");
        assert_eq!(m.title, "New request: Dune (2021)");
        assert_eq!(
            m.message,
            "Movie, requested by Alice\nNote: the 2021 one\nIMDb: tt1160419"
        );
        assert_eq!(m.tags, ["movie_camera"]);
        assert_eq!(
            m.click.as_deref(),
            Some("https://requests.example.com/admin#pending")
        );

        let series = NewRequestInfo {
            title: "Severance",
            year: None,
            is_movie: false,
            requester: "Bob",
            note: None,
            imdb_id: None,
        };
        let m = Message::new_request(&series, "https://r.example");
        assert_eq!(m.title, "New request: Severance");
        assert_eq!(m.message, "Series, requested by Bob\nIMDb: unknown");
        assert_eq!(m.tags, ["tv"]);
    }

    #[test]
    fn disabled_without_url() {
        let notifier = Notifier::new(reqwest::Client::new(), NtfyConfig::default());
        assert!(!notifier.is_enabled());
    }
}
