//! CalDAV adapter: iCalendar parse/serialize between the normalized model
//! (calendar-db rows) and RFC 5545 wire format, the dav-server-rs guarded
//! filesystem adapter, and CalDAV REPORT helpers dav-server lacks.

pub mod adapter;
pub mod journal;
pub mod store;
pub mod todo;

pub use adapter::{DavAuth, PgDavFs, upsert_for};
pub use journal::{ParsedJournal, journal_to_ics};
pub(crate) use store::upsert_data;
pub use todo::{ParsedTodo, ParsedTodoSeries, TaskExportRow, todos_to_ics};

use calendar_core::DateOrDateTime;
use calendar_db::{AttendeeRow, EventRow};
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use icalendar::{Calendar, Component, DatePerhapsTime, Event, EventLike};

#[derive(Debug, thiserror::Error)]
pub enum IcsError {
    #[error("a resource must hold only one component kind (VEVENT/VTODO/VJOURNAL)")]
    MixedComponents,
    #[error("a resource must hold only one UID")]
    UidMismatch,
    #[error("VTIMEZONE {0} is not compilable: {1}")]
    UnsupportedTimezone(String, String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("VEVENT missing UID")]
    MissingUid,
    #[error("VEVENT missing DTSTART")]
    MissingDtstart,
}

/// Injection-safety cap on one row's `extra_props` (design section 4): the
/// JSON encoding of every unmodelled property together must stay at or under
/// the attachment cap. Constant because calendar-caldav sees no config; keep
/// in sync with the server's ATTACHMENT_MAX_BYTES default (50 MiB).
pub const EXTRA_PROPS_MAX_BYTES: usize = 50 * 1024 * 1024;

/// One unmodelled property preserved verbatim (D2): X-*, ATTACH, GEO,
/// COMMENT, RELATED-TO with a non-PARENT RELTYPE, ...
#[derive(Debug, Clone, PartialEq)]
pub struct ExtraProp {
    pub name: String,
    pub params: Vec<(String, String)>,
    pub value: String,
}

/// Accepts an extra-props list only per the injection-safety rules: name is
/// an ICS token (`^[A-Za-z0-9-]+$`), no CR/LF anywhere (the parser has
/// already unfolded), and the row's total encoded size within
/// [`EXTRA_PROPS_MAX_BYTES`] — the `max-resource-size` precondition.
pub(crate) fn extra_props_json(props: &[ExtraProp]) -> Result<serde_json::Value, IcsError> {
    let mut total = 0usize;
    let entries: Vec<serde_json::Value> = props
        .iter()
        .map(|p| {
            if p.name.is_empty()
                || !p
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
                return Err(IcsError::Parse(format!(
                    "unmodelled property name {p:?} is not a valid ICS token"
                )));
            }
            if p.value.contains(['\r', '\n'])
                || p.params
                    .iter()
                    .any(|(k, v)| k.contains(['\r', '\n']) || v.contains(['\r', '\n']))
            {
                return Err(IcsError::Parse(format!(
                    "property {p:?} carries a line break"
                )));
            }
            total += p.name.len()
                + p.value.len()
                + p.params
                    .iter()
                    .map(|(k, v)| k.len() + v.len() + 8)
                    .sum::<usize>();
            Ok(serde_json::json!({
                "name": p.name,
                "params": p
                    .params
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::json!(v)))
                    .collect::<serde_json::Map<String, serde_json::Value>>(),
                "value": p.value,
            }))
        })
        .collect::<Result<_, _>>()?;
    if total > EXTRA_PROPS_MAX_BYTES {
        return Err(IcsError::Parse(
            "unmodelled properties exceed max-resource-size".into(),
        ));
    }
    Ok(serde_json::Value::Array(entries))
}

/// Reads stored `extra_props` back, dropping any entry that no longer
/// satisfies the injection-safety rules (guards hand-written rows).
pub(crate) fn extra_props_from_json(value: &serde_json::Value) -> Vec<ExtraProp> {
    value
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| {
                    let name = e.get("name")?.as_str()?.to_string();
                    let value = e.get("value")?.as_str()?.to_string();
                    let params = e
                        .get("params")
                        .and_then(|p| p.as_object())
                        .map(|m| {
                            m.iter()
                                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                                .collect()
                        })
                        .unwrap_or_default();
                    let prop = ExtraProp {
                        name,
                        params,
                        value,
                    };
                    (prop
                        .name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                        && !prop.value.contains(['\r', '\n'])
                        && !prop
                            .params
                            .iter()
                            .any(|(k, v)| k.contains(['\r', '\n']) || v.contains(['\r', '\n'])))
                    .then_some(prop)
                })
                .collect()
        })
        .unwrap_or_default()
}

// ============ serialization ============

/// RFC 5545 unfolding: a line beginning with space or tab continues the
/// previous line.
pub(crate) fn unfold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if (line.starts_with(' ') || line.starts_with('\t')) && !out.is_empty() {
            out.push_str(line.trim_start_matches([' ', '\t']));
        } else {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(line);
        }
    }
    out.push('\n');
    out
}

fn today() -> NaiveDate {
    Utc::now().date_naive()
}

fn known_tz(tzid: Option<&str>) -> Option<Tz> {
    tzid.and_then(|tz| tz.parse::<Tz>().ok())
}

/// DTSTART/DTEND/RECURRENCE-ID/DUE property: `VALUE=DATE` for all-day, the bare
/// wall clock for floating, local wall-clock with TZID for known or
/// custom-stored zones, UTC with `Z` otherwise.
pub(crate) fn date_time_property(
    key: &str,
    tzid: Option<&str>,
    all_day: bool,
    floating: bool,
    at: DateTime<Utc>,
    date: NaiveDate,
    custom_zones: &std::collections::HashMap<String, calendar_core::recurrence::Zone>,
) -> icalendar::Property {
    let tzid = tzid.filter(|_| !floating);
    let zone = tzid.filter(|t| known_tz(Some(t)).is_some() || custom_zones.contains_key(*t));
    let mut prop = if all_day {
        icalendar::Property::new(key, date.format("%Y%m%d").to_string())
    } else if floating {
        icalendar::Property::new(key, at.format("%Y%m%dT%H%M%S").to_string())
    } else if let Some(tz) = zone {
        let local = match known_tz(Some(tz)) {
            Some(tz) => at.with_timezone(&tz).format("%Y%m%dT%H%M%S").to_string(),
            // Custom zone: render the wall clock via its compiled offsets.
            None => custom_zones.get(tz).map_or_else(
                || at.format("%Y%m%dT%H%M%S").to_string(),
                |zone| zone.to_local(at).format("%Y%m%dT%H%M%S").to_string(),
            ),
        };
        icalendar::Property::new(key, local)
    } else {
        icalendar::Property::new(key, at.format("%Y%m%dT%H%M%SZ").to_string())
    };
    if all_day {
        prop.add_parameter("VALUE", "DATE");
    } else if let Some(tz) = zone {
        prop.add_parameter("TZID", tz);
    }
    prop
}

fn recurrence_id_property(
    event: &EventRow,
    custom_zones: &std::collections::HashMap<String, calendar_core::recurrence::Zone>,
) -> Option<icalendar::Property> {
    if let Some(date) = event.recurrence_id_date {
        let mut prop = icalendar::Property::new("RECURRENCE-ID", date.format("%Y%m%d").to_string());
        prop.add_parameter("VALUE", "DATE");
        Some(prop)
    } else if let Some(naive) = event.recurrence_id {
        // RECURRENCE-ID is stored as wall clock in the event's zone.
        let has_zone = !event.floating
            && event
                .tzid
                .as_deref()
                .is_some_and(|t| known_tz(Some(t)).is_some() || custom_zones.contains_key(t));
        let mut prop = if has_zone || event.floating {
            icalendar::Property::new("RECURRENCE-ID", naive.format("%Y%m%dT%H%M%S").to_string())
        } else {
            icalendar::Property::new("RECURRENCE-ID", naive.format("%Y%m%dT%H%M%SZ").to_string())
        };
        if has_zone {
            prop.add_parameter("TZID", event.tzid.as_deref().unwrap_or_default());
        }
        Some(prop)
    } else {
        None
    }
}

/// One exportable resource: event row, attendees, location and its VALARM set.
pub struct ExportRow {
    pub event: EventRow,
    pub attendees: Vec<AttendeeRow>,
    pub alarms: Vec<calendar_db::alarms::AlarmRow>,
    pub location: Option<calendar_db::LocationRow>,
    /// Stored VTIMEZONEs for the calendar (ADR-012): the raw definition is
    /// re-emitted at the top of the VCALENDAR so clients round-trip their own
    /// zone, and the compiled rules render custom-zone wall clocks. Empty
    /// when the calendar has no client-supplied zones.
    pub vtimezones: Vec<calendar_db::timezones::StoredTimezone>,
}

impl From<(EventRow, Vec<AttendeeRow>)> for ExportRow {
    fn from((event, attendees): (EventRow, Vec<AttendeeRow>)) -> Self {
        Self {
            event,
            attendees,
            alarms: vec![],
            location: None,
            vtimezones: vec![],
        }
    }
}

/// Custom (non-tzdb) zones of an export, compiled to offset transitions for
/// wall-clock rendering.
fn custom_zones_of(
    rows: &[ExportRow],
) -> std::collections::HashMap<String, calendar_core::recurrence::Zone> {
    rows.iter()
        .flat_map(|row| row.vtimezones.iter())
        .filter(|stored| !calendar_core::recurrence::is_tzdb_tzid(&stored.tzid))
        .filter_map(|stored| {
            compiled_zone(&stored.tzid, &stored.rules).map(|zone| (stored.tzid.clone(), zone))
        })
        .collect()
}

/// One VEVENT per row (masters and exceptions alike); a calendar export is a
/// VCALENDAR of all of them, with the calendar's stored VTIMEZONE components
/// spliced in ahead of the events.
pub fn events_to_ics(rows: &[ExportRow]) -> String {
    let mut calendar = Calendar::new();
    calendar.name("calendar-server");
    // Custom (non-tzdb) zones in the export: DTSTART/DTEND/RECURRENCE-ID with
    // such a TZID render as local wall clock with the TZID parameter.
    let custom_zones = custom_zones_of(rows);
    for row in rows {
        let (event, attendees) = (&row.event, &row.attendees);
        let alarms = &row.alarms;
        let mut ev = Event::new();
        ev.uid(&event.uid).summary(&event.summary);
        if let Some(text) = &event.description_text {
            ev.description(text);
        }
        if let Some(html) = &event.description_html {
            let mut html_prop = icalendar::Property::new("X-ALT-DESC", html);
            html_prop.add_parameter("FMTTYPE", "text/html");
            ev.append_property(html_prop);
        }
        if let Some(url) = &event.url {
            ev.url(url);
        }
        match (event.starts_at, event.start_date) {
            (Some(at), _) => {
                ev.append_property(date_time_property(
                    "DTSTART",
                    event.tzid.as_deref(),
                    false,
                    event.floating,
                    at,
                    today(),
                    &custom_zones,
                ));
            }
            (None, Some(date)) => {
                ev.append_property(date_time_property(
                    "DTSTART",
                    event.tzid.as_deref(),
                    true,
                    false,
                    Utc::now(),
                    date,
                    &custom_zones,
                ));
            }
            (None, None) => {}
        }
        if let Some(at) = event.ends_at {
            ev.append_property(date_time_property(
                "DTEND",
                event.tzid.as_deref(),
                false,
                event.floating,
                at,
                today(),
                &custom_zones,
            ));
        }
        if let Some(date) = event.end_date {
            ev.append_property(date_time_property(
                "DTEND",
                event.tzid.as_deref(),
                true,
                false,
                Utc::now(),
                date,
                &custom_zones,
            ));
        }
        if let Some(rrule) = &event.rrule {
            ev.add_property("RRULE", rrule);
        }
        if event.rdate.as_array().is_some_and(|a| !a.is_empty()) {
            let values = json_points_to_ics(&event.rdate, event.floating);
            ev.add_property("RDATE", &values);
        }
        if event.exdate.as_array().is_some_and(|a| !a.is_empty()) {
            let values = json_points_to_ics(&event.exdate, event.floating);
            ev.add_property("EXDATE", &values);
        }
        if let Some(recurrence) = recurrence_id_property(event, &custom_zones) {
            ev.append_property(recurrence);
        }
        if let Some(status) = &event.status {
            ev.add_property("STATUS", status);
        }
        if let Some(priority) = event.priority {
            ev.priority(priority as u32);
        }
        if let Some(class) = &event.class {
            ev.append_property(icalendar::Property::new("CLASS", class.as_str()));
        }
        if let Some(transp) = &event.transp {
            ev.add_property("TRANSP", transp);
        }
        if !event.categories.is_empty() {
            ev.add_property("CATEGORIES", event.categories.join(","));
        }
        if let Some(loc) = &row.location {
            let text = loc
                .display_name
                .clone()
                .or_else(|| loc.formatted_address.clone())
                .unwrap_or_default();
            if !text.is_empty() {
                ev.location(&text);
            }
            if let (Some(lat), Some(lon)) = (loc.latitude, loc.longitude) {
                ev.append_property(icalendar::Property::new("GEO", format!("{lat};{lon}")));
            }
        }
        let mut organizer =
            icalendar::Property::new("ORGANIZER", format!("mailto:{}", event.organizer_email));
        organizer.add_parameter("CN", event.organizer_name.as_deref().unwrap_or(""));
        ev.append_property(organizer);
        for attendee in attendees {
            // SMS-only attendees carry an sms: CAL-ADDRESS instead of mailto:.
            let value = match (&attendee.email, &attendee.telephone) {
                (Some(email), _) => format!("mailto:{}", email),
                (None, Some(phone)) => format!("sms:{}", phone),
                (None, None) => continue,
            };
            let mut prop = icalendar::Property::new("ATTENDEE", value);
            prop.add_parameter("CN", attendee.display_name.as_deref().unwrap_or(""));
            prop.add_parameter("PARTSTAT", &attendee.partstat);
            prop.add_parameter("ROLE", &attendee.role);
            if let Some(rsvp) = attendee.rsvp {
                prop.add_parameter("RSVP", if rsvp { "TRUE" } else { "FALSE" });
            }
            ev.append_property(prop);
        }
        for alarm in alarms {
            serialize_alarm(&mut ev, alarm, event);
        }
        ev.sequence(event.sequence.max(0) as u32);
        let stamp = icalendar::Property::new(
            "DTSTAMP",
            event.updated_at.format("%Y%m%dT%H%M%SZ").to_string(),
        );
        ev.append_property(stamp);
        ev.last_modified(event.updated_at);
        calendar.push(ev);
    }
    let mut out = calendar.to_string();
    // Splice the calendar's stored VTIMEZONE components in ahead of the
    // events (icalendar has no VTIMEZONE builder). Definitions are unfolded
    // text; re-fold nothing, just normalize line endings to CRLF.
    let mut zone_text = String::new();
    let mut emitted = std::collections::HashSet::new();
    for row in rows {
        for stored in &row.vtimezones {
            if !emitted.insert(stored.tzid.clone()) {
                continue;
            }
            for line in stored.definition.lines() {
                zone_text.push_str(line.trim_end_matches('\r'));
                zone_text.push_str("\r\n");
            }
        }
    }
    if !zone_text.is_empty()
        && let Some(pos) = out
            .find("BEGIN:VEVENT")
            .or_else(|| out.rfind("END:VCALENDAR"))
    {
        out.insert_str(pos, &zone_text);
    }
    out
}

/// VALARM: ACTION, TRIGGER (relative or absolute), RELATED, recipients.
/// Built through `Alarm::display` (the only public constructor) and adjusted
/// property-by-property for the EMAIL action.
fn serialize_alarm(
    ev: &mut icalendar::Event,
    alarm: &calendar_db::alarms::AlarmRow,
    event: &EventRow,
) {
    use icalendar::{Related, Trigger};
    let related = if alarm.related.as_deref() == Some("END") {
        Related::End
    } else {
        Related::Start
    };
    let trigger = match alarm.trigger_at {
        Some(at) => Trigger::DateTime(icalendar::CalendarDateTime::Utc(at)),
        None => Trigger::Duration(
            chrono::Duration::seconds(alarm.offset_secs().unwrap_or(0)),
            Some(related),
        ),
    };
    let description = alarm
        .description
        .clone()
        .unwrap_or_else(|| event.summary.clone());
    let mut valarm = icalendar::Alarm::display(&description, trigger);
    // Wire keeps RFC 5545 semantics: email is the only non-DISPLAY action;
    // sms/push selections never reach the ICS.
    if alarm.notify_channels.iter().any(|c| c == "email") {
        valarm
            .remove_property("ACTION")
            .add_property("ACTION", "EMAIL");
        for recipient in &alarm.recipient_emails {
            valarm.append_property(icalendar::Property::new(
                "ATTENDEE",
                format!("mailto:{recipient}"),
            ));
        }
        if let Some(summary) = &alarm.summary {
            valarm.add_property("SUMMARY", summary);
        }
    }
    icalendar::EventLike::alarm(ev, valarm);
}

fn json_points_to_ics(value: &serde_json::Value, floating: bool) -> String {
    let time_format = if floating {
        "%Y%m%dT%H%M%S"
    } else {
        "%Y%m%dT%H%M%SZ"
    };
    value
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| {
                    if let Ok(at) = DateTime::parse_from_rfc3339(s) {
                        at.format(time_format).to_string()
                    } else if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                        d.format("%Y%m%d").to_string()
                    } else {
                        s.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default()
}

// ============ parsing ============

/// Parsed VEVENT fields, normalized to the schema's column shapes.
#[derive(Debug, Default, Clone)]
pub struct ParsedEvent {
    pub uid: String,
    pub alarms: Vec<ParsedAlarm>,
    pub summary: Option<String>,
    pub description_text: Option<String>,
    pub description_html: Option<String>,
    pub url: Option<String>,
    pub location_text: Option<String>,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub start_date: Option<NaiveDate>,
    pub end_date: Option<NaiveDate>,
    /// DURATION property in seconds (when the client sent DURATION not DTEND).
    pub duration_secs: Option<i64>,
    pub tzid: Option<String>,
    pub all_day: bool,
    /// DTSTART carried neither Z nor TZID; times are the wall clock as if UTC.
    pub floating: bool,
    pub rrule: Option<String>,
    pub rdate: Vec<DateOrDateTime>,
    pub exdate: Vec<DateOrDateTime>,
    pub status: Option<String>,
    pub priority: Option<i16>,
    pub class: Option<String>,
    pub transp: Option<String>,
    pub categories: Vec<String>,
    pub organizer_email: Option<String>,
    pub organizer_name: Option<String>,
    pub attendees: Vec<ParsedAttendee>,
    pub sequence: Option<i32>,
    pub recurrence_id: Option<NaiveDateTime>,
    pub recurrence_id_date: Option<NaiveDate>,
    /// VCALENDAR-level METHOD (REQUEST/REPLY/CANCEL/...) when present. Lives
    /// on each event because `parse_ics` returns a flat Vec of VEVENTs.
    pub method: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct ParsedAlarm {
    pub action: String, // DISPLAY | EMAIL
    pub related: Option<String>,
    /// Relative trigger in seconds (negative = before).
    pub offset_secs: Option<i64>,
    pub trigger_at: Option<DateTime<Utc>>,
    pub description: Option<String>,
    pub summary: Option<String>,
    pub recipients: Vec<String>,
}

#[derive(Debug, Default, Clone)]
pub struct ParsedAttendee {
    /// None for SMS-only attendees (`sms:` CAL-ADDRESS).
    pub email: Option<String>,
    pub telephone: Option<String>,
    pub display_name: Option<String>,
    pub role: Option<String>,
    pub partstat: Option<String>,
    pub rsvp: Option<bool>,
}

/// Parsed VCALENDAR: the VEVENTs plus any client-supplied VTIMEZONEs
/// (ADR-012), ready for the PUT storage path.
#[derive(Debug, Default)]
pub struct ParsedCalendar {
    pub events: Vec<ParsedEvent>,
    pub timezones: Vec<ParsedTimezone>,
}

/// A parsed client-supplied VTIMEZONE: the tzid, the raw component text
/// (re-emitted on export) and the compiled STANDARD/DAYLIGHT rules.
#[derive(Debug, Clone)]
pub struct ParsedTimezone {
    pub tzid: String,
    pub definition: String,
    pub rules: Vec<calendar_core::recurrence::ZoneRule>,
}

/// Parses one VCALENDAR into its VEVENTs and VTIMEZONEs. Non-VEVENT
/// components are ignored (VTODO/VJOURNAL go through [`parse_resource`]); a
/// non-tzdb VTIMEZONE that cannot be compiled is rejected with
/// `UnsupportedTimezone` naming the tzid (ADR-012) — nothing that would
/// later expand as UTC is ever stored.
pub fn parse_calendar(text: &str) -> Result<ParsedCalendar, IcsError> {
    // The icalendar 0.17 parser rejects RFC 5545 line folding; unfold first.
    let unfolded = unfold(text);
    let calendar = icalendar::parser::read_calendar(&unfolded).map_err(IcsError::Parse)?;
    parse_calendar_parts(&unfolded, &calendar)
}

fn parse_calendar_parts(
    unfolded: &str,
    calendar: &icalendar::parser::Calendar<'_>,
) -> Result<ParsedCalendar, IcsError> {
    let method = calendar
        .properties
        .iter()
        .find(|prop| prop.name.as_ref() == "METHOD")
        .map(|prop| prop.val.as_str().trim().to_string())
        .filter(|m| !m.is_empty());
    let timezones = parse_timezones(unfolded, calendar)?;
    // Custom zones compiled for event parsing: DTSTART with a custom TZID is
    // converted wall → instant via the zone the client supplied alongside it.
    let custom = compiled_zones(&timezones);
    let mut out = Vec::new();
    for component in &calendar.components {
        if component.name.as_ref() != "VEVENT" {
            continue;
        }
        let alarms: Vec<ParsedAlarm> = component
            .components
            .iter()
            .filter(|sub| sub.name.as_ref() == "VALARM")
            .map(|sub| parse_alarm(sub))
            .collect();
        let event = to_owned_event(component);
        let mut parsed = parse_event(event, &custom)?;
        parsed.alarms = alarms;
        parsed.method = method.clone();
        out.push(parsed);
    }
    Ok(ParsedCalendar {
        events: out,
        timezones,
    })
}

/// tzid → compiled zone, for custom (client-supplied VTIMEZONE) zones.
pub(crate) type Zones = std::collections::HashMap<String, calendar_core::recurrence::Zone>;

/// Parses one VCALENDAR into a PUT-ready resource: VEVENTs (a series), a
/// VTODO series (master plus RECURRENCE-ID overrides, one UID), or a single
/// VJOURNAL. Mixing component kinds is `MixedComponents`; VTODOs with more
/// than one UID are `UidMismatch`. VTIMEZONEs are ignored for the event
/// path's callers (`parse_ics`) but ride along on the task series for the
/// calendar's stored-zone table (ADR-012).
pub fn parse_resource(text: &str) -> Result<ParsedResource, IcsError> {
    let unfolded = unfold(text);
    let calendar = icalendar::parser::read_calendar(&unfolded).map_err(IcsError::Parse)?;
    let mut events = 0usize;
    let mut todos: Vec<&icalendar::parser::Component<'_>> = Vec::new();
    let mut journals: Vec<&icalendar::parser::Component<'_>> = Vec::new();
    for component in &calendar.components {
        match component.name.as_ref() {
            "VEVENT" => events += 1,
            "VTODO" => todos.push(component),
            "VJOURNAL" => journals.push(component),
            _ => {}
        }
    }
    if (events > 0 && (!todos.is_empty() || !journals.is_empty()))
        || (!todos.is_empty() && !journals.is_empty())
    {
        return Err(IcsError::MixedComponents);
    }
    let parsed = parse_calendar_parts(&unfolded, &calendar)?;
    if events > 0 || (todos.is_empty() && journals.is_empty()) {
        return Ok(ParsedResource::Events(parsed));
    }
    if !todos.is_empty() {
        let uid = todos[0]
            .properties
            .iter()
            .find(|p| p.name.as_ref() == "UID")
            .map(|p| p.val.as_str().trim().to_string());
        for todo in &todos[1..] {
            let other = todo
                .properties
                .iter()
                .find(|p| p.name.as_ref() == "UID")
                .map(|p| p.val.as_str().trim().to_string());
            if other != uid {
                return Err(IcsError::UidMismatch);
            }
        }
        let custom = compiled_zones(&parsed.timezones);
        let mut masters = Vec::new();
        let mut overrides = Vec::new();
        for component in todos {
            let parsed = todo::parse_todo(component, &custom)?;
            if parsed.recurrence_id.is_some() || parsed.recurrence_id_date.is_some() {
                overrides.push(parsed);
            } else {
                masters.push(parsed);
            }
        }
        if masters.len() != 1 {
            return Err(IcsError::Parse(
                "a VTODO resource needs exactly one master component".into(),
            ));
        }
        let master = masters.swap_remove(0);
        return Ok(ParsedResource::Todos(Box::new(ParsedTodoSeries {
            master,
            overrides,
            timezones: parsed.timezones,
        })));
    }
    let [journal] = journals.as_slice() else {
        return Err(IcsError::Parse(
            "a VJOURNAL resource holds exactly one component".into(),
        ));
    };
    let custom = compiled_zones(&parsed.timezones);
    Ok(ParsedResource::Journal(Box::new(journal::parse_journal(
        journal, &custom,
    )?)))
}

/// One PUT resource's parsed content.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // the boxed series already carries the big arm
pub enum ParsedResource {
    Events(ParsedCalendar),
    Todos(Box<ParsedTodoSeries>),
    Journal(Box<ParsedJournal>),
}

/// Compiles one VTIMEZONE's rules into a resolvable zone for wall-clock
/// rendering. Failing zones are skipped — they were rejected at PUT, so this
/// only guards against stale stored data.
pub(crate) fn compiled_zone(
    tzid: &str,
    rules: &[calendar_core::recurrence::ZoneRule],
) -> Option<calendar_core::recurrence::Zone> {
    let now = Utc::now();
    let window = (
        now - chrono::Duration::days(366 * calendar_core::recurrence::ZONE_WINDOW_YEARS),
        now + chrono::Duration::days(366 * calendar_core::recurrence::ZONE_WINDOW_YEARS),
    );
    let transitions = calendar_core::recurrence::compile_zone(rules, window.0, window.1).ok()?;
    let mut resolver = calendar_core::recurrence::TzResolver::default();
    resolver.insert(tzid.to_string(), transitions);
    calendar_core::recurrence::resolve_tz(Some(tzid), Some(&resolver)).ok()
}

/// Compiles parsed VTIMEZONEs into tzid → zone lookups.
pub(crate) fn compiled_zones(
    timezones: &[ParsedTimezone],
) -> std::collections::HashMap<String, calendar_core::recurrence::Zone> {
    timezones
        .iter()
        .filter_map(|tz| compiled_zone(&tz.tzid, &tz.rules).map(|zone| (tz.tzid.clone(), zone)))
        .collect()
}

/// Parses one VCALENDAR into its VEVENTs. Non-VEVENT components (VTODO,
/// VJOURNAL) are ignored — those kinds go through [`parse_resource`].
/// VTIMEZONEs are ignored here — callers
/// that store them use [`parse_calendar`].
pub fn parse_ics(text: &str) -> Result<Vec<ParsedEvent>, IcsError> {
    parse_calendar(text).map(|c| c.events)
}

/// Extracts every VTIMEZONE. tzdb-named zones are dropped (tzdb keeps
/// precedence); every other zone must compile over the ADR-012 window.
fn parse_timezones(
    unfolded: &str,
    calendar: &icalendar::parser::Calendar<'_>,
) -> Result<Vec<ParsedTimezone>, IcsError> {
    let now = Utc::now();
    let window = (
        now - chrono::Duration::days(366 * calendar_core::recurrence::ZONE_WINDOW_YEARS),
        now + chrono::Duration::days(366 * calendar_core::recurrence::ZONE_WINDOW_YEARS),
    );
    let mut out = Vec::new();
    for component in calendar
        .components
        .iter()
        .filter(|c| c.name.as_ref() == "VTIMEZONE")
    {
        let parsed = parse_timezone(component)?;
        // IANA-identified zones need no stored definition.
        if calendar_core::recurrence::is_tzdb_tzid(&parsed.tzid) {
            continue;
        }
        calendar_core::recurrence::compile_zone(&parsed.rules, window.0, window.1)
            .map_err(|e| IcsError::UnsupportedTimezone(parsed.tzid.clone(), e.to_string()))?;
        out.push(parsed);
    }
    // Raw definition text is captured from the unfolded input, not
    // re-serialized: the client's zone round-trips byte-faithfully.
    let raw = raw_vtimezones(unfolded);
    Ok(out
        .into_iter()
        .map(|parsed| ParsedTimezone {
            definition: raw
                .iter()
                .find(|text| text.contains(&format!("TZID:{}", parsed.tzid)))
                .cloned()
                .unwrap_or_default(),
            ..parsed
        })
        .collect())
}

/// The raw BEGIN:VTIMEZONE..END:VTIMEZONE blocks (unfolded, one per zone).
fn raw_vtimezones(unfolded: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    for line in unfolded.lines() {
        match line.trim() {
            "BEGIN:VTIMEZONE" => current = Some(format!("{line}\n")),
            "END:VTIMEZONE" => {
                if let Some(mut block) = current.take() {
                    block.push_str(line);
                    out.push(block);
                }
            }
            _ => {
                if let Some(block) = &mut current {
                    block.push_str(line);
                    block.push('\n');
                }
            }
        }
    }
    out
}

/// Parses one VTIMEZONE component into tzid + compiled rules.
fn parse_timezone(
    component: &icalendar::parser::Component<'_>,
) -> Result<ParsedTimezone, IcsError> {
    let tzid = component
        .properties
        .iter()
        .find(|p| p.name.as_ref() == "TZID")
        .map(|p| p.val.as_str().trim().to_string())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            IcsError::UnsupportedTimezone(
                "(missing TZID)".into(),
                "VTIMEZONE without a TZID property".into(),
            )
        })?;
    let mut rules = Vec::new();
    for sub in &component.components {
        let sub_name = sub.name.as_ref();
        if sub_name != "STANDARD" && sub_name != "DAYLIGHT" {
            return Err(IcsError::UnsupportedTimezone(
                tzid.clone(),
                format!("unsupported sub-component {sub_name}"),
            ));
        }
        let get = |key: &str| {
            sub.properties
                .iter()
                .find(|p| p.name.as_ref() == key)
                .map(|p| p.val.as_str().trim().to_string())
        };
        let unsupported =
            |what: &str| IcsError::UnsupportedTimezone(tzid.clone(), what.to_string());
        let dtstart = get("DTSTART")
            .filter(|v| !v.is_empty())
            .and_then(|v| chrono::NaiveDateTime::parse_from_str(&v, "%Y%m%dT%H%M%S").ok())
            .ok_or_else(|| unsupported("DTSTART is not a local date-time"))?;
        let offset_from_secs = parse_utc_offset(
            &get("TZOFFSETFROM")
                .filter(|v| !v.is_empty())
                .ok_or_else(|| unsupported("STANDARD/DAYLIGHT without TZOFFSETFROM"))?,
        )?;
        let offset_to_secs = parse_utc_offset(
            &get("TZOFFSETTO")
                .filter(|v| !v.is_empty())
                .ok_or_else(|| unsupported("STANDARD/DAYLIGHT without TZOFFSETTO"))?,
        )?;
        let rrule = get("RRULE").filter(|v| !v.is_empty());
        let mut rdates = Vec::new();
        if let Some(rdate) = get("RDATE") {
            for value in rdate.split(',') {
                rdates.push(
                    chrono::NaiveDateTime::parse_from_str(value.trim(), "%Y%m%dT%H%M%S")
                        .map_err(|_| unsupported("RDATE is not a local date-time"))?,
                );
            }
        }
        rules.push(calendar_core::recurrence::ZoneRule {
            dtstart,
            offset_from_secs,
            offset_to_secs,
            rrule,
            rdates,
        });
    }
    if rules.is_empty() {
        return Err(IcsError::UnsupportedTimezone(
            tzid.clone(),
            "no STANDARD/DAYLIGHT sub-components".into(),
        ));
    }
    Ok(ParsedTimezone {
        tzid,
        definition: String::new(),
        rules,
    })
}

/// Parses a ±HHMM or ±HHMMSS UTC offset into seconds.
fn parse_utc_offset(value: &str) -> Result<i32, IcsError> {
    let (sign, digits) = value.split_at(1);
    let sign: i32 = match sign {
        "+" => 1,
        "-" => -1,
        _ => return Err(IcsError::Parse(format!("bad TZOFFSET {value}"))),
    };
    let h: i32 = digits
        .get(0..2)
        .and_then(|d| d.parse().ok())
        .ok_or_else(|| IcsError::Parse(format!("bad TZOFFSET {value}")))?;
    let m: i32 = digits.get(2..4).and_then(|d| d.parse().ok()).unwrap_or(0);
    let s: i32 = digits.get(4..6).and_then(|d| d.parse().ok()).unwrap_or(0);
    Ok(sign * (h * 3600 + m * 60 + s))
}

/// Converts a borrowed parser component into an owned `Event`, collecting
/// nested VALARM components (the 0.17 crate parser is never called directly —
/// unfolding happens first).
fn to_owned_event(component: &icalendar::parser::Component<'_>) -> icalendar::Event {
    let mut event = icalendar::Event::new();
    for prop in &component.properties {
        event.append_property(prop.clone());
    }
    event
}

fn parse_event(
    event: icalendar::Event,
    custom: &std::collections::HashMap<String, calendar_core::recurrence::Zone>,
) -> Result<ParsedEvent, IcsError> {
    use icalendar::Component;
    let uid = event.get_uid().ok_or(IcsError::MissingUid)?.to_string();
    let mut parsed = ParsedEvent {
        uid,
        summary: event.get_summary().map(|s| s.to_string()),
        description_text: event.get_description().map(|s| s.to_string()),
        url: event.get_url().map(|s| s.to_string()),
        location_text: event.property_value("LOCATION").map(|s| s.to_string()),
        class: event.property_value("CLASS").map(|s| s.to_string()),
        sequence: event.get_sequence().map(|s| s as i32),
        status: event.property_value("STATUS").map(|s| s.to_string()),
        transp: event.property_value("TRANSP").map(|s| s.to_string()),
        ..Default::default()
    };
    let Some(points) = event.get_start() else {
        return Err(IcsError::MissingDtstart);
    };
    // Preserve the DTSTART zone identity for round-trips (PRD data rules).
    if let DatePerhapsTime::DateTime(icalendar::CalendarDateTime::WithTimezone { tzid, .. }) =
        &points
    {
        parsed.tzid = Some(tzid.clone());
    }
    parsed.floating = matches!(
        points,
        DatePerhapsTime::DateTime(icalendar::CalendarDateTime::Floating(_))
    );
    let Some(point) = points_to_core(&points, custom) else {
        return Err(IcsError::MissingDtstart);
    };
    apply_date_point(&mut parsed, &point, true);
    if let Some(end) = event.get_end()
        && let Some(point) = points_to_core(&end, custom)
    {
        apply_date_point(&mut parsed, &point, false);
    }
    if let Some(prop) = event.properties().get("DURATION")
        && let Some(secs) = parse_ics_duration(prop.value())
    {
        parsed.duration_secs = Some(secs);
    }
    if let Some(recurrence) = event.get_recurrence_id() {
        // RECURRENCE-ID is stored as wall-clock in the event's zone.
        match recurrence {
            DatePerhapsTime::Date(date) => parsed.recurrence_id_date = Some(date),
            DatePerhapsTime::DateTime(icalendar::CalendarDateTime::WithTimezone {
                date_time,
                tzid,
            }) => {
                parsed.recurrence_id = Some(date_time);
                parsed.tzid.get_or_insert(tzid);
            }
            DatePerhapsTime::DateTime(icalendar::CalendarDateTime::Floating(naive)) => {
                parsed.recurrence_id = Some(naive);
            }
            DatePerhapsTime::DateTime(icalendar::CalendarDateTime::Utc(at)) => {
                parsed.recurrence_id = Some(at.naive_utc());
            }
        }
    }
    parsed.rrule = event.property_value("RRULE").map(|s| s.to_string());
    for key in ["RDATE", "EXDATE"] {
        let mut props: Vec<icalendar::Property> = Vec::new();
        if let Some(prop) = event.properties().get(key) {
            props.push(prop.clone());
        }
        if let Some(multi) = event.multi_properties().get(key) {
            props.extend(multi.iter().cloned());
        }
        let points = parse_recurrence_points(props, custom);
        if key == "RDATE" {
            parsed.rdate = points;
        } else {
            parsed.exdate = points;
        }
    }
    parsed.categories = collect_categories(&event);
    if let Some(prop) = event.properties().get("ATTENDEE") {
        parsed.attendees.push(attendee_from_prop(prop));
    }
    if let Some(props) = event.multi_properties().get("ATTENDEE") {
        for prop in props {
            parsed.attendees.push(attendee_from_prop(prop));
        }
    }

    if let Some(prop) = event.properties().get("ORGANIZER").or_else(|| {
        event
            .multi_properties()
            .get("ORGANIZER")
            .and_then(|p| p.first())
    }) {
        let mailto = prop.value();
        parsed.organizer_email = Some(
            mailto
                .split_once(':')
                .map(|(_, rest)| rest)
                .unwrap_or(mailto)
                .to_string(),
        );
        parsed.organizer_name = prop.params().get("CN").map(|p| p.value().to_string());
    }
    parsed.description_html = event.property_value("X-ALT-DESC").map(|s| s.to_string());
    parsed.priority = event.get_priority().map(|p| p as i16);
    Ok(parsed)
}

/// VALARM subset: ACTION, TRIGGER, RELATED, DESCRIPTION, SUMMARY, ATTENDEE.
/// Shared by the event and task parse paths.
pub(crate) fn parse_alarm(component: &icalendar::parser::Component<'_>) -> ParsedAlarm {
    let mut parsed = ParsedAlarm::default();
    for prop in &component.properties {
        match prop.name.as_ref() {
            "ACTION" => parsed.action = prop.val.as_str().to_string(),
            "RELATED" => parsed.related = Some(prop.val.as_str().to_string()),
            "DESCRIPTION" => parsed.description = Some(prop.val.as_str().to_string()),
            "SUMMARY" => parsed.summary = Some(prop.val.as_str().to_string()),
            "TRIGGER" => {
                let value = prop.val.as_str();
                if value.contains('P') {
                    if let Some(secs) = parse_ics_duration(value) {
                        parsed.offset_secs = Some(secs);
                    }
                    // ponytail: RELATED=END semantics land with per-occurrence
                    // end triggers if a client needs them; START-relative is
                    // the only path real clients send today.
                } else if value.len() == 16
                    && let Ok(naive) =
                        chrono::NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ")
                {
                    parsed.trigger_at = Some(Utc.from_utc_datetime(&naive));
                }
            }
            "ATTENDEE" => {
                let mailto = prop.val.as_str();
                parsed.recipients.push(
                    mailto
                        .split_once(':')
                        .map(|(_, rest)| rest)
                        .unwrap_or(mailto)
                        .to_string(),
                );
            }
            _ => {}
        }
    }
    parsed
}

/// ISO 8601 duration subset: [+-]P[nW][nD][T[nH][nM][nS]] → seconds.
pub(crate) fn parse_ics_duration(value: &str) -> Option<i64> {
    let (negative, value) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value.trim_start_matches('+')),
    };
    let value = value.strip_prefix('P')?;
    let mut seconds: i64 = 0;
    let mut number = String::new();
    let mut in_time = false;
    for ch in value.chars() {
        match ch {
            'T' => in_time = true,
            'W' => {
                seconds += number.parse::<i64>().ok()? * 604_800;
                number.clear();
            }
            'D' => {
                seconds += number.parse::<i64>().ok()? * 86_400;
                number.clear();
            }
            'H' if in_time => {
                seconds += number.parse::<i64>().ok()? * 3600;
                number.clear();
            }
            'M' if in_time => {
                seconds += number.parse::<i64>().ok()? * 60;
                number.clear();
            }
            'S' if in_time => {
                seconds += number.parse::<i64>().ok()?;
                number.clear();
            }
            '0'..='9' => number.push(ch),
            _ => return None,
        }
    }
    if !number.is_empty() {
        return None; // trailing digits without a unit
    }
    Some(if negative { -seconds } else { seconds })
}

fn parse_ics_datetime(value: &str) -> Option<DateTime<Utc>> {
    if let Some(utc) = value.strip_suffix('Z') {
        return NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S")
            .ok()
            .map(|naive| Utc.from_utc_datetime(&naive));
    }
    // Floating local time is interpreted as UTC by this server when no TZID
    // parameter reached the value; the TZID parameter is handled by callers
    // through DatePerhapsTime when available.
    NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")
        .ok()
        .map(|naive| Utc.from_utc_datetime(&naive))
}

/// RDATE/EXDATE values to points. TZID-qualified values are converted
/// through the zone so the stored point is an absolute instant regardless of
/// form. A custom tzid resolves through the calendar's VTIMEZONEs; an
/// unresolvable tzid leaves the value as a UTC instant rather than guessing —
/// put_series rejects the unknown tzid.
pub(crate) fn parse_recurrence_points(
    props: Vec<icalendar::Property>,
    custom: &Zones,
) -> Vec<DateOrDateTime> {
    let mut out = Vec::new();
    for prop in props {
        for value in prop.value().split(',') {
            if value.is_empty() {
                continue;
            }
            let tz_param = prop.params().get("TZID").map(|p| p.value().to_string());
            let zone: Option<calendar_core::recurrence::Zone> =
                tz_param.as_deref().and_then(|t| match custom.get(t) {
                    Some(zone) => Some(zone.clone()),
                    None => t
                        .parse::<Tz>()
                        .ok()
                        .map(calendar_core::recurrence::Zone::Tz),
                });
            let point = if value.len() == 8 && value.chars().all(|c| c.is_ascii_digit()) {
                NaiveDate::parse_from_str(value, "%Y%m%d")
                    .ok()
                    .map(DateOrDateTime::AllDay)
            } else if let Some(zone) = zone {
                NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")
                    .ok()
                    .and_then(|naive| zone.from_local(naive))
                    .map(DateOrDateTime::Timed)
            } else {
                parse_ics_datetime(value).map(DateOrDateTime::Timed)
            };
            out.extend(point);
        }
    }
    out
}

/// One date-or-date-time property (DTSTART/DUE) as it sat on the wire,
/// resolved to the storage shapes shared by tasks and journals: `date` for
/// VALUE=DATE, `at` otherwise, with `floating` marking a bare wall clock
/// (stored as if UTC) and `tzid` keeping the client's zone identity.
#[derive(Debug, Default, Clone)]
pub(crate) struct WirePoint {
    pub date: Option<NaiveDate>,
    pub at: Option<DateTime<Utc>>,
    pub tzid: Option<String>,
    pub floating: bool,
}

/// Parses DTSTART/DUE/COMPLETED-style values: VALUE=DATE (or 8 digits),
/// UTC (`Z`), TZID-qualified local time (custom zones resolve through the
/// calendar's VTIMEZONEs), or floating (no zone marker).
pub(crate) fn parse_wire_point(prop: &icalendar::Property, custom: &Zones) -> Option<WirePoint> {
    let value = prop.value().trim();
    if value.len() == 8 && value.chars().all(|c| c.is_ascii_digit()) {
        return NaiveDate::parse_from_str(value, "%Y%m%d")
            .ok()
            .map(|date| WirePoint {
                date: Some(date),
                ..Default::default()
            });
    }
    if let Some(utc) = value.strip_suffix('Z') {
        let naive = NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S").ok()?;
        return Some(WirePoint {
            at: Some(Utc.from_utc_datetime(&naive)),
            ..Default::default()
        });
    }
    let naive = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()?;
    match prop.params().get("TZID").map(|p| p.value().to_string()) {
        Some(tzid) => {
            let zone = custom.get(&tzid).cloned().or_else(|| {
                tzid.parse::<Tz>()
                    .ok()
                    .map(calendar_core::recurrence::Zone::Tz)
            });
            Some(WirePoint {
                at: zone.and_then(|zone| zone.from_local(naive)),
                tzid: Some(tzid),
                floating: false,
                date: None,
            })
        }
        // Floating: the wall clock is stored as if it were UTC.
        None => Some(WirePoint {
            at: Some(naive.and_utc()),
            floating: true,
            tzid: None,
            date: None,
        }),
    }
}

/// CATEGORIES across the single and multi property forms, split on commas.
pub(crate) fn collect_categories<C: icalendar::Component>(component: &C) -> Vec<String> {
    let mut raw: Vec<String> = Vec::new();
    if let Some(value) = component.property_value("CATEGORIES") {
        raw.push(value.to_string());
    }
    if let Some(props) = component.multi_properties().get("CATEGORIES") {
        for prop in props {
            raw.push(prop.value().to_string());
        }
    }
    let mut out: Vec<String> = Vec::new();
    for joined in raw {
        for category in joined.split(',') {
            let category = category.trim();
            if !category.is_empty() && !out.iter().any(|c| c == category) {
                out.push(category.to_string());
            }
        }
    }
    out
}

pub(crate) fn attendee_from_prop(prop: &icalendar::Property) -> ParsedAttendee {
    // CAL-ADDRESS is a URI: mailto: for email attendees, sms: for SMS-only.
    let value = prop.value();
    let (scheme, rest) = match value.split_once(':') {
        Some((scheme, rest)) if scheme == "mailto" || scheme == "sms" => (scheme, rest),
        // Bare values and unknown schemes keep the legacy email interpretation.
        _ => ("mailto", value),
    };
    if scheme == "sms" {
        ParsedAttendee {
            email: None,
            telephone: Some(rest.to_string()),
            display_name: prop.params().get("CN").map(|p| p.value().to_string()),
            role: prop.params().get("ROLE").map(|p| p.value().to_string()),
            partstat: prop.params().get("PARTSTAT").map(|p| p.value().to_string()),
            rsvp: prop
                .params()
                .get("RSVP")
                .map(|p| p.value().eq_ignore_ascii_case("true")),
        }
    } else {
        ParsedAttendee {
            email: Some(rest.to_string()),
            telephone: None,
            display_name: prop.params().get("CN").map(|p| p.value().to_string()),
            role: prop.params().get("ROLE").map(|p| p.value().to_string()),
            partstat: prop.params().get("PARTSTAT").map(|p| p.value().to_string()),
            rsvp: prop
                .params()
                .get("RSVP")
                .map(|p| p.value().eq_ignore_ascii_case("true")),
        }
    }
}

fn points_to_core(
    point: &DatePerhapsTime,
    custom: &std::collections::HashMap<String, calendar_core::recurrence::Zone>,
) -> Option<DateOrDateTime> {
    match point {
        DatePerhapsTime::Date(date) => Some(DateOrDateTime::AllDay(*date)),
        // Floating: the wall clock is stored as if it were UTC (events.floating).
        DatePerhapsTime::DateTime(icalendar::CalendarDateTime::Floating(naive)) => {
            Some(DateOrDateTime::Timed(naive.and_utc()))
        }
        DatePerhapsTime::DateTime(dt) => {
            // Custom tzid: the calendar's own VTIMEZONEs supply the conversion.
            if let icalendar::CalendarDateTime::WithTimezone { date_time, tzid } = dt
                && let Some(zone) = custom.get(tzid.as_ref() as &str)
            {
                return zone.from_local(*date_time).map(DateOrDateTime::Timed);
            }
            dt.try_into_utc().map(DateOrDateTime::Timed)
        }
    }
}

fn apply_date_point(parsed: &mut ParsedEvent, point: &DateOrDateTime, start: bool) {
    match point {
        DateOrDateTime::AllDay(date) => {
            if start {
                parsed.start_date = Some(*date);
                parsed.all_day = true;
            } else {
                parsed.end_date = Some(*date);
            }
        }
        DateOrDateTime::Timed(at) => {
            if start {
                parsed.starts_at = Some(*at);
            } else {
                parsed.ends_at = Some(*at);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Timelike};

    const SAMPLE: &str = "BEGIN:VCALENDAR\r\n\
PRODID:-//calendar-server//EN\r\nVERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:test-1\r\n\
DTSTAMP:20260911T120000Z\r\n\
DTSTART;TZID=America/Denver:20260105T090000\r\n\
DTEND;TZID=America/Denver:20260105T100000\r\n\
SUMMARY:Standup\r\n\
DESCRIPTION:plain text\r\n\
LOCATION:Union Station\r\n\
RRULE:FREQ=WEEKLY;BYDAY=MO\r\n\
EXDATE;TZID=America/Denver:20260112T090000\r\n\
CLASS:PRIVATE\r\n\
TRANSP:TRANSPARENT\r\n\
STATUS:CONFIRMED\r\n\
PRIORITY:5\r\n\
CATEGORIES:work,team\r\n\
ORGANIZER;CN=Brian:mailto:brian@example.com\r\n\
ATTENDEE;CN=Al;PARTSTAT=ACCEPTED;ROLE=REQ-PARTICIPANT;RSVP=TRUE:mailto:al@example.com\r\n\
SEQUENCE:2\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn calendar_method_is_captured_and_absent_is_none() {
        let ics = "BEGIN:VCALENDAR\r\n\
METHOD:CANCEL\r\n\
BEGIN:VEVENT\r\n\
UID:m-1\r\n\
DTSTART:20260301T100000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let events = parse_ics(ics).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].method.as_deref(), Some("CANCEL"));
        // SAMPLE has no METHOD property.
        assert_eq!(parse_ics(SAMPLE).unwrap()[0].method, None);
    }

    #[test]
    fn parse_round_trip_fields() {
        let events = parse_ics(SAMPLE).unwrap();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.uid, "test-1");
        assert_eq!(ev.summary.as_deref(), Some("Standup"));
        assert_eq!(ev.location_text.as_deref(), Some("Union Station"));
        assert_eq!(ev.status.as_deref(), Some("CONFIRMED"));
        assert_eq!(ev.class.as_deref(), Some("PRIVATE"));
        assert_eq!(ev.transp.as_deref(), Some("TRANSPARENT"));
        assert_eq!(ev.priority, Some(5));
        assert_eq!(ev.categories, vec!["work", "team"]);
        assert_eq!(ev.organizer_email.as_deref(), Some("brian@example.com"));
        assert_eq!(ev.organizer_name.as_deref(), Some("Brian"));
        assert_eq!(ev.attendees.len(), 1);
        assert_eq!(ev.attendees[0].email.as_deref(), Some("al@example.com"));
        assert_eq!(ev.attendees[0].partstat.as_deref(), Some("ACCEPTED"));
        assert_eq!(ev.attendees[0].rsvp, Some(true));
        assert_eq!(ev.rrule.as_deref(), Some("FREQ=WEEKLY;BYDAY=MO"));
        assert_eq!(ev.exdate.len(), 1);
        assert_eq!(ev.sequence, Some(2));
    }

    #[test]
    fn sms_attendee_round_trips() {
        let mut attendee = sample_attendee();
        attendee.email = None;
        attendee.telephone = Some("+13216166280".into());
        let ics = events_to_ics(&[ExportRow {
            vtimezones: vec![],
            event: sample_event_row(),
            attendees: vec![attendee],
            alarms: vec![],
            location: None,
        }]);
        // The sms: value may be RFC-folded across lines; match on the prefix.
        assert!(
            ics.contains(":sms:+"),
            "expected sms CAL-ADDRESS in:\n{}",
            ics
        );
        assert!(!ics.contains("mailto:al@example.com"));
        let parsed = parse_ics(&ics).unwrap();
        assert_eq!(parsed[0].attendees.len(), 1);
        assert_eq!(parsed[0].attendees[0].email, None);
        assert_eq!(
            parsed[0].attendees[0].telephone.as_deref(),
            Some("+13216166280")
        );
    }

    #[test]
    fn alarm_channels_map_to_wire_action() {
        let mut event = sample_event_row();
        event.starts_at = Some(Utc::now());
        let alarm = |channels: &[&str]| calendar_db::alarms::AlarmRow {
            id: uuid::Uuid::new_v4(),
            event_id: event.id,
            action: "DISPLAY".into(),
            related: Some("START".into()),
            offset_interval: Some(sqlx::postgres::types::PgInterval {
                months: 0,
                days: 0,
                microseconds: -900 * 1_000_000,
            }),
            trigger_at: None,
            description: None,
            summary: Some("R".into()),
            recipient_emails: vec![],
            notify_channels: channels.iter().map(|c| c.to_string()).collect(),
            created_at: Utc::now(),
        };
        let ics = events_to_ics(&[ExportRow {
            vtimezones: vec![],
            event: event.clone(),
            attendees: vec![],
            alarms: vec![
                alarm(&["in_app", "email", "sms"]),
                alarm(&["in_app", "sms"]),
            ],
            location: None,
        }]);
        assert_eq!(ics.matches("ACTION:EMAIL").count(), 1);
        assert_eq!(ics.matches("ACTION:DISPLAY").count(), 1);
        assert!(!ics.contains("ACTION:SMS") && !ics.contains("ACTION:PUSH"));
        // Import maps wire ACTION back to app channels.
        let parsed = parse_ics(&ics).unwrap();
        assert_eq!(parsed[0].alarms.len(), 2);
        assert!(parsed[0].alarms[0].action.eq_ignore_ascii_case("EMAIL"));
    }

    fn sample_location() -> calendar_db::LocationRow {
        calendar_db::LocationRow {
            id: uuid::Uuid::new_v4(),
            provider: Some("google_places".into()),
            provider_place_id: Some("place-1".into()),
            display_name: Some("Union Station".into()),
            formatted_address: Some("1701 Wynkoop St, Denver, CO".into()),
            street_address: None,
            locality: None,
            administrative_area: None,
            postal_code: None,
            country: None,
            latitude: Some(39.7534),
            longitude: Some(-105.0016),
            website: None,
            phone: None,
            provider_metadata: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn location_appears_in_ics() {
        let event = sample_event_row();
        let ics = events_to_ics(&[ExportRow {
            vtimezones: vec![],
            event,
            attendees: vec![],
            alarms: vec![],
            location: Some(sample_location()),
        }]);
        assert!(ics.contains("LOCATION:Union Station"));
        assert!(ics.contains("GEO:39.7534;-105.0016"));
    }

    #[test]
    fn floating_times_round_trip_without_zone() {
        let ics = "BEGIN:VCALENDAR\r\nPRODID:-//x//EN\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\n\
UID:float-1\r\nDTSTAMP:20260911T120000Z\r\nDTSTART:20260915T090000\r\n\
DTEND:20260915T100000\r\nRRULE:FREQ=DAILY;COUNT=3\r\nEXDATE:20260916T090000\r\n\
SUMMARY:Floating\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:float-1\r\nDTSTAMP:20260911T120000Z\r\n\
RECURRENCE-ID:20260917T090000\r\nDTSTART:20260917T110000\r\nDTEND:20260917T120000\r\n\
SUMMARY:Moved\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let parsed = parse_ics(ics).unwrap();
        assert_eq!(parsed.len(), 2);
        assert!(parsed.iter().all(|e| e.floating && e.tzid.is_none()));
        let rows: Vec<ExportRow> = parsed
            .iter()
            .map(|p| {
                let mut row = sample_event_row();
                row.uid = p.uid.clone();
                row.tzid = None;
                row.floating = p.floating;
                row.starts_at = p.starts_at;
                row.ends_at = p.ends_at;
                row.rrule = p.rrule.clone();
                row.recurrence_id = p.recurrence_id;
                row.master_event_id = p.recurrence_id.map(|_| uuid::Uuid::new_v4());
                row.rdate = serde_json::json!([]);
                row.exdate = serde_json::json!(
                    p.exdate
                        .iter()
                        .map(|d| match d {
                            DateOrDateTime::Timed(at) => at.to_rfc3339(),
                            DateOrDateTime::AllDay(d) => d.to_string(),
                        })
                        .collect::<Vec<_>>()
                );
                ExportRow::from((row, vec![]))
            })
            .collect();
        let out = events_to_ics(&rows);
        assert!(out.contains("DTSTART:20260915T090000\r\n"), "{out}");
        assert!(out.contains("DTEND:20260915T100000\r\n"), "{out}");
        assert!(out.contains("EXDATE:20260916T090000\r\n"), "{out}");
        assert!(out.contains("RECURRENCE-ID:20260917T090000\r\n"), "{out}");
        assert!(!out.contains("TZID"), "{out}");
    }

    #[test]
    fn vtodo_is_ignored_by_parse_ics_and_parsed_by_parse_resource() {
        let todo = "BEGIN:VCALENDAR\r\nPRODID:-//x//EN\r\nVERSION:2.0\r\n\
BEGIN:VTODO\r\nUID:t1\r\nDTSTAMP:20260911T120000Z\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        // parse_ics keeps its event-only contract (free-busy, iMIP readers).
        assert!(parse_ics(todo).unwrap().is_empty());
        // The PUT path dispatches on the parsed kind instead of rejecting.
        assert!(matches!(parse_resource(todo), Ok(ParsedResource::Todos(_))));
    }

    fn sample_event_row() -> calendar_db::EventRow {
        let now = Utc::now();
        calendar_db::EventRow {
            id: uuid::Uuid::new_v4(),
            calendar_id: uuid::Uuid::new_v4(),
            uid: "round-trip-uid".into(),
            href: None,
            master_event_id: None,
            recurrence_id: None,
            recurrence_id_date: None,
            is_exception: false,
            starts_at: Some(Utc.with_ymd_and_hms(2026, 1, 5, 16, 0, 0).unwrap()),
            ends_at: Some(Utc.with_ymd_and_hms(2026, 1, 5, 17, 0, 0).unwrap()),
            start_date: None,
            end_date: None,
            duration: None,
            tzid: Some("America/Denver".into()),
            all_day: false,
            floating: false,
            rrule: Some("FREQ=DAILY;COUNT=3".into()),
            rdate: serde_json::json!([]),
            exdate: serde_json::json!([]),
            summary: "Round trip".into(),
            description_html: None,
            description_text: Some("text".into()),
            url: None,
            status: Some("CONFIRMED".into()),
            priority: Some(5),
            class: Some("PUBLIC".into()),
            transp: Some("OPAQUE".into()),
            categories: vec!["a".into(), "b".into()],
            location_id: None,
            organizer_user_id: None,
            organizer_email: "brian@example.com".into(),
            organizer_name: Some("Brian".into()),
            sequence: 0,
            etag: String::new(),
            created_by: None,
            deleted_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn sample_attendee() -> calendar_db::AttendeeRow {
        let now = Utc::now();
        calendar_db::AttendeeRow {
            id: uuid::Uuid::new_v4(),
            event_id: uuid::Uuid::new_v4(),
            user_id: None,
            contact_id: None,
            email: Some("al@example.com".into()),
            display_name: Some("Al".into()),
            telephone: None,
            role: "REQ-PARTICIPANT".into(),
            partstat: "ACCEPTED".into(),
            rsvp: Some(true),
            schedule_status: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn serialize_then_parse_round_trip() {
        let event = sample_event_row();
        let attendees = vec![sample_attendee()];
        let ics = events_to_ics(&[ExportRow {
            vtimezones: vec![],
            event,
            attendees,
            alarms: vec![],
            location: None,
        }]);
        eprintln!("GENERATED ICS:\n{}<<END>>", ics);
        let parsed = parse_ics(&ics).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].uid, "round-trip-uid");
        assert_eq!(parsed[0].summary.as_deref(), Some("Round trip"));
        assert_eq!(parsed[0].rrule.as_deref(), Some("FREQ=DAILY;COUNT=3"));
        assert_eq!(parsed[0].categories, vec!["a", "b"]);
        assert_eq!(parsed[0].attendees.len(), 1);
        assert_eq!(
            parsed[0].attendees[0].email.as_deref(),
            Some("al@example.com")
        );
        assert_eq!(
            parsed[0].organizer_email.as_deref(),
            Some("brian@example.com")
        );
        assert!(parsed[0].starts_at.is_some());
    }

    #[test]
    fn exception_row_gets_recurrence_id() {
        let mut event = sample_event_row();
        event.master_event_id = Some(uuid::Uuid::new_v4());
        event.recurrence_id = Some(
            NaiveDateTime::parse_from_str("2026-01-12T09:00:00", "%Y-%m-%dT%H:%M:%S").unwrap(),
        );
        event.summary = "Moved".into();
        let ics = events_to_ics(&[ExportRow {
            vtimezones: vec![],
            event,
            attendees: vec![],
            alarms: vec![],
            location: None,
        }]);
        assert!(ics.contains("RECURRENCE-ID"));
        let parsed = parse_ics(&ics).unwrap();
        assert_eq!(
            parsed[0].recurrence_id,
            Some(
                NaiveDateTime::parse_from_str("2026-01-12T09:00:00", "%Y-%m-%dT%H:%M:%S").unwrap()
            )
        );
    }
    const ZONE_CALENDAR: &str = "BEGIN:VCALENDAR\r\n\
PRODID:-//calendar-server//EN\r\nVERSION:2.0\r\n\
BEGIN:VTIMEZONE\r\nTZID:Custom/Test\r\n\
BEGIN:STANDARD\r\nDTSTART:19701101T020000\r\n\
TZOFFSETFROM:-0600\r\nTZOFFSETTO:-0700\r\n\
RRULE:FREQ=YEARLY;BYMONTH=11;BYDAY=1SU\r\nEND:STANDARD\r\n\
BEGIN:DAYLIGHT\r\nDTSTART:19700308T020000\r\n\
TZOFFSETFROM:-0700\r\nTZOFFSETTO:-0600\r\n\
RRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=2SU\r\nEND:DAYLIGHT\r\n\
END:VTIMEZONE\r\n\
BEGIN:VEVENT\r\nUID:zone-1\r\nDTSTAMP:20260911T120000Z\r\n\
DTSTART;TZID=Custom/Test:20260305T090000\r\n\
DTEND;TZID=Custom/Test:20260305T100000\r\n\
RRULE:FREQ=DAILY\r\nSUMMARY:Zone standup\r\n\
ORGANIZER:mailto:brian@example.com\r\nEND:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn vtimezone_rules_are_extracted() {
        let parsed = parse_calendar(ZONE_CALENDAR).unwrap();
        assert_eq!(parsed.events.len(), 1);
        assert_eq!(parsed.timezones.len(), 1);
        let tz = &parsed.timezones[0];
        assert_eq!(tz.tzid, "Custom/Test");
        assert!(tz.definition.starts_with("BEGIN:VTIMEZONE"));
        assert!(tz.definition.contains("TZID:Custom/Test"));
        assert_eq!(tz.rules.len(), 2);
        let standard = tz
            .rules
            .iter()
            .find(|r| r.offset_to_secs == -7 * 3600)
            .unwrap();
        assert_eq!(standard.offset_from_secs, -6 * 3600);
        assert_eq!(
            standard.rrule.as_deref(),
            Some("FREQ=YEARLY;BYMONTH=11;BYDAY=1SU")
        );
        // The event keeps its custom tzid identity.
        assert_eq!(parsed.events[0].tzid.as_deref(), Some("Custom/Test"));
    }

    #[test]
    fn tzdb_named_vtimezone_is_not_stored() {
        // tzdb zones keep precedence; the definition is dropped, not stored.
        let ics = ZONE_CALENDAR
            .replace("TZID:Custom/Test", "TZID:America/Denver")
            .replace("TZID=Custom/Test", "TZID=America/Denver");
        let parsed = parse_calendar(&ics).unwrap();
        assert!(parsed.timezones.is_empty());
        assert_eq!(parsed.events[0].tzid.as_deref(), Some("America/Denver"));
    }

    #[test]
    fn uncompilable_vtimezone_names_the_tzid() {
        let prefix = "BEGIN:VCALENDAR\r\nPRODID:-//x//EN\r\nVERSION:2.0\r\nBEGIN:VTIMEZONE\r\nTZID:Broken/Zone\r\n";
        let cases = [
            // unsupported FREQ
            format!(
                "{prefix}BEGIN:STANDARD\r\nDTSTART:19701101T020000\r\nTZOFFSETFROM:-0600\r\nTZOFFSETTO:-0700\r\nRRULE:FREQ=MINUTELY\r\nEND:STANDARD\r\n"
            ),
            // TZOFFSETTO missing
            format!(
                "{prefix}BEGIN:STANDARD\r\nDTSTART:19701101T020000\r\nTZOFFSETFROM:-0600\r\nEND:STANDARD\r\n"
            ),
            // no STANDARD/DAYLIGHT at all
            format!("{prefix}END:VTIMEZONE\r\nEND:VCALENDAR\r\n"),
            // unsupported sub-component
            format!(
                "{prefix}BEGIN:STANDARD\r\nDTSTART:19701101T020000\r\nTZOFFSETFROM:-0600\r\nTZOFFSETTO:-0700\r\nEND:STANDARD\r\nBEGIN:X-WEIRD\r\nFOO:bar\r\nEND:X-WEIRD\r\n"
            ),
        ];
        for ics in &cases {
            // The icalendar parser itself rejects a STANDARD/DAYLIGHT missing
            // required properties; anything else must fail with the tzid.
            let err = parse_calendar(ics).unwrap_err();
            assert!(
                matches!(err, IcsError::UnsupportedTimezone(ref tz, _) if tz == "Broken/Zone")
                    || matches!(err, IcsError::Parse(_)),
                "expected UnsupportedTimezone or parse error, got {err}"
            );
        }
        // parse_ics (the VEVENT-only path used by free-busy/iMIP readers)
        // does not reject zones it ignores.
        assert_eq!(parse_ics(ZONE_CALENDAR).unwrap().len(), 1);
    }

    #[test]
    fn folded_vtimezone_unfolds_and_parses() {
        // RFC 5545 folded lines: TZID split across a continuation line.
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTIMEZONE\r\n\
TZID:Custom/Fo\r\n ld\r\nBEGIN:STANDARD\r\nDTSTART:19701101T020000\r\n\
TZOFFSETFROM:-0600\r\nTZOFFSETTO:-0700\r\nEND:STANDARD\r\n\
END:VTIMEZONE\r\nEND:VCALENDAR\r\n";
        let parsed = parse_calendar(ics).unwrap();
        assert_eq!(parsed.timezones.len(), 1);
        assert_eq!(parsed.timezones[0].tzid, "Custom/Fold");
    }

    /// DB-backed tests need a live PostgreSQL via DATABASE_URL (the throwaway
    /// instance the interop suite boots works). Without it they skip so
    /// `cargo test` still passes on machines without infrastructure.
    async fn test_pool() -> Option<sqlx::PgPool> {
        let url = std::env::var("DATABASE_URL")
            .ok()
            .filter(|u| !u.is_empty())?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .ok()?;
        calendar_db::migrate(&pool).await.ok()?;
        Some(pool)
    }

    #[tokio::test]
    async fn vtimezone_round_trips_put_expand_and_get() {
        let Some(pool) = test_pool().await else {
            return;
        };
        // Calendar fixture (users/tenants/calendars), mirroring the ics_upsert tests.
        let user = uuid::Uuid::new_v4();
        let calendar = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, username, email) VALUES ($1, $2, $3)")
            .bind(user)
            .bind(format!("u-{}", user.simple()))
            .bind(format!("{}@zone.test", user))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO tenants (id, slug, name, is_personal) VALUES ($1, $2, $2, true)")
            .bind(user)
            .bind(user.simple().to_string())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO tenant_members (tenant_id, user_id, role) VALUES ($1, $2, 'owner')",
        )
        .bind(user)
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO calendars (id, tenant_id, slug, name, created_by) VALUES ($1, $2, $3, $3, $4)")
            .bind(calendar)
            .bind(user)
            .bind(user.simple().to_string())
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();

        // PUT through the same mapping the DAV flush() path uses.
        let parsed = parse_calendar(ZONE_CALENDAR).unwrap();
        let mut master = upsert_data(&parsed.events[0]);
        if master.organizer_email.is_empty() {
            master.organizer_email = "brian@example.com".into();
        }
        let zones: Vec<calendar_db::timezones::NewTimezone> = parsed
            .timezones
            .iter()
            .map(|tz| calendar_db::timezones::NewTimezone {
                tzid: tz.tzid.clone(),
                definition: tz.definition.clone(),
                rules: tz.rules.clone(),
            })
            .collect();
        let (row, created) = calendar_db::ics_upsert::put_series(
            &pool,
            calendar,
            user,
            "zone.ics",
            &master,
            &[],
            &zones,
            &calendar_db::ics_upsert::PutPrecondition::None,
        )
        .await
        .unwrap();
        assert!(created);
        assert_eq!(row.tzid.as_deref(), Some("Custom/Test"));

        // Expansion through the loaded resolver crosses the DST transition
        // with the wall clock intact.
        let resolver = calendar_db::timezones::load_for_calendar(&pool, calendar)
            .await
            .unwrap();
        let expanded = calendar_core::recurrence::expand_occurrences(
            DateOrDateTime::Timed(row.starts_at.unwrap()),
            row.tzid.as_deref(),
            Some(&resolver),
            row.rrule.as_deref(),
            &[],
            &[],
            chrono::Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap(),
            chrono::Utc.with_ymd_and_hms(2026, 3, 12, 0, 0, 0).unwrap(),
        )
        .unwrap();
        let utc_hours: Vec<(u32, u32)> = expanded
            .iter()
            .map(|p| match p {
                DateOrDateTime::Timed(at) => (at.day(), at.hour()),
                _ => panic!("timed expected"),
            })
            .collect();
        assert_eq!(
            utc_hours,
            vec![
                (5, 16),
                (6, 16),
                (7, 16),
                (8, 15),
                (9, 15),
                (10, 15),
                (11, 15)
            ]
        );

        // GET-render: the client's VTIMEZONE is serialized back and the
        // DTSTART keeps its TZID-qualified wall clock.
        let zones = calendar_db::timezones::list_for_calendar(&pool, calendar)
            .await
            .unwrap();
        let (event, _) = calendar_db::get_event_by_href(&pool, calendar, "zone.ics")
            .await
            .unwrap();
        let out = events_to_ics(&[ExportRow {
            event,
            attendees: vec![],
            alarms: vec![],
            location: None,
            vtimezones: zones,
        }]);
        assert!(out.contains("BEGIN:VTIMEZONE"), "{out}");
        assert!(out.contains("TZID:Custom/Test"), "{out}");
        assert!(
            out.contains("DTSTART;TZID=Custom/Test:20260305T090000"),
            "{out}"
        );
        // The export parses again: zone + event survive the round trip.
        let reparsed = parse_calendar(&out).unwrap();
        assert_eq!(reparsed.timezones[0].tzid, "Custom/Test");
        assert_eq!(reparsed.events[0].tzid.as_deref(), Some("Custom/Test"));
        assert_eq!(reparsed.events[0].starts_at, row.starts_at);
    }
}
