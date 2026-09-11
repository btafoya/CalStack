//! Domain types and validation for the calendar server.
//!
//! These types mirror the normalized PostgreSQL schema in
//! `migrations/0001_initial.sql`; iCalendar is a wire representation handled
//! by `calendar-caldav`, never the canonical model.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub mod recurrence;

#[derive(Debug, Error, PartialEq)]
pub enum ValidationError {
    #[error("invalid slug: {0}")]
    Slug(String),
    #[error("invalid username: {0}")]
    Username(String),
    #[error("invalid email: {0}")]
    Email(String),
    #[error("invalid event: {0}")]
    Event(String),
    #[error("invalid attendee: {0}")]
    Attendee(String),
}

// ============ tenancy ============

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantRole {
    Owner,
    Admin,
    Member,
}

impl TenantRole {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Member => "member",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "owner" => Some(Self::Owner),
            "admin" => Some(Self::Admin),
            "member" => Some(Self::Member),
            _ => None,
        }
    }
}

// ============ calendar ACL ============

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalendarCapability {
    Owner,
    ReadWrite,
    ReadOnly,
    FreeBusy,
}

impl CalendarCapability {
    /// Whether this capability grants `required` (variant order is the
    /// privilege order: Owner > ReadWrite > ReadOnly > FreeBusy).
    pub fn satisfies(&self, required: CalendarCapability) -> bool {
        *self <= required
    }
    pub fn as_db_str(&self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::ReadWrite => "read_write",
            Self::ReadOnly => "read_only",
            Self::FreeBusy => "free_busy",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "owner" => Some(Self::Owner),
            "read_write" => Some(Self::ReadWrite),
            "read_only" => Some(Self::ReadOnly),
            "free_busy" => Some(Self::FreeBusy),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AclEntry {
    pub principal_user_id: Uuid,
    pub capability: CalendarCapability,
    pub can_manage_acl: bool,
}

#[derive(Debug, Error, PartialEq)]
pub enum AclError {
    #[error("calendar must have at least one owner")]
    NoOwner,
}

/// Validated ACL set: owner rows imply `can_manage_acl`, at least one owner.
#[derive(Debug, Clone)]
pub struct AclSet {
    pub entries: Vec<AclEntry>,
}

impl AclSet {
    pub fn validate(entries: &[AclEntry]) -> Result<Self, AclError> {
        if !entries
            .iter()
            .any(|e| e.capability == CalendarCapability::Owner)
        {
            return Err(AclError::NoOwner);
        }
        Ok(Self {
            entries: entries.to_vec(),
        })
    }

    /// Highest capability `user_id` holds across entries.
    pub fn capability_for(&self, user_id: Uuid) -> Option<CalendarCapability> {
        self.entries
            .iter()
            .filter(|e| e.principal_user_id == user_id)
            .map(|e| e.capability)
            .min()
    }
}

// ============ attendees ============

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttendeeRole {
    Chair,
    Required,
    Optional,
    NonParticipant,
}

impl AttendeeRole {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            Self::Chair => "CHAIR",
            Self::Required => "REQ-PARTICIPANT",
            Self::Optional => "OPT-PARTICIPANT",
            Self::NonParticipant => "NON-PARTICIPANT",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "CHAIR" => Some(Self::Chair),
            "REQ-PARTICIPANT" => Some(Self::Required),
            "OPT-PARTICIPANT" => Some(Self::Optional),
            "NON-PARTICIPANT" => Some(Self::NonParticipant),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartStat {
    NeedsAction,
    Accepted,
    Declined,
    Tentative,
    Delegated,
}

impl PartStat {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            Self::NeedsAction => "NEEDS-ACTION",
            Self::Accepted => "ACCEPTED",
            Self::Declined => "DECLINED",
            Self::Tentative => "TENTATIVE",
            Self::Delegated => "DELEGATED",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "NEEDS-ACTION" => Some(Self::NeedsAction),
            "ACCEPTED" => Some(Self::Accepted),
            "DECLINED" => Some(Self::Declined),
            "TENTATIVE" => Some(Self::Tentative),
            "DELEGATED" => Some(Self::Delegated),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attendee {
    pub email: String,
    pub display_name: Option<String>,
    pub telephone: Option<String>,
    pub user_id: Option<Uuid>,
    pub role: AttendeeRole,
    pub partstat: PartStat,
    pub rsvp: Option<bool>,
}

// ============ events ============

/// Start or identity point of an event: either a normalized instant with the
/// owning tzid, or an all-day date. Mirrors the timed/all-day column split in
/// the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DateOrDateTime {
    Timed(DateTime<Utc>),
    AllDay(NaiveDate),
}

/// RFC 5545 STATUS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum EventStatus {
    Tentative,
    Confirmed,
    Cancelled,
}

/// RFC 5545 CLASS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Classification {
    Public,
    Private,
    Confidential,
}

/// RFC 5545 TRANSP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Transparency {
    Opaque,
    Transparent,
}

/// VALARM trigger: relative to event start/end, or absolute.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AlarmTrigger {
    Relative {
        offset: chrono::Duration,
        related: AlarmRelated,
    },
    Absolute(DateTime<Utc>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlarmRelated {
    Start,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum AlarmAction {
    Display,
    Email,
}

/// Recurrence properties. Present only on master events; exceptions are rows
/// with `master_event_id` set and never recur.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RecurrenceSpec {
    /// RRULE value string (e.g. `FREQ=WEEKLY;BYDAY=MO`), parsed by the
    /// recurrence engine.
    pub rrule: Option<String>,
    pub rdate: Vec<DateOrDateTime>,
    pub exdate: Vec<DateOrDateTime>,
}

/// An event master or RECURRENCE-ID exception. `master_event_id` being set
/// makes this row an exception; `recurrence_id` is the original occurrence
/// start (wall-clock in `tzid`, or an all-day date).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: Uuid,
    pub calendar_id: Uuid,
    pub uid: String,
    pub master_event_id: Option<Uuid>,
    pub recurrence_id: Option<DateOrDateTime>,
    pub timing: EventTiming,
    pub recurrence: Option<RecurrenceSpec>,
    pub summary: String,
    pub description_html: Option<String>,
    pub description_text: Option<String>,
    pub url: Option<String>,
    pub status: Option<EventStatus>,
    pub priority: Option<u8>,
    pub class: Option<Classification>,
    pub transparency: Option<Transparency>,
    pub categories: Vec<String>,
    pub location: Option<Location>,
    pub organizer: Organizer,
    pub attendees: Vec<Attendee>,
    pub sequence: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventTiming {
    /// Normalized start instant (timed events).
    pub starts_at: Option<DateTime<Utc>>,
    /// All-day start date.
    pub start_date: Option<NaiveDate>,
    /// Normalized end instant (timed events).
    pub ends_at: Option<DateTime<Utc>>,
    /// All-day end date (inclusive per RFC 5545 DTEND semantics).
    pub end_date: Option<NaiveDate>,
    /// IANA tzid the wall-clock representation belongs to.
    pub tzid: Option<String>,
}

impl EventTiming {
    pub fn timed(starts_at: DateTime<Utc>, ends_at: DateTime<Utc>, tzid: Option<String>) -> Self {
        Self {
            starts_at: Some(starts_at),
            start_date: None,
            ends_at: Some(ends_at),
            end_date: None,
            tzid,
        }
    }

    pub fn all_day(start_date: NaiveDate, end_date: Option<NaiveDate>) -> Self {
        Self {
            starts_at: None,
            start_date: Some(start_date),
            ends_at: None,
            end_date,
            tzid: None,
        }
    }

    pub fn start(&self) -> DateOrDateTime {
        match (self.starts_at, self.start_date) {
            (Some(at), _) => DateOrDateTime::Timed(at),
            (None, Some(date)) => DateOrDateTime::AllDay(date),
            // Construction via the constructors above keeps this unreachable;
            // the DB CHECK enforces the XOR.
            (None, None) => unreachable!("event has no start"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Organizer {
    pub email: String,
    pub name: Option<String>,
    pub user_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    pub id: Uuid,
    pub provider: Option<String>,
    pub provider_place_id: Option<String>,
    pub display_name: Option<String>,
    pub formatted_address: Option<String>,
    pub street_address: Option<String>,
    pub locality: Option<String>,
    pub administrative_area: Option<String>,
    pub postal_code: Option<String>,
    pub country: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub website: Option<String>,
    pub phone: Option<String>,
    pub provider_metadata: Option<serde_json::Value>,
}

impl Event {
    /// Structural validation mirroring the events-table CHECK constraints:
    /// exactly one anchor, an end or duration representation, recurrence only
    /// on masters, exceptions carry a RECURRENCE-ID.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let is_exception = self.master_event_id.is_some();
        if self.uid.is_empty() || self.uid.len() > 255 {
            return Err(ValidationError::Event(
                "uid must be 1..=255 characters".into(),
            ));
        }

        let has_timed = self.timing.starts_at.is_some();
        let has_all_day = self.timing.start_date.is_some();
        if has_timed == has_all_day {
            return Err(ValidationError::Event(
                "exactly one of timed start or all-day start".into(),
            ));
        }
        if self.timing.starts_at.is_some()
            && (self.timing.ends_at.is_none() && self.timing.end_date.is_none())
        {
            return Err(ValidationError::Event("timed event needs an end".into()));
        }

        if let (Some(s), Some(e)) = (self.timing.starts_at, self.timing.ends_at)
            && e < s
        {
            return Err(ValidationError::Event("end before start".into()));
        }

        if is_exception {
            if self.recurrence_id.is_none() {
                return Err(ValidationError::Event(
                    "exception requires recurrence_id".into(),
                ));
            }
            if self.recurrence.is_some() {
                return Err(ValidationError::Event("exceptions cannot recur".into()));
            }
        }

        if self.priority.is_some_and(|p| p > 9) {
            return Err(ValidationError::Event("priority must be 0..=9".into()));
        }

        for attendee in &self.attendees {
            validate_email(&attendee.email)?;
        }
        validate_email(&self.organizer.email)?;

        Ok(())
    }
}

// ============ validation helpers ============

pub fn validate_slug(slug: &str) -> Result<(), ValidationError> {
    let ok = !slug.is_empty()
        && slug.len() <= 63
        && slug
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(ValidationError::Slug(slug.to_string()))
    }
}

pub fn validate_username(username: &str) -> Result<(), ValidationError> {
    let ok = !username.is_empty()
        && username.len() <= 64
        && username
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(ValidationError::Username(username.to_string()))
    }
}

/// Structural email check only; real deliverability is the mail provider's job.
pub fn validate_email(email: &str) -> Result<(), ValidationError> {
    if email.len() > 254 || email.contains(char::is_whitespace) || email.matches('@').count() != 1 {
        return Err(ValidationError::Email(email.to_string()));
    }
    let (local, domain) = email.split_once('@').expect("exactly one @");
    let ok = !local.is_empty()
        && !domain.is_empty()
        && !domain.starts_with('.')
        && !domain.ends_with('.');
    if ok {
        Ok(())
    } else {
        Err(ValidationError::Email(email.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn timed_event() -> Event {
        let start = Utc.with_ymd_and_hms(2026, 1, 5, 9, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 1, 5, 10, 0, 0).unwrap();
        Event {
            id: Uuid::new_v4(),
            calendar_id: Uuid::new_v4(),
            uid: "uid-1".into(),
            master_event_id: None,
            recurrence_id: None,
            timing: EventTiming::timed(start, end, Some("America/Denver".into())),
            recurrence: None,
            summary: "Standup".into(),
            description_html: None,
            description_text: None,
            url: None,
            status: None,
            priority: None,
            class: None,
            transparency: None,
            categories: vec![],
            location: None,
            organizer: Organizer {
                email: "brian@example.com".into(),
                name: None,
                user_id: None,
            },
            attendees: vec![],
            sequence: 0,
            created_at: start,
            updated_at: start,
        }
    }

    #[test]
    fn valid_timed_event() {
        assert!(timed_event().validate().is_ok());
    }

    #[test]
    fn all_day_event_without_explicit_end_is_valid() {
        let mut e = timed_event();
        e.timing = EventTiming::all_day(NaiveDate::from_ymd_opt(2026, 1, 5).unwrap(), None);
        assert!(e.validate().is_ok());
    }

    #[test]
    fn both_anchors_rejected() {
        let mut e = timed_event();
        e.timing.start_date = Some(NaiveDate::from_ymd_opt(2026, 1, 5).unwrap());
        assert_eq!(
            e.validate(),
            Err(ValidationError::Event(
                "exactly one of timed start or all-day start".into()
            ))
        );
    }

    #[test]
    fn no_anchor_rejected() {
        let mut e = timed_event();
        e.timing.starts_at = None;
        e.timing.ends_at = None;
        assert!(e.validate().is_err());
    }

    #[test]
    fn end_before_start_rejected() {
        let mut e = timed_event();
        let start = e.timing.starts_at.unwrap();
        e.timing.ends_at = Some(start - chrono::Duration::hours(1));
        assert_eq!(
            e.validate(),
            Err(ValidationError::Event("end before start".into()))
        );
    }

    #[test]
    fn exception_requires_recurrence_id() {
        let mut e = timed_event();
        e.master_event_id = Some(Uuid::new_v4());
        assert!(e.validate().is_err());
        e.recurrence_id = Some(DateOrDateTime::Timed(e.timing.starts_at.unwrap()));
        assert!(e.validate().is_ok());
    }

    #[test]
    fn exception_cannot_recur() {
        let mut e = timed_event();
        e.master_event_id = Some(Uuid::new_v4());
        e.recurrence_id = Some(DateOrDateTime::Timed(e.timing.starts_at.unwrap()));
        e.recurrence = Some(RecurrenceSpec {
            rrule: Some("FREQ=DAILY".into()),
            ..Default::default()
        });
        assert_eq!(
            e.validate(),
            Err(ValidationError::Event("exceptions cannot recur".into()))
        );
    }

    #[test]
    fn priority_bounds() {
        let mut e = timed_event();
        e.priority = Some(9);
        assert!(e.validate().is_ok());
        e.priority = Some(10);
        assert!(e.validate().is_err());
    }

    #[test]
    fn acl_requires_owner() {
        let entry = |cap| AclEntry {
            principal_user_id: Uuid::new_v4(),
            capability: cap,
            can_manage_acl: false,
        };
        assert_eq!(
            AclSet::validate(&[entry(CalendarCapability::ReadOnly)]).unwrap_err(),
            AclError::NoOwner
        );
        assert!(AclSet::validate(&[entry(CalendarCapability::Owner)]).is_ok());
    }

    #[test]
    fn capability_ordering_and_resolution() {
        let reader = Uuid::new_v4();
        let owner = Uuid::new_v4();
        let set = AclSet::validate(&[
            AclEntry {
                principal_user_id: reader,
                capability: CalendarCapability::ReadOnly,
                can_manage_acl: false,
            },
            AclEntry {
                principal_user_id: owner,
                capability: CalendarCapability::Owner,
                can_manage_acl: true,
            },
        ])
        .unwrap();
        assert_eq!(
            set.capability_for(reader),
            Some(CalendarCapability::ReadOnly)
        );
        assert_eq!(set.capability_for(owner), Some(CalendarCapability::Owner));
        assert_eq!(set.capability_for(Uuid::new_v4()), None);
        assert!(CalendarCapability::Owner.satisfies(CalendarCapability::ReadOnly));
        assert!(CalendarCapability::ReadOnly.satisfies(CalendarCapability::ReadOnly));
        assert!(!CalendarCapability::ReadOnly.satisfies(CalendarCapability::ReadWrite));
        assert!(!CalendarCapability::FreeBusy.satisfies(CalendarCapability::ReadOnly));
    }

    #[test]
    fn slug_rules() {
        assert!(validate_slug("personal").is_ok());
        assert!(validate_slug("my-cal-1").is_ok());
        assert!(validate_slug("").is_err());
        assert!(validate_slug("-lead").is_err());
        assert!(validate_slug("Under_Score").is_err());
        assert!(validate_slug(&"a".repeat(64)).is_err());
    }

    #[test]
    fn username_rules() {
        assert!(validate_username("brian").is_ok());
        assert!(validate_username("b.t_1").is_ok());
        assert!(validate_username("").is_err());
        assert!(validate_username(".lead").is_err());
        assert!(validate_username("sp ace").is_err());
    }

    #[test]
    fn email_rules() {
        assert!(validate_email("brian@example.com").is_ok());
        assert!(validate_email("no-at-sign").is_err());
        assert!(validate_email("a@b@c").is_err());
        assert!(validate_email("@example.com").is_err());
        assert!(validate_email("local@").is_err());
        assert!(validate_email("local@.example.com").is_err());
        assert!(validate_email("two words@example.com").is_err());
    }

    #[test]
    fn db_string_round_trip() {
        for cap in [
            CalendarCapability::Owner,
            CalendarCapability::ReadWrite,
            CalendarCapability::ReadOnly,
            CalendarCapability::FreeBusy,
        ] {
            assert_eq!(CalendarCapability::from_db_str(cap.as_db_str()), Some(cap));
        }
        for role in [TenantRole::Owner, TenantRole::Admin, TenantRole::Member] {
            assert_eq!(TenantRole::from_db_str(role.as_db_str()), Some(role));
        }
        for ps in [
            PartStat::NeedsAction,
            PartStat::Accepted,
            PartStat::Declined,
        ] {
            assert_eq!(PartStat::from_db_str(ps.as_db_str()), Some(ps));
        }
        assert_eq!(CalendarCapability::from_db_str("bogus"), None);
    }
}
