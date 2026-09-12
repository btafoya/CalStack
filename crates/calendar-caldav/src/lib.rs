//! CalDAV adapter: iCalendar parse/serialize between the normalized model
//! (calendar-db rows) and RFC 5545 wire format, the dav-server-rs guarded
//! filesystem adapter, and CalDAV REPORT helpers dav-server lacks.

pub mod adapter;
pub mod store;

pub use adapter::{DavAuth, PgDavFs};
pub(crate) use store::upsert_data;

use calendar_core::DateOrDateTime;
use calendar_db::{AttendeeRow, EventRow};
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use icalendar::{Calendar, Component, DatePerhapsTime, Event, EventLike};

#[derive(Debug, thiserror::Error)]
pub enum IcsError {
    #[error("VTODO is not supported (ADR-011)")]
    TodoUnsupported,
    #[error("parse error: {0}")]
    Parse(String),
    #[error("VEVENT missing UID")]
    MissingUid,
    #[error("VEVENT missing DTSTART")]
    MissingDtstart,
}

// ============ serialization ============

/// RFC 5545 unfolding: a line beginning with space or tab continues the
/// previous line.
fn unfold(text: &str) -> String {
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

/// DTSTART/DTEND/RECURRENCE-ID property: `VALUE=DATE` for all-day, local
/// wall-clock with TZID for known zones, UTC with `Z` otherwise.
fn date_time_property(
    key: &str,
    tzid: Option<&str>,
    all_day: bool,
    at: DateTime<Utc>,
    date: NaiveDate,
) -> icalendar::Property {
    let mut prop = if all_day {
        icalendar::Property::new(key, date.format("%Y%m%d").to_string())
    } else if let Some(tz) = known_tz(tzid) {
        let local = at.with_timezone(&tz).format("%Y%m%dT%H%M%S").to_string();
        icalendar::Property::new(key, local)
    } else {
        icalendar::Property::new(key, at.format("%Y%m%dT%H%M%SZ").to_string())
    };
    if all_day {
        prop.add_parameter("VALUE", "DATE");
    } else if let Some(tz) = known_tz(tzid) {
        prop.add_parameter("TZID", tz.name());
    }
    prop
}

fn recurrence_id_property(event: &EventRow) -> Option<icalendar::Property> {
    if let Some(date) = event.recurrence_id_date {
        let mut prop = icalendar::Property::new("RECURRENCE-ID", date.format("%Y%m%d").to_string());
        prop.add_parameter("VALUE", "DATE");
        Some(prop)
    } else if let Some(naive) = event.recurrence_id {
        let mut prop = match known_tz(event.tzid.as_deref()) {
            Some(_) => {
                icalendar::Property::new("RECURRENCE-ID", naive.format("%Y%m%dT%H%M%S").to_string())
            }
            None => icalendar::Property::new(
                "RECURRENCE-ID",
                naive.format("%Y%m%dT%H%M%SZ").to_string(),
            ),
        };
        if let Some(tz) = known_tz(event.tzid.as_deref()) {
            prop.add_parameter("TZID", tz.name());
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
}

impl From<(EventRow, Vec<AttendeeRow>)> for ExportRow {
    fn from((event, attendees): (EventRow, Vec<AttendeeRow>)) -> Self {
        Self {
            event,
            attendees,
            alarms: vec![],
            location: None,
        }
    }
}

/// One VEVENT per row (masters and exceptions alike); a calendar export is a
/// VCALENDAR of all of them.
pub fn events_to_ics(rows: &[ExportRow]) -> String {
    let mut calendar = Calendar::new();
    calendar.name("calendar-server");
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
                    at,
                    today(),
                ));
            }
            (None, Some(date)) => {
                ev.append_property(date_time_property(
                    "DTSTART",
                    event.tzid.as_deref(),
                    true,
                    Utc::now(),
                    date,
                ));
            }
            (None, None) => {}
        }
        if let Some(at) = event.ends_at {
            ev.append_property(date_time_property(
                "DTEND",
                event.tzid.as_deref(),
                false,
                at,
                today(),
            ));
        }
        if let Some(date) = event.end_date {
            ev.append_property(date_time_property(
                "DTEND",
                event.tzid.as_deref(),
                true,
                Utc::now(),
                date,
            ));
        }
        if let Some(rrule) = &event.rrule {
            ev.add_property("RRULE", rrule);
        }
        if event.rdate.as_array().is_some_and(|a| !a.is_empty()) {
            let values = json_points_to_ics(&event.rdate, event.tzid.as_deref());
            ev.add_property("RDATE", &values);
        }
        if event.exdate.as_array().is_some_and(|a| !a.is_empty()) {
            let values = json_points_to_ics(&event.exdate, event.tzid.as_deref());
            ev.add_property("EXDATE", &values);
        }
        if let Some(recurrence) = recurrence_id_property(event) {
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
            let mut prop =
                icalendar::Property::new("ATTENDEE", format!("mailto:{}", attendee.email));
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
    calendar.to_string()
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
    if alarm.action == "EMAIL" {
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

fn json_points_to_ics(value: &serde_json::Value, tzid: Option<&str>) -> String {
    let _ = tzid;
    value
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| {
                    if let Ok(at) = DateTime::parse_from_rfc3339(s) {
                        at.format("%Y%m%dT%H%M%SZ").to_string()
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
    pub email: String,
    pub display_name: Option<String>,
    pub role: Option<String>,
    pub partstat: Option<String>,
    pub rsvp: Option<bool>,
}

/// Parses one VCALENDAR into its VEVENTs. VTODO (or any unsupported
/// component) is rejected per ADR-011.
pub fn parse_ics(text: &str) -> Result<Vec<ParsedEvent>, IcsError> {
    // The icalendar 0.17 parser rejects RFC 5545 line folding; unfold first.
    let unfolded = unfold(text);
    let calendar = icalendar::parser::read_calendar(&unfolded).map_err(IcsError::Parse)?;
    let mut out = Vec::new();
    for component in calendar.components {
        let name = component.name.as_ref();
        if name == "VTODO" {
            return Err(IcsError::TodoUnsupported);
        }
        if name != "VEVENT" {
            continue;
        }
        let alarms: Vec<ParsedAlarm> = component
            .components
            .iter()
            .filter(|sub| sub.name.as_ref() == "VALARM")
            .map(|sub| parse_alarm(sub))
            .collect();
        let event = to_owned_event(&component);
        let mut parsed = parse_event(event)?;
        parsed.alarms = alarms;
        out.push(parsed);
    }
    Ok(out)
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

fn parse_event(event: icalendar::Event) -> Result<ParsedEvent, IcsError> {
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
    let Some(point) = points_to_core(&points) else {
        return Err(IcsError::MissingDtstart);
    };
    apply_date_point(&mut parsed, &point, true);
    if let Some(end) = event.get_end()
        && let Some(point) = points_to_core(&end)
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
        for prop in props {
            for value in prop.value().split(',') {
                if value.is_empty() {
                    continue;
                }
                // TZID-qualified values are converted through the zone so the
                // stored point is an absolute instant regardless of form.
                let tzid = prop
                    .params()
                    .get("TZID")
                    .and_then(|p| p.value().parse::<Tz>().ok());
                let point = if value.len() == 8 && value.chars().all(|c| c.is_ascii_digit()) {
                    NaiveDate::parse_from_str(value, "%Y%m%d")
                        .ok()
                        .map(DateOrDateTime::AllDay)
                } else if let Some(tz) = tzid {
                    NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")
                        .ok()
                        .and_then(|naive| instant_in_zone(tz, naive))
                        .map(DateOrDateTime::Timed)
                } else {
                    parse_ics_datetime(value).map(DateOrDateTime::Timed)
                };
                match point {
                    Some(DateOrDateTime::Timed(at)) if key == "RDATE" => {
                        parsed.rdate.push(DateOrDateTime::Timed(at))
                    }
                    Some(DateOrDateTime::AllDay(date)) if key == "RDATE" => {
                        parsed.rdate.push(DateOrDateTime::AllDay(date))
                    }
                    Some(DateOrDateTime::Timed(at)) => {
                        parsed.exdate.push(DateOrDateTime::Timed(at))
                    }
                    Some(DateOrDateTime::AllDay(date)) => {
                        parsed.exdate.push(DateOrDateTime::AllDay(date))
                    }
                    None => {}
                }
            }
        }
    }
    let mut categories: Vec<String> = Vec::new();
    if let Some(value) = event.property_value("CATEGORIES") {
        categories.push(value.to_string());
    }
    if let Some(props) = event.multi_properties().get("CATEGORIES") {
        for prop in props {
            categories.push(prop.value().to_string());
        }
    }
    for joined in categories {
        for category in joined.split(',') {
            let category = category.trim();
            if !category.is_empty() && !parsed.categories.iter().any(|c| c == category) {
                parsed.categories.push(category.to_string());
            }
        }
    }
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
fn parse_alarm(component: &icalendar::parser::Component<'_>) -> ParsedAlarm {
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
fn parse_ics_duration(value: &str) -> Option<i64> {
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

fn attendee_from_prop(prop: &icalendar::Property) -> ParsedAttendee {
    let mailto = prop.value();
    ParsedAttendee {
        email: mailto
            .split_once(':')
            .map(|(_, rest)| rest)
            .unwrap_or(mailto)
            .to_string(),
        display_name: prop.params().get("CN").map(|p| p.value().to_string()),
        role: prop.params().get("ROLE").map(|p| p.value().to_string()),
        partstat: prop.params().get("PARTSTAT").map(|p| p.value().to_string()),
        rsvp: prop
            .params()
            .get("RSVP")
            .map(|p| p.value().eq_ignore_ascii_case("true")),
    }
}

fn instant_in_zone(tz: Tz, naive: NaiveDateTime) -> Option<DateTime<Utc>> {
    use chrono::LocalResult;
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(dt) => Some(dt.with_timezone(&Utc)),
        LocalResult::Ambiguous(earliest, _) => Some(earliest.with_timezone(&Utc)),
        LocalResult::None => None,
    }
}

fn points_to_core(point: &DatePerhapsTime) -> Option<DateOrDateTime> {
    match point {
        DatePerhapsTime::Date(date) => Some(DateOrDateTime::AllDay(*date)),
        DatePerhapsTime::DateTime(dt) => dt.try_into_utc().map(DateOrDateTime::Timed),
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
        assert_eq!(ev.attendees[0].email, "al@example.com");
        assert_eq!(ev.attendees[0].partstat.as_deref(), Some("ACCEPTED"));
        assert_eq!(ev.attendees[0].rsvp, Some(true));
        assert_eq!(ev.rrule.as_deref(), Some("FREQ=WEEKLY;BYDAY=MO"));
        assert_eq!(ev.exdate.len(), 1);
        assert_eq!(ev.sequence, Some(2));
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
            event,
            attendees: vec![],
            alarms: vec![],
            location: Some(sample_location()),
        }]);
        assert!(ics.contains("LOCATION:Union Station"));
        assert!(ics.contains("GEO:39.7534;-105.0016"));
    }

    #[test]
    fn vtodo_rejected() {
        let todo = "BEGIN:VCALENDAR\r\nPRODID:-//x//EN\r\nVERSION:2.0\r\n\
BEGIN:VTODO\r\nUID:t1\r\nDTSTAMP:20260911T120000Z\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        assert!(matches!(parse_ics(todo), Err(IcsError::TodoUnsupported)));
    }

    fn sample_event_row() -> calendar_db::EventRow {
        let now = Utc::now();
        calendar_db::EventRow {
            id: uuid::Uuid::new_v4(),
            calendar_id: uuid::Uuid::new_v4(),
            uid: "round-trip-uid".into(),
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
            email: "al@example.com".into(),
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
        assert_eq!(parsed[0].attendees[0].email, "al@example.com");
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
}
