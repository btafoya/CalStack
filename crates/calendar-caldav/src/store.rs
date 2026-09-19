//! Mapping from parsed iCalendar wire data to the storage upsert model, plus
//! CalDAV REPORT helpers that dav-server does not provide (sync-collection).

use calendar_db::ics_upsert::{IcsAttendee, IcsEventUpsert};

/// ParsedEvent (wire) → storage record for the PUT path.
pub(crate) fn upsert_data(parsed: &crate::ParsedEvent) -> IcsEventUpsert {
    IcsEventUpsert {
        uid: parsed.uid.clone(),
        starts_at: parsed.starts_at,
        ends_at: parsed.ends_at,
        start_date: parsed.start_date,
        end_date: parsed.end_date,
        duration_secs: parsed.duration_secs,
        tzid: parsed.tzid.clone(),
        all_day: parsed.all_day,
        floating: parsed.floating,
        rrule: parsed.rrule.clone(),
        rdate: points_to_json(&parsed.rdate),
        exdate: points_to_json(&parsed.exdate),
        summary: parsed.summary.clone(),
        description_html: parsed
            .description_html
            .as_deref()
            .map(calendar_core::sanitize_html),
        description_text: parsed.description_text.clone(),
        url: parsed.url.clone(),
        location_text: parsed.location_text.clone(),
        status: parsed.status.clone(),
        priority: parsed.priority,
        class: parsed.class.clone(),
        transp: parsed.transp.clone(),
        categories: parsed.categories.clone(),
        sequence: parsed.sequence,
        recurrence_id: parsed.recurrence_id,
        recurrence_id_date: parsed.recurrence_id_date,
        organizer_email: parsed.organizer_email.clone().unwrap_or_default(),
        organizer_name: parsed.organizer_name.clone(),
        organizer_user_id: None, // set by the PUT path, which knows the writer
        alarms: parsed
            .alarms
            .iter()
            .map(|a| calendar_db::alarms::NewAlarm {
                action: a.action.clone(),
                related: a.related.clone(),
                offset_secs: a.offset_secs,
                trigger_at: a.trigger_at,
                description: a.description.clone(),
                summary: a.summary.clone(),
                recipient_emails: a.recipients.clone(),
            })
            .collect(),
        attendees: parsed
            .attendees
            .iter()
            .map(|a| IcsAttendee {
                email: a.email.clone(),
                display_name: a.display_name.clone(),
                role: a.role.clone(),
                partstat: a.partstat.clone(),
                rsvp: a.rsvp,
            })
            .collect(),
    }
}

fn points_to_json(points: &[calendar_core::DateOrDateTime]) -> serde_json::Value {
    let list: Vec<serde_json::Value> = points
        .iter()
        .map(|p| match p {
            calendar_core::DateOrDateTime::Timed(at) => serde_json::json!(at.to_rfc3339()),
            calendar_core::DateOrDateTime::AllDay(date) => serde_json::json!(date.to_string()),
        })
        .collect();
    serde_json::Value::Array(list)
}

// ============ sync-collection (RFC 6578) ============

/// One changed resource reported by a sync-collection REPORT.
#[derive(Debug, Clone)]
pub struct SyncChange {
    pub href_suffix: String, // "{resource-uuid}.ics" relative to the collection
    pub etag: Option<String>,
    pub deleted: bool,
}

/// Builds the RFC 6578 multistatus response. `sync_token` is the change_log
/// sequence the client should store.
pub fn sync_collection_xml(
    collection_href: &str,
    changes: &[SyncChange],
    sync_token: i64,
) -> String {
    let mut out = String::from(
        r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">"#,
    );
    let base = collection_href.trim_end_matches('/');
    for change in changes {
        out.push_str("<D:response>");
        out.push_str(&format!(
            "<D:href>{}/{}</D:href>",
            xml_escape(base),
            xml_escape(&percent_encode_segment(&change.href_suffix))
        ));
        if change.deleted {
            out.push_str("<D:status>HTTP/1.1 404 Not Found</D:status>");
        } else {
            out.push_str("<D:propstat><D:prop>");
            if let Some(etag) = &change.etag {
                out.push_str(&format!("<D:getetag>{}</D:getetag>", xml_escape(etag)));
            }
            out.push_str("</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>");
        }
        out.push_str("</D:response>");
    }
    out.push_str(&format!(
        "<D:sync-token>{sync_token}</D:sync-token></D:multistatus>"
    ));
    out
}

/// One URL path segment: everything but unreserved characters is %XX-encoded.
fn percent_encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hrefs_are_percent_encoded() {
        let xml = sync_collection_xml(
            "/calendars/alice/work",
            &[SyncChange {
                href_suffix: "my task@x y.ics".into(),
                etag: None,
                deleted: true,
            }],
            1,
        );
        assert!(xml.contains("/calendars/alice/work/my%20task%40x%20y.ics"));
    }

    #[test]
    fn sync_xml_shape() {
        let xml = sync_collection_xml(
            "/calendars/alice/work/",
            &[
                SyncChange {
                    href_suffix: "abc.ics".into(),
                    etag: Some("\"1\"".into()),
                    deleted: false,
                },
                SyncChange {
                    href_suffix: "def.ics".into(),
                    etag: None,
                    deleted: true,
                },
            ],
            42,
        );
        assert!(xml.contains("<D:sync-token>42</D:sync-token>"));
        assert!(xml.contains("/calendars/alice/work/abc.ics"));
        assert!(xml.contains("HTTP/1.1 404 Not Found"));
        assert!(xml.contains("<D:getetag>\"1\"</D:getetag>"));
    }
}
