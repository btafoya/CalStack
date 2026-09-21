//! VJOURNAL wire mapping (ADR-015 D9): modelled summary, first DESCRIPTION,
//! DTSTART, STATUS, CLASS, CATEGORIES, URL — one VJOURNAL per resource, no
//! overrides. Everything else (extra DESCRIPTIONs, ORGANIZER, ATTENDEE,
//! RRULE/RDATE/EXDATE, RELATED-TO) round-trips through extra_props.

use crate::{
    IcsError, Zones, collect_categories, date_time_property, extra_props_from_json,
    extra_props_json, parse_wire_point,
};
use calendar_db::journals::{JournalPatch, JournalRow, NewJournalData};
use chrono::{NaiveDate, Utc};
use icalendar::Component;

/// Parsed VJOURNAL fields, normalized to the journals table's column shapes.
#[derive(Debug, Default, Clone)]
pub struct ParsedJournal {
    pub uid: String,
    pub summary: Option<String>,
    pub description_text: Option<String>,
    pub description_html: Option<String>,
    pub url: Option<String>,
    pub starts_at: Option<chrono::DateTime<Utc>>,
    pub start_date: Option<NaiveDate>,
    pub tzid: Option<String>,
    /// DTSTART carried neither Z nor TZID; wall clock stored as if UTC.
    pub floating: bool,
    pub status: Option<String>,
    pub class: Option<String>,
    pub categories: Vec<String>,
    pub extra_props: Vec<crate::ExtraProp>,
    pub sequence: Option<i32>,
}

/// Maps one VJOURNAL component onto the journals-table shapes. X-ALT-DESC is
/// sanitized (ADR-005); the first DESCRIPTION is modelled, later ones are
/// preserved; DTSTART is optional (undated = a note).
pub(crate) fn parse_journal(
    component: &icalendar::parser::Component,
    custom: &Zones,
) -> Result<ParsedJournal, IcsError> {
    let mut journal = ParsedJournal::default();
    let mut description_taken = false;
    let mut alt_desc_taken = false;
    for raw in &component.properties {
        // Convert once: the owned Property carries key/value/params accessors.
        let prop: icalendar::Property = raw.clone().into();
        let name = prop.key().to_string();
        let value = prop.value().to_string();
        match name.as_str() {
            "UID" => journal.uid = value.trim().to_string(),
            "SUMMARY" => journal.summary = Some(value),
            "URL" => journal.url = Some(value),
            "STATUS" => journal.status = Some(value.trim().to_string()),
            "CLASS" => journal.class = Some(value.trim().to_string()),
            "SEQUENCE" => journal.sequence = value.trim().parse().ok(),
            "DESCRIPTION" => {
                if description_taken {
                    journal.extra_props.push(extra_prop(&prop));
                } else {
                    journal.description_text = Some(value);
                    description_taken = true;
                }
            }
            "X-ALT-DESC" => {
                if alt_desc_taken {
                    journal.extra_props.push(extra_prop(&prop));
                } else {
                    journal.description_html = Some(calendar_core::sanitize_html(&value));
                    alt_desc_taken = true;
                }
            }
            "DTSTART" => {
                if let Some(point) = parse_wire_point(&prop, custom) {
                    if let Some(tzid) = point.tzid {
                        journal.tzid.get_or_insert(tzid);
                    }
                    journal.floating = point.floating;
                    journal.start_date = point.date;
                    journal.starts_at = point.at;
                }
            }
            "CATEGORIES" => {}
            "DTSTAMP" | "LAST-MODIFIED" | "CREATED" => {}
            _ => journal.extra_props.push(extra_prop(&prop)),
        }
    }
    if journal.uid.is_empty() {
        return Err(IcsError::MissingUid);
    }
    journal.categories = collect_categories(&to_owned(component));
    Ok(journal)
}

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
    // Any component shell works: only the generic property access is used.
    let mut todo = icalendar::Todo::new();
    for prop in &component.properties {
        todo.append_property(prop.clone());
    }
    todo
}

/// Parsed VJOURNAL → storage record for a new journal.
pub(crate) fn new_journal_data(parsed: &ParsedJournal) -> Result<NewJournalData, IcsError> {
    Ok(NewJournalData {
        uid: parsed.uid.clone(),
        // Set by the PUT path (the client's filename); None = "{id}.ics".
        href: None,
        starts_at: parsed.starts_at,
        start_date: parsed.start_date,
        tzid: parsed.tzid.clone(),
        floating: parsed.floating,
        summary: parsed.summary.clone().unwrap_or_default(),
        description_html: parsed.description_html.clone(),
        description_text: parsed.description_text.clone(),
        url: parsed.url.clone(),
        status: parsed.status.clone(),
        class: parsed.class.clone(),
        categories: parsed.categories.clone(),
        extra_props: Some(extra_props_json(&parsed.extra_props)?),
    })
}

/// Parsed VJOURNAL → full-replace patch for an existing journal. The patch
/// model cannot express extra_props (the db layer leaves them untouched).
pub(crate) fn journal_patch(parsed: &ParsedJournal) -> JournalPatch {
    JournalPatch {
        summary: Some(parsed.summary.clone().unwrap_or_default()),
        description_html: parsed.description_html.clone(),
        description_text: parsed.description_text.clone(),
        url: parsed.url.clone(),
        starts_at: parsed.starts_at,
        start_date: parsed.start_date,
        tzid: parsed.tzid.clone(),
        floating: Some(parsed.floating),
        status: parsed.status.clone(),
        class: parsed.class.clone(),
        categories: Some(parsed.categories.clone()),
    }
}

// ============ serialization ============

/// One VJOURNAL per resource. STATUS exports only when stored (the journal
/// vocabulary has no RFC default like the task's NEEDS-ACTION); extra_props
/// are re-emitted verbatim after the modelled properties. Built as a Todo
/// (icalendar has no VJOURNAL type) and renamed on the wire; the crate
/// handles escaping and 75-octet folding.
pub fn journal_to_ics(journal: &JournalRow) -> String {
    let today = Utc::now().date_naive();
    let zones = std::collections::HashMap::new();
    let mut jr = icalendar::Todo::new();
    jr.uid(&journal.uid);
    if !journal.summary.is_empty() {
        jr.summary(&journal.summary);
    }
    if let Some(text) = &journal.description_text {
        jr.description(text);
    }
    if let Some(html) = &journal.description_html {
        let mut prop = icalendar::Property::new("X-ALT-DESC", html);
        prop.add_parameter("FMTTYPE", "text/html");
        jr.append_property(prop);
    }
    if let Some(url) = &journal.url {
        jr.add_property("URL", url);
    }
    if let Some(at) = journal.starts_at {
        jr.append_property(date_time_property(
            "DTSTART",
            journal.tzid.as_deref(),
            false,
            journal.floating,
            at,
            today,
            &zones,
        ));
    } else if let Some(date) = journal.start_date {
        jr.append_property(date_time_property(
            "DTSTART",
            journal.tzid.as_deref(),
            true,
            false,
            Utc::now(),
            date,
            &zones,
        ));
    }
    if let Some(status) = &journal.status {
        jr.add_property("STATUS", status);
    }
    if let Some(class) = &journal.class {
        jr.add_property("CLASS", class);
    }
    if !journal.categories.is_empty() {
        jr.add_property("CATEGORIES", journal.categories.join(","));
    }
    jr.add_property("SEQUENCE", journal.sequence.max(0).to_string());
    jr.append_property(icalendar::Property::new(
        "DTSTAMP",
        journal.updated_at.format("%Y%m%dT%H%M%SZ").to_string(),
    ));
    jr.last_modified(journal.updated_at);
    for prop in extra_props_from_json(&journal.extra_props) {
        let mut wire = icalendar::Property::new(&prop.name, &prop.value);
        for (key, value) in &prop.params {
            wire.add_parameter(key, value);
        }
        jr.append_property(wire);
    }
    // icalendar has no VJOURNAL component; the shell name is the only
    // difference from VTODO on the wire. Wrap in the VCALENDAR shell the
    // other renderers produce (todos_to_ics via icalendar::Calendar).
    let body = jr
        .to_string()
        .replace("BEGIN:VTODO", "BEGIN:VJOURNAL")
        .replace("END:VTODO", "END:VJOURNAL");
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//calendar-server//EN\r\n{body}END:VCALENDAR\r\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row() -> JournalRow {
        JournalRow {
            id: uuid::Uuid::new_v4(),
            calendar_id: uuid::Uuid::new_v4(),
            uid: "j-1".into(),
            href: Some("note.ics".into()),
            starts_at: None,
            start_date: None,
            tzid: None,
            floating: false,
            summary: "Long note".into(),
            description_html: None,
            description_text: Some("text".into()),
            url: None,
            status: Some("FINAL".into()),
            class: None,
            categories: vec![],
            extra_props: serde_json::json!([]),
            sequence: 0,
            etag: String::new(),
            created_by: None,
            deleted_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn journal_renders_vjournal_not_vtodo() {
        let ics = journal_to_ics(&sample_row());
        assert!(ics.starts_with("BEGIN:VCALENDAR"));
        assert!(ics.contains("BEGIN:VJOURNAL"));
        assert!(ics.contains("END:VJOURNAL"));
        assert!(!ics.contains("VTODO"));
        assert!(ics.contains("STATUS:FINAL"));
        assert!(ics.contains("UID:j-1"));
    }

    #[test]
    fn long_values_fold_at_75_octets() {
        let mut row = sample_row();
        row.summary = "w".repeat(200);
        let ics = journal_to_ics(&row);
        for line in ics.lines() {
            // Folded lines are shorter than the 75-octet limit (CRLF not
            // counted); continuations start with a space.
            assert!(
                line.len() <= 76,
                "unfolded line longer than 75 octets: {line}"
            );
        }
        assert!(ics.contains("\r\n SUMMARY") || ics.lines().any(|l| l.starts_with(' ')));
    }
}
