//! Small trigger/condition/action engine.
//! Designed for later expansion without coupling rules to notification providers.

#[derive(Debug, Clone, Copy)]
pub enum Trigger {
    EventCreated,
    EventUpdated,
    EventDeleted,
    AttendeeInvited,
    RsvpChanged,
    AlarmDue,
}

#[derive(Debug, Clone, Copy)]
pub enum Action {
    Email,
    Sms,
    Webhook,
    Notification,
}
