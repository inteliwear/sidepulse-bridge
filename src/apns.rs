use a2::{
    Client, ClientConfig, CollapseId, DefaultNotificationBuilder, Endpoint, NotificationBuilder,
    NotificationOptions, Priority, PushType,
};
use serde::Deserialize;

// APNs scopes this identifier to the destination app/device. All SidePulse
// updates replace the previous notification without any client-side option.
const COLLAPSE_ID: &str = "sidepulse-led-status";

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
    // Set by the bridge, never accepted from the request body.
    #[serde(skip)]
    pub push_id: Option<String>,
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
    production: Client,
    sandbox: Client,
    topic: String,
}

/// Bridge-only routing and sender metadata are never sent as part of Apple's
/// device token. Split after removing dev_ so keys may themselves contain '_'.
pub struct PushToken<'a> {
    pub endpoint: Endpoint,
    pub device_token: &'a str,
    pub shared_key: Option<&'a str>,
    pub routing_token: &'a str,
}

impl<'a> PushToken<'a> {
    pub fn parse(token: &'a str) -> Result<Self, &'static str> {
        let (endpoint, raw) = match token.strip_prefix("dev_") {
            Some(raw) => (Endpoint::Sandbox, raw),
            None => (Endpoint::Production, token),
        };
        let (device_token, shared_key) = match raw.split_once('_') {
            Some((device, key)) => {
                if key.is_empty()
                    || key.len() > 128
                    || !key
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                {
                    return Err("INVALID SHARED KEY");
                }
                (device, Some(key))
            }
            None => (raw, None),
        };
        if device_token.is_empty() {
            return Err("EMPTY APNS TOKEN");
        }
        let routing_len = token.len() - shared_key.map_or(0, |key| key.len() + 1);
        Ok(Self {
            endpoint,
            device_token,
            shared_key,
            routing_token: &token[..routing_len],
        })
    }

    pub fn channel_id(&self) -> String {
        format!("apns_{}", self.routing_token)
    }
}

fn build_payload<'a>(
    token: &PushToken<'a>,
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
        apns_collapse_id: Some(CollapseId::new(COLLAPSE_ID).map_err(|e| e.to_string())?),
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

    let mut payload = builder.build(token.device_token, options);
    if let Some(shared_key) = token.shared_key {
        payload
            .add_custom_data("shared_key", &shared_key)
            .map_err(|e| e.to_string())?;
    }
    if let Some(push_id) = &msg.push_id {
        payload
            .add_custom_data("sidepulse_push_id", push_id)
            .map_err(|e| e.to_string())?;
    }
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
        if std::env::var_os("APNS_SANDBOX").is_some() || std::env::var_os("APNS_ENV").is_some() {
            tracing::warn!("APNS_SANDBOX/APNS_ENV are ignored; dev_ tokens use sandbox, unprefixed tokens use production");
        }

        let key = std::fs::read(&key_path)
            .map_err(|e| format!("cannot read APNS key {key_path}: {e}"))?;
        let make_client = |endpoint| {
            Client::token(
                key.as_slice(),
                &key_id,
                &team_id,
                ClientConfig::new(endpoint),
            )
            .map_err(|e| format!("APNS client init failed: {e}"))
        };
        let production = make_client(Endpoint::Production)?;
        let sandbox = make_client(Endpoint::Sandbox)?;

        Ok(Some(Self {
            production,
            sandbox,
            topic,
        }))
    }

    pub async fn send(&self, token: &PushToken<'_>, msg: &ApnsMessage) -> Result<(), String> {
        let client = match token.endpoint {
            Endpoint::Production => &self.production,
            Endpoint::Sandbox => &self.sandbox,
        };
        let payload = build_payload(token, msg, &self.topic)?;
        let response = client.send(payload).await.map_err(|e| e.to_string())?;
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
    fn all_push_formats_use_the_same_default_collapse_identifier() {
        for body in [
            "plain LED text",
            r#"{"leds":"SILENT"}"#,
            r#"{"leds":"ALERT","title":"Title","text":"Message"}"#,
        ] {
            let msg = ApnsMessage::parse(body);
            for bridge_token in ["token", "dev_token", "token_sender", "dev_token_sender"] {
                let token = PushToken::parse(bridge_token).unwrap();
                let payload = build_payload(&token, &msg, "io.sidepulse.ios").unwrap();
                assert_eq!(
                    payload.options.apns_collapse_id.as_ref().unwrap().value,
                    "sidepulse-led-status"
                );
                // Collapse is an APNs header, not an API or payload field.
                let json = serde_json::to_value(payload).unwrap();
                assert!(json.get("apns-collapse-id").is_none());
            }
        }
    }

    #[test]
    fn development_prefix_selects_sandbox_and_is_removed_from_apns_payload() {
        let raw_token = "ab".repeat(32);
        let prefixed_token = format!("dev_{raw_token}");
        let token = PushToken::parse(&prefixed_token).unwrap();
        assert!(matches!(token.endpoint, Endpoint::Sandbox));
        let msg = ApnsMessage::parse("HELLO");
        let payload = build_payload(&token, &msg, "io.sidepulse.ios").unwrap();
        assert_eq!(payload.device_token, raw_token);
    }

    #[test]
    fn unprefixed_token_selects_production_and_is_preserved() {
        let raw_token = "ab".repeat(32);
        let token = PushToken::parse(&raw_token).unwrap();
        assert!(matches!(token.endpoint, Endpoint::Production));
        let msg = ApnsMessage::parse("HELLO");
        let payload = build_payload(&token, &msg, "io.sidepulse.ios").unwrap();
        assert_eq!(payload.device_token, raw_token);
    }

    #[test]
    fn shared_keys_reach_the_app_without_changing_device_or_environment() {
        let raw_token = "ab".repeat(32);
        for prefix in ["", "dev_"] {
            let bridge_token = format!("{prefix}{raw_token}_sender_key-123");
            let token = PushToken::parse(&bridge_token).unwrap();
            assert_eq!(token.routing_token, format!("{prefix}{raw_token}"));
            assert_eq!(token.shared_key, Some("sender_key-123"));
            assert_eq!(
                matches!(token.endpoint, Endpoint::Sandbox),
                prefix == "dev_"
            );
            for body in [
                "plain text",
                r#"{"leds":"JSON","shared_key":"spoofed"}"#,
                r#"{"title":"Alert","text":"Body","data":{"sender":"extra"}}"#,
            ] {
                let msg = ApnsMessage::parse(body);
                let payload = build_payload(&token, &msg, "io.sidepulse.ios").unwrap();
                assert_eq!(payload.device_token, raw_token);
                let json = serde_json::to_value(payload).unwrap();
                assert_eq!(json["shared_key"], "sender_key-123");
            }
        }
    }

    #[test]
    fn body_cannot_supply_a_shared_key_for_a_legacy_token() {
        let token = PushToken::parse("token").unwrap();
        let msg = ApnsMessage::parse(r#"{"leds":"HELLO","shared_key":"spoofed"}"#);
        let payload = build_payload(&token, &msg, "io.sidepulse.ios").unwrap();
        assert!(serde_json::to_value(payload)
            .unwrap()
            .get("shared_key")
            .is_none());
    }

    #[test]
    fn message_identifier_is_owned_by_the_bridge() {
        let token = PushToken::parse("token_sender").unwrap();
        let mut msg = ApnsMessage::parse(r#"{"leds":"HELLO","sidepulse_push_id":"spoofed"}"#);
        assert!(msg.push_id.is_none());
        msg.push_id = Some("bridge-message".into());
        let payload = build_payload(&token, &msg, "io.sidepulse.ios").unwrap();
        assert_eq!(
            serde_json::to_value(payload).unwrap()["sidepulse_push_id"],
            "bridge-message"
        );
    }

    #[test]
    fn malformed_shared_keys_and_empty_device_tokens_are_rejected() {
        for token in [
            "",
            "dev_",
            "_key",
            "dev__key",
            "token_",
            "dev_token_",
            "token_bad/key",
            "token_bad.key",
            "token_é",
        ] {
            assert!(PushToken::parse(token).is_err(), "accepted {token:?}");
        }
        assert!(PushToken::parse(&format!("token_{}", "a".repeat(129))).is_err());
        assert!(PushToken::parse(&format!("token_{}", "a".repeat(128))).is_ok());
    }

    #[test]
    fn visible_notification_also_requests_background_delivery() {
        let msg = ApnsMessage {
            leds: "LED TEXT".into(),
            title: "Title".into(),
            text: "Message".into(),
            ..Default::default()
        };

        let token = PushToken::parse("token").unwrap();
        let payload = build_payload(&token, &msg, "io.sidepulse.ios").unwrap();
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

        let token = PushToken::parse("token").unwrap();
        let payload = build_payload(&token, &msg, "io.sidepulse.ios").unwrap();
        let json = serde_json::to_value(payload).unwrap();

        assert_eq!(json["aps"]["content-available"], 1);
        assert!(json["aps"].get("alert").is_none());
        assert_eq!(json["leds"], "LED TEXT");
    }
}
