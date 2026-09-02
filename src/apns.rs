use a2::{
    Client, ClientConfig, DefaultNotificationBuilder, Endpoint, NotificationBuilder,
    NotificationOptions, Priority, PushType,
};
use serde::Deserialize;

/// Message posted to an `apns_<token>` channel. Either raw TXT (treated as
/// LED text, delivered as a silent background push), or JSON:
/// `{"leds": "...", "title": "...", "text": "...", "alert": "...",
///   "pattern": "...", "data": {...}}`
///
/// Semantics (matching the original PixiePulse push server): the push is a
/// visible alert only when `title`, `text`, or `alert` is set; otherwise it
/// is a background push (`content-available: 1`) carrying the custom keys.
#[derive(Deserialize, Default)]
pub struct ApnsMessage {
    #[serde(default)]
    pub leds: String,
    #[serde(default, rename = "LEDS.txt")]
    pub leds_txt: String,
    #[serde(default, rename = "LEDS.TXT")]
    pub leds_txt_upper: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub alert: String,
    #[serde(default)]
    pub pattern: String,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

impl ApnsMessage {
    pub fn parse(body: &str) -> Self {
        match serde_json::from_str::<ApnsMessage>(body) {
            Ok(msg) => msg,
            Err(_) => ApnsMessage {
                leds: body.to_string(),
                ..Default::default()
            },
        }
    }

    fn led_text(&self) -> &str {
        [&self.leds, &self.leds_txt, &self.leds_txt_upper]
            .into_iter()
            .find(|s| !s.is_empty())
            .map(String::as_str)
            .unwrap_or("")
    }
}

pub struct Apns {
    client: Client,
    topic: String,
}

fn build_payload<'a>(
    token: &'a str,
    msg: &'a ApnsMessage,
    topic: &'a str,
) -> Result<a2::request::payload::Payload<'a>, String> {
    let alert_body = if !msg.text.is_empty() {
        &msg.text
    } else {
        &msg.alert
    };
    let is_alert = !alert_body.is_empty() || !msg.title.is_empty();

    // Include the background-update flag on every notification. Alert
    // pushes still use the alert push type and high priority, but iOS can
    // also wake the app to process the custom LED data.
    let mut builder = DefaultNotificationBuilder::new().set_content_available();
    if is_alert {
        let title = if msg.title.is_empty() {
            "SidePulse"
        } else {
            &msg.title
        };
        builder = builder.set_title(title).set_sound("default");
        if !alert_body.is_empty() {
            builder = builder.set_body(alert_body);
        }
    }

    let options = NotificationOptions {
        apns_topic: Some(topic),
        apns_push_type: Some(if is_alert {
            PushType::Alert
        } else {
            PushType::Background
        }),
        apns_priority: Some(if is_alert {
            Priority::High
        } else {
            Priority::Normal
        }),
        ..Default::default()
    };

    let mut payload = builder.build(token, options);
    let led_text = msg.led_text();
    if !led_text.is_empty() {
        payload
            .add_custom_data("leds", &led_text)
            .map_err(|e| e.to_string())?;
    }
    if !msg.pattern.is_empty() {
        payload
            .add_custom_data("pattern", &msg.pattern)
            .map_err(|e| e.to_string())?;
    }
    if let Some(data) = &msg.data {
        payload
            .add_custom_data("data", data)
            .map_err(|e| e.to_string())?;
    }

    Ok(payload)
}

fn env_any(names: &[&str]) -> Option<String> {
    names.iter().find_map(|n| std::env::var(n).ok())
}

impl Apns {
    /// Returns Ok(None) when APNS env vars are absent (feature disabled),
    /// Err when they are present but the key can't be loaded.
    pub fn from_env() -> Result<Option<Self>, String> {
        let Some(key_path) = env_any(&["APNS_KEY_PATH", "APNS_AUTH_KEY"]) else {
            return Ok(None);
        };
        let key_id = std::env::var("APNS_KEY_ID").map_err(|_| "APNS_KEY_ID not set")?;
        let team_id = std::env::var("APNS_TEAM_ID").map_err(|_| "APNS_TEAM_ID not set")?;
        let topic = env_any(&["APNS_TOPIC", "APNS_BUNDLE_ID"]).ok_or("APNS_TOPIC not set")?;
        let sandbox = matches!(
            std::env::var("APNS_SANDBOX").as_deref(),
            Ok("1") | Ok("true")
        ) || std::env::var("APNS_ENV").as_deref() == Ok("sandbox");
        let endpoint = if sandbox {
            Endpoint::Sandbox
        } else {
            Endpoint::Production
        };

        let mut key = std::fs::File::open(&key_path)
            .map_err(|e| format!("cannot open APNS key {key_path}: {e}"))?;
        let client = Client::token(&mut key, &key_id, &team_id, ClientConfig::new(endpoint))
            .map_err(|e| format!("APNS client init failed: {e}"))?;

        Ok(Some(Self { client, topic }))
    }

    pub async fn send(&self, token: &str, msg: &ApnsMessage) -> Result<(), String> {
        let payload = build_payload(token, msg, &self.topic)?;
        let response = self.client.send(payload).await.map_err(|e| e.to_string())?;
        if response.code == 200 {
            Ok(())
        } else {
            Err(format!(
                "apns status {}: {:?}",
                response.code,
                response.error.map(|e| e.reason)
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_notification_also_requests_background_delivery() {
        let msg = ApnsMessage {
            leds: "LED TEXT".into(),
            title: "Title".into(),
            text: "Message".into(),
            ..Default::default()
        };

        let payload = build_payload("token", &msg, "io.sidepulse.ios").unwrap();
        let json = serde_json::to_value(payload).unwrap();

        assert_eq!(json["aps"]["content-available"], 1);
        assert_eq!(json["aps"]["alert"]["title"], "Title");
        assert_eq!(json["aps"]["alert"]["body"], "Message");
        assert_eq!(json["leds"], "LED TEXT");
    }

    #[test]
    fn silent_notification_keeps_background_delivery() {
        let msg = ApnsMessage {
            leds: "LED TEXT".into(),
            ..Default::default()
        };

        let payload = build_payload("token", &msg, "io.sidepulse.ios").unwrap();
        let json = serde_json::to_value(payload).unwrap();

        assert_eq!(json["aps"]["content-available"], 1);
        assert!(json["aps"].get("alert").is_none());
        assert_eq!(json["leds"], "LED TEXT");
    }
}
