//! Notification provider adapters (docs/PRD.md section 15): Postmark and
//! generic SMTP for email, Twilio for SMS, Web Push. Provider credentials
//! arrive decrypted (calendar-auth::Crypto) and are never logged.

use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Provider {
    Postmark,
    Smtp,
    Twilio,
    WebPush,
}

impl Provider {
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "postmark" => Some(Self::Postmark),
            "smtp" => Some(Self::Smtp),
            "twilio" => Some(Self::Twilio),
            "webpush" => Some(Self::WebPush),
            _ => None,
        }
    }

    pub fn as_db_str(&self) -> &'static str {
        match self {
            Self::Postmark => "postmark",
            Self::Smtp => "smtp",
            Self::Twilio => "twilio",
            Self::WebPush => "webpush",
        }
    }
}

#[derive(Debug, Error)]
pub enum NotifyError {
    #[error("provider misconfigured: {0}")]
    Config(String),
    #[error("provider send failed: {0}")]
    Send(String),
}

/// One email provider (Postmark HTTP API or a generic SMTP relay).
#[derive(Debug, Clone)]
pub struct EmailProvider {
    pub provider: Provider,
    /// Postmark: server token; SMTP: host:port.
    pub server: String,
    pub from: String,
    /// Postmark auth token (SMTP relays may also take credentials).
    pub token: Option<String>,
    /// Postmark MessageStream; unused by SMTP. Defaults to "outbound".
    pub message_stream: String,
}

impl EmailProvider {
    pub fn kind(&self) -> Provider {
        self.provider
    }
}

impl EmailProvider {
    pub fn from_config(kind: Provider, config: &Value) -> Result<Self, NotifyError> {
        let get = |k: &str| config.get(k).and_then(Value::as_str).map(str::to_string);
        Ok(Self {
            provider: kind,
            server: get("server").ok_or_else(|| NotifyError::Config("server missing".into()))?,
            from: get("from").ok_or_else(|| NotifyError::Config("from missing".into()))?,
            token: get("token"),
            message_stream: get("message_stream").unwrap_or_else(|| "outbound".to_string()),
        })
    }

    /// Sends one email; Postmark via its JSON API, SMTP via lettre.
    pub async fn send(&self, to: &str, subject: &str, text: &str) -> Result<(), NotifyError> {
        match self.provider {
            Provider::Postmark => self.send_postmark(to, subject, text).await,
            Provider::Smtp => self.send_smtp(to, subject, text).await,
            Provider::Twilio | Provider::WebPush => {
                Err(NotifyError::Config("wrong provider kind for email".into()))
            }
        }
    }

    async fn send_postmark(&self, to: &str, subject: &str, text: &str) -> Result<(), NotifyError> {
        let token = self
            .token
            .as_deref()
            .ok_or_else(|| NotifyError::Config("postmark token missing".into()))?;
        let client = reqwest::Client::new();
        let payload = serde_json::json!({
            "From": self.from,
            "To": to,
            "Subject": subject,
            "TextBody": text,
            "MessageStream": self.message_stream,
        });
        let response = client
            .post("https://api.postmarkapp.com/email")
            .header("X-Postmark-Server-Token", token)
            .header("Accept", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| NotifyError::Send(e.to_string()))?;
        if !response.status().is_success() {
            return Err(NotifyError::Send(format!(
                "postmark status {}",
                response.status()
            )));
        }
        Ok(())
    }

    async fn send_smtp(&self, to: &str, subject: &str, text: &str) -> Result<(), NotifyError> {
        let email = Message::builder()
            .from(
                self.from
                    .parse()
                    .map_err(|e| NotifyError::Config(format!("bad from: {e}")))?,
            )
            .to(to
                .parse()
                .map_err(|e| NotifyError::Config(format!("bad to: {e}")))?)
            .subject(subject)
            .body(text.to_string())
            .map_err(|e| NotifyError::Send(e.to_string()))?;
        // ponytail: plain SMTP relay (no STARTTLS negotiation) — Postmark is
        // the recommended path; add TLS when a deployment needs it.
        let relay = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&self.server).build();
        relay
            .send(email)
            .await
            .map_err(|e| NotifyError::Send(e.to_string()))?;
        Ok(())
    }
}

/// Twilio SMS adapter.
#[derive(Debug, Clone)]
pub struct SmsProvider {
    pub account_sid: String,
    pub auth_token: String,
    pub from: String,
}

impl SmsProvider {
    pub fn from_config(config: &Value) -> Result<Self, NotifyError> {
        let get = |k: &str| config.get(k).and_then(Value::as_str).map(str::to_string);
        Ok(Self {
            account_sid: get("account_sid")
                .ok_or_else(|| NotifyError::Config("account_sid missing".into()))?,
            auth_token: get("auth_token")
                .ok_or_else(|| NotifyError::Config("auth_token missing".into()))?,
            from: get("from").ok_or_else(|| NotifyError::Config("from missing".into()))?,
        })
    }

    pub async fn send(&self, to: &str, text: &str) -> Result<(), NotifyError> {
        let url = format!(
            "https://api.twilio.com/2010-04-01/Accounts/{}/Messages.json",
            self.account_sid
        );
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| NotifyError::Send(e.to_string()))?;
        let response = client
            .post(url)
            .basic_auth(&self.account_sid, Some(&self.auth_token))
            .form(&[("To", to), ("From", &self.from), ("Body", text)])
            .send()
            .await
            .map_err(|e| NotifyError::Send(e.to_string()))?;
        if !response.status().is_success() {
            return Err(NotifyError::Send(format!(
                "twilio status {}",
                response.status()
            )));
        }
        Ok(())
    }
}

/// Generate a VAPID key pair (base64url raw private key + 65-byte
/// uncompressed public key), used when a webpush provider is first saved.
pub fn generate_vapid_keys() -> Result<(String, String), NotifyError> {
    use base64::Engine;
    use p256::ecdsa::SigningKey;
    let signing = SigningKey::random(&mut rand_core_06::OsRng);
    let private = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signing.to_bytes());
    let public = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(signing.verifying_key().to_encoded_point(false).as_bytes());
    Ok((private, public))
}

/// Web Push adapter: payload encryption + VAPID signing via the web-push
/// crate, delivery through the workspace reqwest client (the crate's own
/// http types are an older http version, so headers are copied by name).
#[derive(Debug, Clone)]
pub struct WebPushProvider {
    /// base64url raw private key.
    pub vapid_private: String,
    pub subject: String,
}

impl WebPushProvider {
    pub fn from_config(config: &Value) -> Result<Self, NotifyError> {
        let get = |k: &str| config.get(k).and_then(Value::as_str).map(str::to_string);
        Ok(Self {
            vapid_private: get("vapid_private")
                .ok_or_else(|| NotifyError::Config("vapid_private missing".into()))?,
            subject: get("subject").ok_or_else(|| NotifyError::Config("subject missing".into()))?,
        })
    }

    /// Sends one encrypted push. `payload` is JSON {title, body, url}.
    /// Ok(true) reports an expired subscription (410/404) so callers can
    /// drop the row.
    pub async fn send(
        &self,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
        payload: &[u8],
    ) -> Result<bool, NotifyError> {
        use web_push::{SubscriptionInfo, VapidSignatureBuilder, WebPushMessageBuilder};
        let subscription = SubscriptionInfo::new(endpoint, p256dh, auth);
        let mut signature_builder = VapidSignatureBuilder::from_base64_no_sub(
            &self.vapid_private,
            web_push::URL_SAFE_NO_PAD,
        )
        .map_err(|e| NotifyError::Config(format!("vapid key: {e}")))?
        .add_sub_info(&subscription);
        signature_builder.add_claim("sub", self.subject.clone());
        let signature = signature_builder
            .build()
            .map_err(|e| NotifyError::Config(format!("vapid signature: {e}")))?;
        let mut builder = WebPushMessageBuilder::new(&subscription);
        builder.set_payload(web_push::ContentEncoding::Aes128Gcm, payload);
        builder.set_vapid_signature(signature);
        let message = builder
            .build()
            .map_err(|e| NotifyError::Send(format!("push message: {e}")))?;
        let mut request = reqwest::Client::new()
            .post(message.endpoint.to_string())
            .header("TTL", message.ttl.to_string());
        if let Some(urgency) = &message.urgency {
            request = request.header("Urgency", urgency.to_string());
        }
        if let Some(payload) = &message.payload {
            request = request.header("Content-Encoding", payload.content_encoding.to_str());
            for (name, value) in &payload.crypto_headers {
                request = request.header(*name, value);
            }
            let response = request
                .body(payload.content.clone())
                .send()
                .await
                .map_err(|e| NotifyError::Send(e.to_string()))?;
            let status = response.status();
            if status.is_success() {
                Ok(false)
            } else if status.as_u16() == 410 || status.as_u16() == 404 {
                Ok(true)
            } else {
                Err(NotifyError::Send(format!("push status {}", status)))
            }
        } else {
            Err(NotifyError::Send("empty push payload".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_stream_defaults_to_outbound() {
        let config = serde_json::json!({"server": "postmark", "from": "a@b.c", "token": "t"});
        assert_eq!(
            EmailProvider::from_config(Provider::Postmark, &config)
                .unwrap()
                .message_stream,
            "outbound"
        );
        let config = serde_json::json!({
            "server": "postmark", "from": "a@b.c", "token": "t", "message_stream": "broadcasts"
        });
        assert_eq!(
            EmailProvider::from_config(Provider::Postmark, &config)
                .unwrap()
                .message_stream,
            "broadcasts"
        );
    }
}
