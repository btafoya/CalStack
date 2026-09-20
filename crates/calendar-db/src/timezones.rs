//! Client-supplied VTIMEZONEs (ADR-012). Parsed and compiled by
//! calendar-caldav at PUT; this module stores the raw definition (re-emitted
//! on export) and the compiled STANDARD/DAYLIGHT rules expansion resolves
//! unknown tzids from.

use chrono::{Duration, Utc};
use uuid::Uuid;

use super::DbError;

/// A VTIMEZONE to store: raw definition text plus its compiled rules.
#[derive(Debug, Clone)]
pub struct NewTimezone {
    pub tzid: String,
    /// Raw VTIMEZONE component text (unfolded), re-emitted on export.
    pub definition: String,
    pub rules: Vec<calendar_core::recurrence::ZoneRule>,
}

/// One stored VTIMEZONE.
#[derive(Debug, Clone)]
pub struct StoredTimezone {
    pub tzid: String,
    pub definition: String,
    pub rules: Vec<calendar_core::recurrence::ZoneRule>,
}

/// Upserts the PUT's VTIMEZONEs for the calendar. Never deletes: a later PUT
/// without a VTIMEZONE must not drop zones earlier resources supplied.
pub async fn upsert_for_calendar(
    tx: &mut sqlx::PgConnection,
    calendar_id: Uuid,
    zones: &[NewTimezone],
) -> Result<(), DbError> {
    for zone in zones {
        sqlx::query(
            "INSERT INTO timezones (id, calendar_id, tzid, definition, rules)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (calendar_id, tzid) DO UPDATE
             SET definition = EXCLUDED.definition, rules = EXCLUDED.rules, updated_at = now()",
        )
        .bind(Uuid::new_v4())
        .bind(calendar_id)
        .bind(&zone.tzid)
        .bind(&zone.definition)
        .bind(serde_json::to_value(&zone.rules)?)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// The calendar's stored VTIMEZONEs (empty when it has none).
pub async fn list_for_calendar(
    pool: &sqlx::PgPool,
    calendar_id: Uuid,
) -> Result<Vec<StoredTimezone>, DbError> {
    let rows: Vec<(String, String, serde_json::Value)> =
        sqlx::query_as("SELECT tzid, definition, rules FROM timezones WHERE calendar_id = $1")
            .bind(calendar_id)
            .fetch_all(pool)
            .await?;
    rows.into_iter()
        .map(|(tzid, definition, rules)| {
            Ok(StoredTimezone {
                tzid,
                definition,
                rules: serde_json::from_value(rules)?,
            })
        })
        .collect()
}

/// The calendar's stored VTIMEZONEs as a tzid → offset-transitions resolver
/// for recurrence/alarm/free-busy expansion. Transitions are compiled over
/// ±[`calendar_core::recurrence::ZONE_WINDOW_YEARS`] around load time.
pub async fn load_for_calendar(
    pool: &sqlx::PgPool,
    calendar_id: Uuid,
) -> Result<calendar_core::recurrence::TzResolver, DbError> {
    let now = Utc::now();
    let window = (
        now - Duration::days(366 * calendar_core::recurrence::ZONE_WINDOW_YEARS),
        now + Duration::days(366 * calendar_core::recurrence::ZONE_WINDOW_YEARS),
    );
    let mut resolver = calendar_core::recurrence::TzResolver::default();
    for zone in list_for_calendar(pool, calendar_id).await? {
        // Zones are validated compilable at PUT; a rule that no longer
        // compiles is skipped rather than failing every expansion.
        if let Ok(transitions) =
            calendar_core::recurrence::compile_zone(&zone.rules, window.0, window.1)
        {
            resolver.insert(zone.tzid, transitions);
        }
    }
    Ok(resolver)
}

impl From<serde_json::Error> for DbError {
    fn from(e: serde_json::Error) -> Self {
        DbError::Conflict(format!("malformed VTIMEZONE rules: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use calendar_core::recurrence::{ZoneRule, expand_occurrences};
    use chrono::{DateTime, TimeZone};
    use sqlx::postgres::PgPoolOptions;

    /// DB-backed tests need a live PostgreSQL via DATABASE_URL (the throwaway
    /// instance the interop suite boots works). Without it they skip so
    /// `cargo test` still passes on machines without infrastructure.
    async fn test_pool() -> Option<sqlx::PgPool> {
        let url = std::env::var("DATABASE_URL")
            .ok()
            .filter(|u| !u.is_empty())?;
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .ok()?;
        crate::migrate(&pool).await.ok()?;
        Some(pool)
    }

    async fn fixture(pool: &sqlx::PgPool) -> (Uuid, Uuid) {
        let user = Uuid::new_v4();
        let calendar = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, username, email) VALUES ($1, $2, $3)")
            .bind(user)
            .bind(format!("u-{}", user.simple()))
            .bind(format!("{}@timezones.test", user))
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO tenants (id, slug, name, is_personal) VALUES ($1, $2, $2, true)")
            .bind(user)
            .bind(user.simple().to_string())
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO tenant_members (tenant_id, user_id, role) VALUES ($1, $2, 'owner')",
        )
        .bind(user)
        .bind(user)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO calendars (id, tenant_id, slug, name, created_by) VALUES ($1, $2, $3, $3, $4)")
            .bind(calendar)
            .bind(user)
            .bind(user.simple().to_string())
            .bind(user)
            .execute(pool)
            .await
            .unwrap();
        (user, calendar)
    }

    /// MST(-7)/MDT(-6): DST starts the second Sunday of March, ends the first
    /// Sunday of November.
    fn zone_rules() -> Vec<ZoneRule> {
        let parse = |s: &str| chrono::NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%S").unwrap();
        vec![
            ZoneRule {
                dtstart: parse("19700308T020000"),
                offset_from_secs: -7 * 3600,
                offset_to_secs: -6 * 3600,
                rrule: Some("FREQ=YEARLY;BYMONTH=3;BYDAY=2SU".into()),
                rdates: vec![],
            },
            ZoneRule {
                dtstart: parse("19701101T020000"),
                offset_from_secs: -6 * 3600,
                offset_to_secs: -7 * 3600,
                rrule: Some("FREQ=YEARLY;BYMONTH=11;BYDAY=1SU".into()),
                rdates: vec![],
            },
        ]
    }

    fn at(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    #[tokio::test]
    async fn put_series_stores_zone_and_resolver_expands_across_dst() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let (user, calendar) = fixture(&pool).await;
        let zones = vec![NewTimezone {
            tzid: "Custom/Test".into(),
            definition: "BEGIN:VTIMEZONE\r\nTZID:Custom/Test\r\nEND:VTIMEZONE\r\n".into(),
            rules: zone_rules(),
        }];
        // Daily 09:00 wall clock event spanning the 2026 spring-forward.
        let event = super::super::ics_upsert::IcsEventUpsert {
            uid: Uuid::new_v4().to_string(),
            starts_at: Some(at(2026, 3, 5, 16)),
            ends_at: Some(at(2026, 3, 5, 17)),
            tzid: Some("Custom/Test".into()),
            rrule: Some("FREQ=DAILY".into()),
            summary: Some("Zone test".into()),
            organizer_email: "writer@timezones.test".into(),
            ..Default::default()
        };
        let (row, created) = super::super::ics_upsert::put_series(
            &pool,
            calendar,
            user,
            "zone.ics",
            &event,
            &[],
            &zones,
            &super::super::ics_upsert::PutPrecondition::None,
        )
        .await
        .unwrap();
        assert!(created);
        assert_eq!(row.tzid.as_deref(), Some("Custom/Test"));

        // Store/load round trip: the resolver carries the zone with compiled
        // transitions.
        let resolver = load_for_calendar(&pool, calendar).await.unwrap();
        let transitions = resolver.get("Custom/Test").unwrap();
        // 2026-03-08 02:00 MST → 09:00Z switches to -6h.
        assert!(transitions.contains(&(at(2026, 3, 8, 9).timestamp(), -6 * 3600)));

        // Expansion through the resolver: 09:00 wall stays 09:00 wall.
        let out = expand_occurrences(
            calendar_core::DateOrDateTime::Timed(at(2026, 3, 5, 16)),
            Some("Custom/Test"),
            Some(&resolver),
            Some("FREQ=DAILY"),
            &[],
            &[],
            at(2026, 3, 5, 0),
            at(2026, 3, 12, 0),
        )
        .unwrap();
        let hours: Vec<u32> = out
            .iter()
            .map(|p| match p {
                calendar_core::DateOrDateTime::Timed(t) => chrono::Timelike::hour(t),
                _ => panic!("timed expected"),
            })
            .collect();
        assert_eq!(hours, vec![16, 16, 16, 15, 15, 15, 15]);

        // Re-PUT the same resource: the zone upserts without conflict.
        super::super::ics_upsert::put_series(
            &pool,
            calendar,
            user,
            "zone.ics",
            &event,
            &[],
            &zones,
            &super::super::ics_upsert::PutPrecondition::None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn put_series_rejects_unknown_tzid_without_zone() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let (user, calendar) = fixture(&pool).await;
        let event = super::super::ics_upsert::IcsEventUpsert {
            uid: Uuid::new_v4().to_string(),
            starts_at: Some(at(2026, 3, 5, 16)),
            tzid: Some("Never/Stored".into()),
            organizer_email: "writer@timezones.test".into(),
            ..Default::default()
        };
        let err = super::super::ics_upsert::put_series(
            &pool,
            calendar,
            user,
            "bad.ics",
            &event,
            &[],
            &[],
            &super::super::ics_upsert::PutPrecondition::None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)), "got {err:?}");
        // Nothing was stored that could later expand as UTC.
        assert!(
            load_for_calendar(&pool, calendar)
                .await
                .unwrap()
                .get("Never/Stored")
                .is_none()
        );
    }

    #[tokio::test]
    async fn zone_from_earlier_put_allows_later_event() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let (user, calendar) = fixture(&pool).await;
        let zones = vec![NewTimezone {
            tzid: "Custom/Other".into(),
            definition: "BEGIN:VTIMEZONE\r\nTZID:Custom/Other\r\nEND:VTIMEZONE\r\n".into(),
            rules: zone_rules(),
        }];
        let plain = super::super::ics_upsert::IcsEventUpsert {
            uid: Uuid::new_v4().to_string(),
            starts_at: Some(at(2026, 3, 5, 16)),
            ends_at: Some(at(2026, 3, 5, 17)),
            organizer_email: "writer@timezones.test".into(),
            ..Default::default()
        };
        // First PUT carries the VTIMEZONE.
        super::super::ics_upsert::put_series(
            &pool,
            calendar,
            user,
            "z.ics",
            &plain,
            &[],
            &zones,
            &super::super::ics_upsert::PutPrecondition::None,
        )
        .await
        .unwrap();
        // A later PUT referencing the zone without re-sending it is accepted.
        let mut later = super::super::ics_upsert::IcsEventUpsert {
            uid: Uuid::new_v4().to_string(),
            starts_at: Some(at(2026, 3, 5, 16)),
            ends_at: Some(at(2026, 3, 5, 17)),
            tzid: Some("Custom/Other".into()),
            organizer_email: "writer@timezones.test".into(),
            ..Default::default()
        };
        later.uid = Uuid::new_v4().to_string();
        super::super::ics_upsert::put_series(
            &pool,
            calendar,
            user,
            "y.ics",
            &later,
            &[],
            &[],
            &super::super::ics_upsert::PutPrecondition::None,
        )
        .await
        .unwrap();
    }
}
