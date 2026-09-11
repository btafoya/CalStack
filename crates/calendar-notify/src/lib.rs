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
            "MessageStream": "outbound",
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
