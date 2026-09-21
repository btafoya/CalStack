//! VTODO wire mapping (ADR-015; docs/TASKS_JOURNALS_DESIGN.md section 4):
//! parse a VTODO resource into `tasks`-table shapes and render a task series
//! (master plus RECURRENCE-ID overrides, one resource) back to ICS.

use crate::{
    IcsError, Zones, attendee_from_prop, collect_categories, date_time_property,
    extra_props_from_json, extra_props_json, parse_alarm, parse_ics_duration,
    parse_recurrence_points, parse_wire_point,
};
use calendar_db::tasks::{NewTaskData, TaskAlarmRow, TaskAttendeeRow, TaskPatch, TaskRow};
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use icalendar::Component;

/// Parsed VTODO fields, normalized to the tasks table's column shapes.
#[derive(Debug, Default, Clone)]
pub struct ParsedTodo {
    pub uid: String,
    pub summary: Option<String>,
    pub description_text: Option<String>,
    pub description_html: Option<String>,
    pub url: Option<String>,
    pub location: Option<String>,
    pub starts_at: Option<DateTime<Utc>>,
    pub start_date: Option<NaiveDate>,
    pub due_at: Option<DateTime<Utc>>,
    pub due_date: Option<NaiveDate>,
    /// DURATION property in seconds (alternative to DUE).
    pub duration_secs: Option<i64>,
    pub tzid: Option<String>,
    /// DTSTART/DUE carried neither Z nor TZID; wall clock stored as if UTC.
    pub floating: bool,
    pub completed_at: Option<DateTime<Utc>>,
    pub rrule: Option<String>,
    pub rdate: Vec<calendar_core::DateOrDateTime>,
    pub exdate: Vec<calendar_core::DateOrDateTime>,
    pub status: Option<String>,
    pub percent_complete: Option<i16>,
    pub priority: Option<i16>,
    pub class: Option<String>,
    pub categories: Vec<String>,
    /// RELATED-TO with RELTYPE PARENT (or absent RELTYPE).
    pub parent_uid: Option<String>,
    /// X-APPLE-SORT-ORDER.
    pub sort_order: Option<i64>,
    pub extra_props: Vec<crate::ExtraProp>,
    pub organizer_email: Option<String>,
    pub organizer_name: Option<String>,
    pub attendees: Vec<crate::ParsedAttendee>,
    pub alarms: Vec<crate::ParsedAlarm>,
    pub sequence: Option<i32>,
    pub recurrence_id: Option<NaiveDateTime>,
    pub recurrence_id_date: Option<NaiveDate>,
}

/// A parsed VTODO resource: exactly one master plus its own overrides, and
/// any client-supplied VTIMEZONEs from the same body (ADR-012).
#[derive(Debug)]
pub struct ParsedTodoSeries {
    pub master: ParsedTodo,
    pub overrides: Vec<ParsedTodo>,
    pub timezones: Vec<crate::ParsedTimezone>,
}

/// Maps one VTODO component onto the tasks-table shapes. VTIMEZONE sub-parts
/// never appear here; VALARMs become task alarms, ATTENDEE/ORGANIZER become
/// task attendees and organizer columns, X-ALT-DESC is sanitized
/// (ADR-005), and every unmodelled property is preserved in extra_props.
pub(crate) fn parse_todo(
    component: &icalendar::parser::Component,
    custom: &Zones,
) -> Result<ParsedTodo, IcsError> {
    let mut todo = ParsedTodo::default();
    let mut description_taken = false;
    let mut alt_desc_taken = false;
    for raw in &component.properties {
        // Convert once: the owned Property carries key/value/params accessors.
        let prop: icalendar::Property = raw.clone().into();
        let name = prop.key().to_string();
        let value = prop.value().to_string();
        match name.as_str() {
            "UID" => todo.uid = value.trim().to_string(),
            "SUMMARY" => todo.summary = Some(value),
            "URL" => todo.url = Some(value),
            "LOCATION" => todo.location = Some(value),
            "STATUS" => todo.status = Some(value.trim().to_string()),
            "CLASS" => todo.class = Some(value.trim().to_string()),
            "DESCRIPTION" => {
                if description_taken {
                    todo.extra_props.push(extra_prop(&prop));
                } else {
                    todo.description_text = Some(value);
                    description_taken = true;
                }
            }
            "X-ALT-DESC" => {
                if alt_desc_taken {
                    todo.extra_props.push(extra_prop(&prop));
                } else {
                    todo.description_html = Some(calendar_core::sanitize_html(&value));
                    alt_desc_taken = true;
                }
            }
            "DTSTART" => apply_point(&mut todo, parse_wire_point(&prop, custom), true),
            "DUE" => apply_point(&mut todo, parse_wire_point(&prop, custom), false),
            "DURATION" => todo.duration_secs = parse_ics_duration(&value),
            "COMPLETED" => {
                todo.completed_at = NaiveDateTime::parse_from_str(
                    value.trim().trim_end_matches('Z'),
                    "%Y%m%dT%H%M%S",
                )
                .ok()
                .map(|naive| Utc.from_utc_datetime(&naive));
            }
            "RRULE" => todo.rrule = Some(value.trim().to_string()),
            "RDATE" | "EXDATE" => {
                let points = parse_recurrence_points(vec![prop.clone()], custom);
                if name == "RDATE" {
                    todo.rdate.extend(points);
                } else {
                    todo.exdate.extend(points);
                }
            }
            "RECURRENCE-ID" => {
                if value.len() == 8 && value.chars().all(|c| c.is_ascii_digit()) {
                    todo.recurrence_id_date = NaiveDate::parse_from_str(&value, "%Y%m%d").ok();
                } else {
                    let naive = NaiveDateTime::parse_from_str(value.trim(), "%Y%m%dT%H%M%SZ")
                        .or_else(|_| NaiveDateTime::parse_from_str(value.trim(), "%Y%m%dT%H%M%S"))
                        .map_err(|_| IcsError::Parse("bad RECURRENCE-ID".into()))?;
                    todo.recurrence_id = Some(naive);
                    if let Some(tzid) = prop.params().get("TZID").map(|p| p.value().to_string()) {
                        todo.tzid.get_or_insert(tzid);
                    }
                }
            }
            "PERCENT-COMPLETE" => todo.percent_complete = value.trim().parse().ok(),
            "PRIORITY" => todo.priority = value.trim().parse().ok(),
            "SEQUENCE" => todo.sequence = value.trim().parse().ok(),
            "X-APPLE-SORT-ORDER" => todo.sort_order = value.trim().parse().ok(),
            "CATEGORIES" => {}
            "RELATED-TO" => {
                let reltype = prop.params().get("RELTYPE").map(|p| p.value().to_string());
                match reltype.as_deref() {
                    None | Some("PARENT") if todo.parent_uid.is_none() => {
                        todo.parent_uid = Some(value);
                    }
                    _ => todo.extra_props.push(extra_prop(&prop)),
                }
            }
            "ORGANIZER" => {
                todo.organizer_email = Some(
                    value
                        .split_once(':')
                        .map(|(_, rest)| rest)
                        .unwrap_or(&value)
                        .to_string(),
                );
                todo.organizer_name = prop.params().get("CN").map(|p| p.value().to_string());
            }
            "ATTENDEE" => todo.attendees.push(attendee_from_prop(&prop)),
            "DTSTAMP" | "LAST-MODIFIED" | "CREATED" => {}
            _ => todo.extra_props.push(extra_prop(&prop)),
        }
    }
    if todo.uid.is_empty() {
        return Err(IcsError::MissingUid);
    }
    todo.categories = collect_categories(&to_owned(component));
    todo.alarms = component
        .components
        .iter()
        .filter(|sub| sub.name.as_ref() == "VALARM")
        .map(parse_alarm)
        .collect();
    Ok(todo)
}

/// One parser property as a preserved extra prop (params in wire order).
fn extra_prop(prop: &icalendar::Property) -> crate::ExtraProp {
    crate::ExtraProp {
        name: prop.key().to_string(),
        params: prop
            .params()
            .iter()
            .map(|(k, v)| (k.clone(), v.value().to_string()))
            .collect(),
        value: prop.value().to_string(),
    }
}

fn to_owned(component: &icalendar::parser::Component) -> icalendar::Todo {
    let mut todo = icalendar::Todo::new();
    for prop in &component.properties {
        todo.append_property(prop.clone());
    }
    todo
}

/// Applies a parsed DTSTART/DUE point to the task: date → `*_date`, timed →
/// `*_at` (floating keeps the wall clock as if UTC, plus the flag), and the
/// zone identity is preserved for round-trips.
fn apply_point(todo: &mut ParsedTodo, point: Option<crate::WirePoint>, start: bool) {
    let Some(point) = point else { return };
    if let Some(tzid) = point.tzid {
        todo.tzid.get_or_insert(tzid);
    }
    if point.floating {
        todo.floating = true;
    }
    if let Some(date) = point.date {
        if start {
            todo.start_date = Some(date);
        } else {
            todo.due_date = Some(date);
        }
    }
    if let Some(at) = point.at {
        if start {
            todo.starts_at = Some(at);
        } else {
            todo.due_at = Some(at);
        }
    }
}

/// Wire attendee → storage attendee (shared with the flush path).
pub(crate) fn attendee(a: &crate::ParsedAttendee) -> calendar_db::NewAttendee {
    calendar_db::NewAttendee {
        email: a.email.clone(),
        telephone: a.telephone.clone(),
        display_name: a.display_name.clone(),
        role: a.role.clone(),
        partstat: a.partstat.clone(),
        rsvp: a.rsvp,
        ..Default::default()
    }
}

/// Wire alarm → storage alarm (same channel mapping as the event path:
/// the wire only carries DISPLAY/EMAIL).
pub(crate) fn alarm_data(a: &crate::ParsedAlarm) -> calendar_db::alarms::NewAlarm {
    calendar_db::alarms::NewAlarm {
        action: a.action.clone(),
        related: a.related.clone(),
        offset_secs: a.offset_secs,
        trigger_at: a.trigger_at,
        description: a.description.clone(),
        summary: a.summary.clone(),
        recipient_emails: a.recipients.clone(),
        notify_channels: if a.action.eq_ignore_ascii_case("EMAIL") {
            vec!["in_app".into(), "email".into()]
        } else {
            vec!["in_app".into()]
        },
    }
}

/// Parsed master VTODO → storage record for a new task. Returns an error when
/// the extra-props set breaks the injection-safety rules.
pub(crate) fn new_task_data(parsed: &ParsedTodo) -> Result<NewTaskData, IcsError> {
    Ok(NewTaskData {
        uid: parsed.uid.clone(),
        // Set by the PUT path (the client's filename); None = "{id}.ics".
        href: None,
        starts_at: parsed.starts_at,
        start_date: parsed.start_date,
        due_at: parsed.due_at,
        due_date: parsed.due_date,
        duration_secs: parsed.duration_secs,
        tzid: parsed.tzid.clone(),
        floating: parsed.floating,
        completed_at: parsed.completed_at,
        rrule: parsed.rrule.clone(),
        rdate: Some(points_json(&parsed.rdate)),
        exdate: Some(points_json(&parsed.exdate)),
        summary: parsed.summary.clone().unwrap_or_default(),
        description_html: parsed.description_html.clone(),
        description_text: parsed.description_text.clone(),
        url: parsed.url.clone(),
        location: parsed.location.clone(),
        status: parsed.status.clone(),
        percent_complete: parsed.percent_complete,
        priority: parsed.priority,
        class: parsed.class.clone(),
        categories: parsed.categories.clone(),
        parent_uid: parsed.parent_uid.clone(),
        sort_order: parsed.sort_order,
        extra_props: Some(extra_props_json(&parsed.extra_props)?),
        organizer_user_id: None,
        organizer_email: parsed.organizer_email.clone(),
        organizer_name: parsed.organizer_name.clone(),
    })
}

/// Parsed VTODO → full-replace patch for an existing task. Fields the patch
/// model cannot express (extra_props, rdate/exdate, RRULE, COMPLETED) are
/// left untouched by the db layer — see the WriteFile::flush note.
pub(crate) fn task_patch(parsed: &ParsedTodo) -> Result<TaskPatch, IcsError> {
    Ok(TaskPatch {
        summary: Some(parsed.summary.clone().unwrap_or_default()),
        description_html: parsed.description_html.clone(),
        description_text: parsed.description_text.clone(),
        url: parsed.url.clone(),
        location: parsed.location.clone(),
        starts_at: parsed.starts_at,
        start_date: parsed.start_date,
        due_at: parsed.due_at,
        due_date: parsed.due_date,
        duration_secs: parsed.duration_secs,
        tzid: parsed.tzid.clone(),
        floating: Some(parsed.floating),
        status: parsed.status.clone(),
        percent_complete: parsed.percent_complete,
        priority: parsed.priority,
        class: parsed.class.clone(),
        categories: Some(parsed.categories.clone()),
        parent_uid: Some(parsed.parent_uid.clone()),
        sort_order: parsed.sort_order,
        attendees: Some(parsed.attendees.iter().map(attendee).collect()),
        alarms: Some(parsed.alarms.iter().map(alarm_data).collect()),
    })
}

fn points_json(points: &[calendar_core::DateOrDateTime]) -> serde_json::Value {
    serde_json::Value::Array(
        points
            .iter()
            .map(|p| match p {
                calendar_core::DateOrDateTime::Timed(at) => serde_json::json!(at.to_rfc3339()),
                calendar_core::DateOrDateTime::AllDay(date) => serde_json::json!(date.to_string()),
            })
            .collect(),
    )
}

// ============ serialization ============

/// One exportable task resource row: the task (master or override) with its
/// attendees, alarms, and the calendar's stored VTIMEZONEs (ADR-012).
pub struct TaskExportRow {
    pub task: TaskRow,
    pub attendees: Vec<TaskAttendeeRow>,
    pub alarms: Vec<TaskAlarmRow>,
    pub vtimezones: Vec<calendar_db::timezones::StoredTimezone>,
}

/// One VTODO per row (master first, then overrides) in a single VCALENDAR.
/// STATUS NULL exports as NEEDS-ACTION (the RFC default); extra_props are
/// re-emitted verbatim (folded by the ICS writer) after the modelled
/// properties.
pub fn todos_to_ics(rows: &[TaskExportRow]) -> String {
    let mut calendar = icalendar::Calendar::new();
    calendar.name("calendar-server");
    // Custom (non-tzdb) zones: DTSTART/DUE/RECURRENCE-ID with such a TZID
    // render as local wall clock with the TZID parameter.
    let custom_zones: Zones = rows
        .iter()
        .flat_map(|row| row.vtimezones.iter())
        .filter(|stored| !calendar_core::recurrence::is_tzdb_tzid(&stored.tzid))
        .filter_map(|stored| {
            crate::compiled_zone(&stored.tzid, &stored.rules)
                .map(|zone| (stored.tzid.clone(), zone))
        })
        .collect();
    let mut zone_text = String::new();
    for row in rows {
        let (task, alarms) = (&row.task, &row.alarms);
        let mut td = icalendar::Todo::new();
        td.uid(&task.uid);
        if !task.summary.is_empty() {
            td.summary(&task.summary);
        }
        if let Some(text) = &task.description_text {
            td.description(text);
        }
        if let Some(html) = &task.description_html {
            let mut prop = icalendar::Property::new("X-ALT-DESC", html);
            prop.add_parameter("FMTTYPE", "text/html");
            td.append_property(prop);
        }
        if let Some(url) = &task.url {
            td.add_property("URL", url);
        }
        if let Some(loc) = &task.location {
            td.add_property("LOCATION", loc);
        }
        if let Some(at) = task.starts_at {
            td.append_property(date_time_property(
                "DTSTART",
                task.tzid.as_deref(),
                false,
                task.floating,
                at,
                today(),
                &custom_zones,
            ));
        } else if let Some(date) = task.start_date {
            td.append_property(date_time_property(
                "DTSTART",
                task.tzid.as_deref(),
                true,
                false,
                Utc::now(),
                date,
                &custom_zones,
            ));
        }
        if let Some(at) = task.due_at {
            td.append_property(date_time_property(
                "DUE",
                task.tzid.as_deref(),
                false,
                task.floating,
                at,
                today(),
                &custom_zones,
            ));
        } else if let Some(date) = task.due_date {
            td.append_property(date_time_property(
                "DUE",
                task.tzid.as_deref(),
                true,
                false,
                Utc::now(),
                date,
                &custom_zones,
            ));
        }
        if task.due_at.is_none()
            && task.due_date.is_none()
            && let Some(interval) = &task.duration
        {
            td.add_property("DURATION", format_duration(interval));
        }
        if task.master_task_id.is_none() {
            if let Some(rrule) = &task.rrule {
                td.add_property("RRULE", rrule);
            }
            if task.rdate.as_array().is_some_and(|a| !a.is_empty()) {
                td.add_property("RDATE", rdate_text(&task.rdate, task.floating));
            }
            if task.exdate.as_array().is_some_and(|a| !a.is_empty()) {
                td.add_property("EXDATE", rdate_text(&task.exdate, task.floating));
            }
        }
        if let Some(prop) = recurrence_id_property(task, &custom_zones) {
            td.append_property(prop);
        }
        td.add_property("STATUS", task.status.as_deref().unwrap_or("NEEDS-ACTION"));
        if let Some(percent) = task.percent_complete {
            td.add_property("PERCENT-COMPLETE", percent.to_string());
        }
        if let Some(at) = task.completed_at {
            td.add_property("COMPLETED", at.format("%Y%m%dT%H%M%SZ").to_string());
        }
        if let Some(priority) = task.priority {
            td.add_property("PRIORITY", priority.to_string());
        }
        if let Some(class) = &task.class {
            td.add_property("CLASS", class);
        }
        if !task.categories.is_empty() {
            td.add_property("CATEGORIES", task.categories.join(","));
        }
        if let Some(parent) = &task.parent_uid {
            td.add_property("RELATED-TO", parent);
        }
        if let Some(sort_order) = task.sort_order {
            td.add_property("X-APPLE-SORT-ORDER", sort_order.to_string());
        }
        if let Some(email) = &task.organizer_email {
            let mut prop = icalendar::Property::new("ORGANIZER", format!("mailto:{email}"));
            prop.add_parameter("CN", task.organizer_name.as_deref().unwrap_or(""));
            td.append_property(prop);
        }
        for attendee in &row.attendees {
            let value = match (&attendee.email, &attendee.telephone) {
                (Some(email), _) => format!("mailto:{email}"),
                (None, Some(phone)) => format!("sms:{phone}"),
                (None, None) => continue,
            };
            let mut prop = icalendar::Property::new("ATTENDEE", value);
            prop.add_parameter("CN", attendee.display_name.as_deref().unwrap_or(""));
            prop.add_parameter("PARTSTAT", &attendee.partstat);
            prop.add_parameter("ROLE", &attendee.role);
            if let Some(rsvp) = attendee.rsvp {
                prop.add_parameter("RSVP", if rsvp { "TRUE" } else { "FALSE" });
            }
            td.append_property(prop);
        }
        for alarm in alarms {
            let valarm = build_alarm(alarm, &task.summary);
            td.append_component(valarm);
        }
        td.add_property("SEQUENCE", task.sequence.max(0).to_string());
        td.append_property(icalendar::Property::new(
            "DTSTAMP",
            task.updated_at.format("%Y%m%dT%H%M%SZ").to_string(),
        ));
        td.last_modified(task.updated_at);
        for prop in extra_props_from_json(&task.extra_props) {
            let mut wire = icalendar::Property::new(&prop.name, &prop.value);
            for (key, value) in &prop.params {
                wire.add_parameter(key, value);
            }
            td.append_property(wire);
        }
        calendar.push(td);
    }
    // Custom-zone round-trips: the calendar's stored VTIMEZONE definitions
    // are spliced ahead of the components, as for events.
    let mut out = calendar.to_string();
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
    splice_vtimezones(&mut out, &zone_text);
    out
}

/// RECURRENCE-ID for an override row: stored as wall clock in the task's
/// zone (or the date for all-day series).
fn recurrence_id_property(task: &TaskRow, custom_zones: &Zones) -> Option<icalendar::Property> {
    if let Some(date) = task.recurrence_id_date {
        let mut prop = icalendar::Property::new("RECURRENCE-ID", date.format("%Y%m%d").to_string());
        prop.add_parameter("VALUE", "DATE");
        Some(prop)
    } else if let Some(naive) = task.recurrence_id {
        let has_zone = !task.floating
            && task.tzid.as_deref().is_some_and(|t| {
                t.parse::<chrono_tz::Tz>().is_ok() || custom_zones.contains_key(t)
            });
        let mut prop = if has_zone || task.floating {
            icalendar::Property::new("RECURRENCE-ID", naive.format("%Y%m%dT%H%M%S").to_string())
        } else {
            icalendar::Property::new("RECURRENCE-ID", naive.format("%Y%m%dT%H%M%SZ").to_string())
        };
        if has_zone {
            prop.add_parameter("TZID", task.tzid.as_deref().unwrap_or_default());
        }
        Some(prop)
    } else {
        None
    }
}

fn rdate_text(value: &serde_json::Value, floating: bool) -> String {
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

/// VALARM for a task alarm: same rules as the event path (EMAIL carries
/// recipients, everything else renders DISPLAY).
fn build_alarm(alarm: &TaskAlarmRow, summary: &str) -> icalendar::Alarm {
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
        .unwrap_or_else(|| summary.to_string());
    let mut valarm = icalendar::Alarm::display(&description, trigger);
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
        if let Some(text) = &alarm.summary {
            valarm.add_property("SUMMARY", text);
        }
    }
    valarm
}

fn format_duration(interval: &sqlx::postgres::types::PgInterval) -> String {
    let secs = interval.microseconds / 1_000_000;
    let (sign, secs) = if secs < 0 { ("-", -secs) } else { ("", secs) };
    let days = secs / 86_400;
    let rest = secs % 86_400;
    let (h, m, s) = (rest / 3600, (rest % 3600) / 60, rest % 60);
    let mut out = format!("{sign}P");
    if days > 0 {
        out.push_str(&format!("{days}D"));
    }
    if h > 0 || m > 0 || s > 0 || days == 0 {
        out.push('T');
        if h > 0 {
            out.push_str(&format!("{h}H"));
        }
        if m > 0 {
            out.push_str(&format!("{m}M"));
        }
        if s > 0 || (h == 0 && m == 0) {
            out.push_str(&format!("{s}S"));
        }
    }
    out
}

fn today() -> NaiveDate {
    Utc::now().date_naive()
}

/// Splices stored VTIMEZONE definition text ahead of the first component.
pub(crate) fn splice_vtimezones(out: &mut String, zone_text: &str) {
    if zone_text.is_empty() {
        return;
    }
    if let Some(pos) = out
        .find("BEGIN:VEVENT")
        .or_else(|| out.find("BEGIN:VTODO"))
        .or_else(|| out.find("BEGIN:VJOURNAL"))
        .or_else(|| out.rfind("END:VCALENDAR"))
    {
        out.insert_str(pos, zone_text);
    }
}
