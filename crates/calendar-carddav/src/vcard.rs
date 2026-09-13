//! Hand-rolled vCard 3.0 parse/serialize (RFC 2426, with the RFC 6350
//! properties CardDAV clients commonly send). Mirrors calendar-caldav's
//! `parse_ics`: only the properties the normalized schema stores are
//! extracted here, but the caller always keeps the verbatim source text
//! (`raw_vcard`) for wire fidelity, so an unrecognized property never loses
//! data — it just doesn't get its own column.
//!
//! A published crate (`calcard`, already a transitive dependency of
//! dav-server's `carddav` feature) could replace this, but its RFC 6350
//! fidelity wasn't something this pass could verify against real clients, so
//! a small parser matching the codebase's existing unfold/fold conventions
//! was the lower-risk choice (docs/CARDDAV_DESIGN.md).

use calendar_db::contacts::{NewContact, NewEmail, NewTel};

#[derive(Debug, thiserror::Error)]
pub enum VCardError {
    #[error("not a VCARD")]
    NotAVCard,
    #[error("VCARD missing UID")]
    MissingUid,
}

/// RFC 2425 unfolding: a line beginning with space or tab continues the
/// previous line (identical rule to RFC 5545; see calendar-caldav::unfold).
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
    out
}

fn unescape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn escape_text(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace(',', "\\,")
        .replace(';', "\\;")
        .replace('\n', "\\n")
}

/// Splits on unescaped ';' (structured field components like N/ADR).
fn split_components(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for c in value.chars() {
        if escaped {
            current.push(c);
            escaped = false;
        } else if c == '\\' {
            current.push(c);
            escaped = true;
        } else if c == ';' {
            parts.push(std::mem::take(&mut current));
        } else {
            current.push(c);
        }
    }
    parts.push(current);
    parts.into_iter().map(|p| unescape_text(&p)).collect()
}

struct Line {
    name: String,
    params: Vec<(String, String)>,
    value: String,
}

fn parse_line(raw: &str) -> Option<Line> {
    let colon = raw.find(':')?;
    let (head, value) = (&raw[..colon], &raw[colon + 1..]);
    let mut segments = head.split(';');
    let name = segments.next()?.trim();
    // Drop a leading "group." prefix (RFC 2425 grouping) if present.
    let name = name.rsplit('.').next().unwrap_or(name).to_uppercase();
    let params = segments
        .filter_map(|seg| {
            let (k, v) = seg.split_once('=')?;
            Some((k.trim().to_uppercase(), v.trim().to_string()))
        })
        .collect();
    Some(Line {
        name,
        params,
        value: value.to_string(),
    })
}

fn param_values<'a>(params: &'a [(String, String)], key: &str) -> Vec<&'a str> {
    params
        .iter()
        .filter(|(k, _)| k == key || k == "TYPE" && key == "TYPE")
        .flat_map(|(_, v)| v.split(','))
        .collect()
}

/// One VCARD (individual or group). uid is always populated: a card that
/// arrives without UID gets one minted so it can be addressed by URL.
pub struct ParsedVCard {
    pub uid: String,
    pub kind: String,
    pub full_name: String,
    pub given_name: Option<String>,
    pub family_name: Option<String>,
    pub org: Option<String>,
    pub title: Option<String>,
    pub street_address: Option<String>,
    pub locality: Option<String>,
    pub region: Option<String>,
    pub postal_code: Option<String>,
    pub country: Option<String>,
    pub emails: Vec<NewEmail>,
    pub tels: Vec<NewTel>,
    pub members: Vec<String>,
}

/// Parses every VCARD component in `text` (a .vcf resource is one card, but
/// this accepts a multi-card export too).
pub fn parse_vcard(text: &str) -> Result<Vec<ParsedVCard>, VCardError> {
    let unfolded = unfold(text);
    let mut cards = Vec::new();
    let mut lines_iter = unfolded.lines().peekable();
    let mut found_any = false;
    while lines_iter.peek().is_some() {
        // Skip to the next BEGIN:VCARD.
        loop {
            match lines_iter.next() {
                Some(l) if l.trim().eq_ignore_ascii_case("BEGIN:VCARD") => break,
                Some(_) => continue,
                None => {
                    if !found_any {
                        return Err(VCardError::NotAVCard);
                    }
                    return Ok(cards);
                }
            }
        }
        found_any = true;
        let mut card = ParsedVCard {
            uid: String::new(),
            kind: "individual".into(),
            full_name: String::new(),
            given_name: None,
            family_name: None,
            org: None,
            title: None,
            street_address: None,
            locality: None,
            region: None,
            postal_code: None,
            country: None,
            emails: Vec::new(),
            tels: Vec::new(),
            members: Vec::new(),
        };
        for raw in lines_iter.by_ref() {
            if raw.trim().eq_ignore_ascii_case("END:VCARD") {
                break;
            }
            let Some(line) = parse_line(raw) else {
                continue;
            };
            match line.name.as_str() {
                "UID" => card.uid = unescape_text(line.value.trim()),
                "KIND" | "X-ADDRESSBOOKSERVER-KIND" => {
                    card.kind = if line.value.trim().eq_ignore_ascii_case("group") {
                        "group".into()
                    } else {
                        "individual".into()
                    };
                }
                "FN" => card.full_name = unescape_text(line.value.trim()),
                "N" => {
                    let parts = split_components(&line.value);
                    card.family_name = parts.first().filter(|s| !s.is_empty()).cloned();
                    card.given_name = parts.get(1).filter(|s| !s.is_empty()).cloned();
                }
                "ORG" => card.org = Some(unescape_text(line.value.trim())),
                "TITLE" => card.title = Some(unescape_text(line.value.trim())),
                "ADR" => {
                    let parts = split_components(&line.value);
                    card.street_address = parts.get(2).filter(|s| !s.is_empty()).cloned();
                    card.locality = parts.get(3).filter(|s| !s.is_empty()).cloned();
                    card.region = parts.get(4).filter(|s| !s.is_empty()).cloned();
                    card.postal_code = parts.get(5).filter(|s| !s.is_empty()).cloned();
                    card.country = parts.get(6).filter(|s| !s.is_empty()).cloned();
                }
                "EMAIL" => {
                    let types = param_values(&line.params, "TYPE");
                    card.emails.push(NewEmail {
                        email: unescape_text(line.value.trim()),
                        kind: types.first().map(|s| s.to_lowercase()),
                        is_primary: types.iter().any(|t| t.eq_ignore_ascii_case("pref"))
                            || card.emails.is_empty(),
                    });
                }
                "TEL" => {
                    let types = param_values(&line.params, "TYPE");
                    let is_mobile = types.iter().any(|t| {
                        t.eq_ignore_ascii_case("cell") || t.eq_ignore_ascii_case("mobile")
                    });
                    card.tels.push(NewTel {
                        number: unescape_text(line.value.trim()),
                        kind: types
                            .iter()
                            .find(|t| !t.eq_ignore_ascii_case("pref"))
                            .map(|s| s.to_lowercase()),
                        is_mobile,
                        is_primary: types.iter().any(|t| t.eq_ignore_ascii_case("pref"))
                            || card.tels.is_empty(),
                    });
                }
                "MEMBER" => card.members.push(unescape_text(line.value.trim())),
                _ => {} // preserved via raw_vcard, not otherwise modeled
            }
        }
        if card.uid.is_empty() {
            card.uid = uuid::Uuid::new_v4().to_string();
        }
        if card.full_name.is_empty() {
            card.full_name = card
                .given_name
                .iter()
                .chain(card.family_name.iter())
                .cloned()
                .collect::<Vec<_>>()
                .join(" ");
        }
        cards.push(card);
    }
    if !found_any {
        return Err(VCardError::NotAVCard);
    }
    Ok(cards)
}

impl ParsedVCard {
    pub fn into_new_contact(self, raw_vcard: String) -> Result<NewContact, VCardError> {
        if self.uid.is_empty() {
            return Err(VCardError::MissingUid);
        }
        Ok(NewContact {
            uid: self.uid,
            kind: self.kind,
            full_name: self.full_name,
            given_name: self.given_name,
            family_name: self.family_name,
            org: self.org,
            title: self.title,
            street_address: self.street_address,
            locality: self.locality,
            region: self.region,
            postal_code: self.postal_code,
            country: self.country,
            raw_vcard,
            emails: self.emails,
            tels: self.tels,
            group_members: self.members,
        })
    }
}

/// Generates a vCard 3.0 text from normalized fields — used for API/UI-created
/// contacts and the virtual directory projection. Clients that PUT their own
/// card get their raw text stored verbatim instead (see calendar_db::contacts).
#[allow(clippy::too_many_arguments)]
pub fn write_vcard_3_0(
    uid: &str,
    kind: &str,
    full_name: &str,
    given_name: Option<&str>,
    family_name: Option<&str>,
    org: Option<&str>,
    title: Option<&str>,
    emails: &[NewEmail],
    tels: &[NewTel],
    members: &[String],
) -> String {
    let mut out = String::from("BEGIN:VCARD\r\nVERSION:3.0\r\n");
    out.push_str(&format!("UID:{}\r\n", escape_text(uid)));
    if kind == "group" {
        out.push_str("KIND:group\r\n");
    }
    out.push_str(&format!("FN:{}\r\n", escape_text(full_name)));
    out.push_str(&format!(
        "N:{};{};;;\r\n",
        escape_text(family_name.unwrap_or("")),
        escape_text(given_name.unwrap_or(""))
    ));
    if let Some(org) = org {
        out.push_str(&format!("ORG:{}\r\n", escape_text(org)));
    }
    if let Some(title) = title {
        out.push_str(&format!("TITLE:{}\r\n", escape_text(title)));
    }
    for e in emails {
        let pref = if e.is_primary { ";TYPE=pref" } else { "" };
        let kind = e
            .kind
            .as_deref()
            .map(|k| format!(";TYPE={k}"))
            .unwrap_or_default();
        out.push_str(&format!("EMAIL{kind}{pref}:{}\r\n", escape_text(&e.email)));
    }
    for t in tels {
        let mut types = Vec::new();
        if t.is_mobile {
            types.push("CELL");
        }
        if let Some(k) = &t.kind {
            types.push(k.as_str());
        }
        if t.is_primary {
            types.push("pref");
        }
        let type_param = if types.is_empty() {
            String::new()
        } else {
            format!(";TYPE={}", types.join(","))
        };
        out.push_str(&format!("TEL{type_param}:{}\r\n", escape_text(&t.number)));
    }
    for member in members {
        out.push_str(&format!("MEMBER:{}\r\n", escape_text(member)));
    }
    out.push_str("END:VCARD\r\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:abc-1\r\nFN:Al Jones\r\n\
N:Jones;Al;;;\r\nORG:Acme\r\nEMAIL;TYPE=work,pref:al@example.com\r\n\
TEL;TYPE=CELL:+15551234567\r\nEND:VCARD\r\n";

    #[test]
    fn parses_core_fields() {
        let cards = parse_vcard(SAMPLE).unwrap();
        assert_eq!(cards.len(), 1);
        let c = &cards[0];
        assert_eq!(c.uid, "abc-1");
        assert_eq!(c.full_name, "Al Jones");
        assert_eq!(c.given_name.as_deref(), Some("Al"));
        assert_eq!(c.family_name.as_deref(), Some("Jones"));
        assert_eq!(c.org.as_deref(), Some("Acme"));
        assert_eq!(c.emails.len(), 1);
        assert_eq!(c.emails[0].email, "al@example.com");
        assert!(c.emails[0].is_primary);
        assert_eq!(c.tels.len(), 1);
        assert!(c.tels[0].is_mobile);
    }

    #[test]
    fn missing_uid_gets_generated() {
        let text = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:No Uid\r\nEND:VCARD\r\n";
        let cards = parse_vcard(text).unwrap();
        assert!(!cards[0].uid.is_empty());
    }

    #[test]
    fn not_a_vcard_errors() {
        assert!(matches!(parse_vcard("hello"), Err(VCardError::NotAVCard)));
    }

    #[test]
    fn write_then_parse_round_trip() {
        let text = write_vcard_3_0(
            "u1",
            "individual",
            "Jane Doe",
            Some("Jane"),
            Some("Doe"),
            Some("Acme"),
            None,
            &[NewEmail {
                email: "jane@example.com".into(),
                kind: Some("work".into()),
                is_primary: true,
            }],
            &[NewTel {
                number: "+15550000000".into(),
                kind: None,
                is_mobile: true,
                is_primary: true,
            }],
            &[],
        );
        let cards = parse_vcard(&text).unwrap();
        assert_eq!(cards[0].uid, "u1");
        assert_eq!(cards[0].full_name, "Jane Doe");
        assert_eq!(cards[0].emails[0].email, "jane@example.com");
        assert!(cards[0].tels[0].is_mobile);
    }

    #[test]
    fn write_group_members_round_trip() {
        let text = write_vcard_3_0(
            "g1",
            "group",
            "Team",
            None,
            None,
            None,
            None,
            &[],
            &[],
            &["urn:uuid:member-1".to_string()],
        );
        let cards = parse_vcard(&text).unwrap();
        assert_eq!(cards[0].kind, "group");
        assert_eq!(cards[0].members, vec!["urn:uuid:member-1"]);
    }

    #[test]
    fn group_members_round_trip() {
        let text = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:g1\r\nKIND:group\r\nFN:Team\r\n\
MEMBER:urn:uuid:member-1\r\nEND:VCARD\r\n";
        let cards = parse_vcard(text).unwrap();
        assert_eq!(cards[0].kind, "group");
        assert_eq!(cards[0].members, vec!["urn:uuid:member-1"]);
    }
}
