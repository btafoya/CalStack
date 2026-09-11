//! Notification provider interfaces.
//! Implement Postmark, generic SMTP, Twilio and Web Push adapters here.

#[derive(Debug, Clone, Copy)]
pub enum Provider {
    Postmark,
    Smtp,
    Twilio,
    WebPush,
}
