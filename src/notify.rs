//! Fire-and-forget ntfy pushes. Sending runs on its own task with a
//! timeout, so a slow or broken ntfy server can never delay or fail the
//! request that triggered it. A push that fails for a passing reason (a
//! timeout, a connection error, a 5xx or 429 from the server) is tried again
//! a little later, because the public ntfy.sh does have such moments. Used
//! for throttle alerts and for genuinely new requests (never duplicates).

use std::{sync::Arc, time::Duration};

use serde::Serialize;

use crate::config::NtfyConfig;

/// For each attempt. ntfy.sh from a small home server can take a few seconds.
const SEND_TIMEOUT: Duration = Duration::from_secs(8);

/// How long to wait before the second and the third attempt.
const RETRY_DELAYS: &[Duration] = &[Duration::from_secs(2), Duration::from_secs(10)];

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
    pub infuse_pro: bool,
}

impl Message {
    /// `Ny bestilling: Dune (2021)`, with type, requester and note in the
    /// body, and a click-through to the admin Pending tab.
    pub fn new_request(info: &NewRequestInfo<'_>, public_origin: &str) -> Self {
        let title = match info.year {
            Some(year) => format!("Ny bestilling: {} ({year})", info.title),
            None => format!("Ny bestilling: {}", info.title),
        };
        let kind = if info.is_movie { "Film" } else { "Serie" };
        let pro = if info.infuse_pro { "med" } else { "uten" };
        let mut lines = vec![format!(
            "{kind}, bestilt av {} ({pro} Infuse PRO)",
            info.requester
        )];
        if let Some(note) = info.note {
            lines.push(format!("Merknad: {note}"));
        }
        Message {
            title,
            message: lines.join("\n"),
            tags: vec![if info.is_movie { "movie_camera" } else { "tv" }],
            click: Some(format!("{public_origin}/admin#pending")),
            priority: None,
        }
    }
}

/// Is this failure worth another try? A timeout, a connection problem, a 5xx
/// or a 429 may pass; a 4xx (wrong token, bad topic) will not.
fn is_passing(error: &reqwest::Error) -> bool {
    match error.status() {
        Some(status) => status.is_server_error() || status.as_u16() == 429,
        None => !error.is_builder(),
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
    retry_delays: &'static [Duration],
}

impl Notifier {
    pub fn new(client: reqwest::Client, config: NtfyConfig) -> Self {
        Self {
            client,
            config: Arc::new(config),
            retry_delays: RETRY_DELAYS,
        }
    }

    /// Different waits between attempts, so tests need not sit through the
    /// real ones.
    #[cfg(test)]
    fn with_retry_delays(mut self, delays: &'static [Duration]) -> Self {
        self.retry_delays = delays;
        self
    }

    pub fn is_enabled(&self) -> bool {
        self.config.url.is_some()
    }

    /// Queues `message` for sending and returns immediately. A passing
    /// failure is retried after the configured waits; a final failure is
    /// logged and otherwise ignored. A no-op when ntfy is not configured.
    pub fn send(&self, message: Message) {
        if !self.is_enabled() {
            return;
        }
        let notifier = self.clone();
        tokio::spawn(async move {
            let mut attempt = 0;
            loop {
                let Err(error) = notifier.deliver(&message).await else {
                    return;
                };
                // reqwest errors carry the URL, not the topic or token.
                let error = error.without_url();
                match notifier.retry_delays.get(attempt) {
                    Some(&wait) if is_passing(&error) => {
                        tracing::info!(%error, attempt = attempt + 1, "ntfy notification failed; trying again");
                        tokio::time::sleep(wait).await;
                        attempt += 1;
                    }
                    _ => {
                        tracing::warn!(%error, attempts = attempt + 1, "ntfy notification failed");
                        return;
                    }
                }
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
            infuse_pro: true,
        };
        let m = Message::new_request(&info, "https://requests.example.com");
        assert_eq!(m.title, "Ny bestilling: Dune (2021)");
        assert_eq!(
            m.message,
            "Film, bestilt av Alice (med Infuse PRO)\nMerknad: the 2021 one"
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
            infuse_pro: false,
        };
        let m = Message::new_request(&series, "https://r.example");
        assert_eq!(m.title, "Ny bestilling: Severance");
        assert_eq!(m.message, "Serie, bestilt av Bob (uten Infuse PRO)");
        assert_eq!(m.tags, ["tv"]);
    }

    const FAST: &[Duration] = &[Duration::from_millis(10), Duration::from_millis(10)];

    fn notifier_for(server: &wiremock::MockServer) -> Notifier {
        let config = NtfyConfig {
            url: Some(server.uri()),
            topic: "t".into(),
            token: None,
        };
        Notifier::new(reqwest::Client::new(), config).with_retry_delays(FAST)
    }

    fn push() -> Message {
        Message {
            title: "Ny bestilling: Dune".into(),
            message: "Film, bestilt av Alice".into(),
            tags: vec!["movie_camera"],
            click: None,
            priority: None,
        }
    }

    /// How many pushes the server has received once it has gone quiet.
    async fn received(server: &wiremock::MockServer, at_least: usize) -> usize {
        for _ in 0..100 {
            let n = server.received_requests().await.unwrap_or_default().len();
            if n >= at_least {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        server.received_requests().await.unwrap_or_default().len()
    }

    #[tokio::test]
    async fn a_push_that_hits_a_502_is_tried_again_and_delivered() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(502))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        notifier_for(&server).send(push());
        assert_eq!(received(&server, 2).await, 2, "one failure, one retry");
    }

    #[tokio::test]
    async fn a_push_gives_up_after_three_attempts() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        notifier_for(&server).send(push());
        assert_eq!(received(&server, 3).await, 3);
    }

    #[tokio::test]
    async fn a_rejected_push_is_not_retried() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        notifier_for(&server).send(push());
        assert_eq!(received(&server, 2).await, 1, "a 403 will not pass");
    }

    #[test]
    fn disabled_without_url() {
        let notifier = Notifier::new(reqwest::Client::new(), NtfyConfig::default());
        assert!(!notifier.is_enabled());
    }
}
