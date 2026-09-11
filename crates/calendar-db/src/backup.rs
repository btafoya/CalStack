//! Portable application backup/restore (docs/PRD.md section 20): a JSON
//! document covering tenants, users, calendars, ACLs and events, with
//! attachments embedded base64. PostgreSQL-native pg_dump remains the
//! full-fidelity path; this is the portable application layer.

type UserRow = (Uuid, String, String, Option<String>, Option<String>, bool);
type CalendarRow = (
    Uuid,
    Uuid,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

use base64::Engine;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::DbError;

/// Exports the whole application state as a JSON document.
pub async fn export(pool: &PgPool) -> Result<Value, DbError> {
    let tenants: Vec<(Uuid, String, String, bool)> =
        sqlx::query_as("SELECT id, slug, name, is_personal FROM tenants")
            .fetch_all(pool)
            .await?;
    let users: Vec<UserRow> = sqlx::query_as(
        "SELECT id, username, email, display_name, password_hash, is_admin FROM users",
    )
    .fetch_all(pool)
    .await?;
    let calendars: Vec<CalendarRow> = sqlx::query_as(
        "SELECT id, tenant_id, slug, name, description, color, timezone
             FROM calendars WHERE deleted_at IS NULL",
    )
    .fetch_all(pool)
    .await?;
    let events: Vec<Value> =
        sqlx::query_as::<_, super::EventRow>("SELECT * FROM events WHERE deleted_at IS NULL")
            .fetch_all(pool)
            .await?
            .iter()
            .map(|e| {
                json!({
                    "id": e.id, "calendar_id": e.calendar_id, "uid": e.uid,
                    "starts_at": e.starts_at, "ends_at": e.ends_at,
                    "start_date": e.start_date, "end_date": e.end_date,
                    "tzid": e.tzid, "all_day": e.all_day, "rrule": e.rrule,
                    "rdate": e.rdate, "exdate": e.exdate, "summary": e.summary,
                    "description_text": e.description_text, "description_html": e.description_html,
                    "status": e.status, "class": e.class, "transp": e.transp,
                    "sequence": e.sequence,
                })
            })
            .collect();
    let attachments: Vec<(Uuid, Uuid, String, String, Vec<u8>)> =
        sqlx::query_as("SELECT id, event_id, filename, content_type, data FROM attachments")
            .fetch_all(pool)
            .await?;

    let tenants_json: Vec<Value> = tenants
        .iter()
        .map(|(id, slug, name, personal)| {
            json!({"id": id, "slug": slug, "name": name, "is_personal": personal})
        })
        .collect();
    let users_json: Vec<Value> = users
        .iter()
        .map(|(id, username, email, display, hash, admin)| {
            json!({
                "id": id, "username": username, "email": email,
                "display_name": display, "password_hash": hash, "is_admin": admin,
            })
        })
        .collect();
    let calendars_json: Vec<Value> = calendars
        .iter()
        .map(|(id, tenant, slug, name, desc, color, tz)| {
            json!({
                "id": id, "tenant_id": tenant, "slug": slug, "name": name,
                "description": desc, "color": color, "timezone": tz,
            })
        })
        .collect();
    let attachments_json: Vec<Value> = attachments
        .iter()
        .map(|(id, event, filename, content_type, data)| {
            json!({
                "id": id, "event_id": event, "filename": filename,
                "content_type": content_type,
                "data": base64::engine::general_purpose::STANDARD.encode(data),
            })
        })
        .collect();
    Ok(json!({
        "format": "calendar-server-backup",
        "version": 1,
        "tenants": tenants_json,
        "users": users_json,
        "calendars": calendars_json,
        "events": events,
        "attachments": attachments_json,
    }))
}

/// Restores into an empty database, keeping original UUIDs.
pub async fn import(pool: &PgPool, document: &Value) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    for user in document["users"].as_array().unwrap_or(&vec![]) {
        sqlx::query(
            "INSERT INTO users (id, username, email, display_name, password_hash, is_admin)
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (id) DO NOTHING",
        )
        .bind(parse_uuid(&user["id"]))
        .bind(user["username"].as_str().unwrap_or_default())
        .bind(user["email"].as_str().unwrap_or_default())
        .bind(user["display_name"].as_str())
        .bind(user["password_hash"].as_str())
        .bind(user["is_admin"].as_bool().unwrap_or(false))
        .execute(&mut *tx)
        .await?;
    }
    for tenant in document["tenants"].as_array().unwrap_or(&vec![]) {
        sqlx::query(
            "INSERT INTO tenants (id, slug, name, is_personal) VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(parse_uuid(&tenant["id"]))
        .bind(tenant["slug"].as_str().unwrap_or_default())
        .bind(tenant["name"].as_str().unwrap_or_default())
        .bind(tenant["is_personal"].as_bool().unwrap_or(false))
        .execute(&mut *tx)
        .await?;
    }
    for calendar in document["calendars"].as_array().unwrap_or(&vec![]) {
        sqlx::query(
            "INSERT INTO calendars (id, tenant_id, slug, name, description, color, timezone)
             VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (id) DO NOTHING",
        )
        .bind(parse_uuid(&calendar["id"]))
        .bind(parse_uuid(&calendar["tenant_id"]))
        .bind(calendar["slug"].as_str().unwrap_or_default())
        .bind(calendar["name"].as_str().unwrap_or_default())
        .bind(calendar["description"].as_str())
        .bind(calendar["color"].as_str())
        .bind(calendar["timezone"].as_str())
        .execute(&mut *tx)
        .await?;
    }
    for event in document["events"].as_array().unwrap_or(&vec![]) {
        sqlx::query(
            "INSERT INTO events (id, calendar_id, uid, summary, sequence)
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (id) DO NOTHING",
        )
        .bind(parse_uuid(&event["id"]))
        .bind(parse_uuid(&event["calendar_id"]))
        .bind(event["uid"].as_str().unwrap_or_default())
        .bind(event["summary"].as_str().unwrap_or_default())
        .bind(event["sequence"].as_i64().unwrap_or(0) as i32)
        .execute(&mut *tx)
        .await?;
    }
    for attachment in document["attachments"].as_array().unwrap_or(&vec![]) {
        let data = base64::engine::general_purpose::STANDARD
            .decode(attachment["data"].as_str().unwrap_or_default())
            .unwrap_or_default();
        sqlx::query(
            "INSERT INTO attachments (id, event_id, filename, content_type, byte_size, sha256, data)
             VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (id) DO NOTHING",
        )
        .bind(parse_uuid(&attachment["id"]))
        .bind(parse_uuid(&attachment["event_id"]))
        .bind(attachment["filename"].as_str().unwrap_or_default())
        .bind(attachment["content_type"].as_str().unwrap_or_default())
        .bind(data.len() as i64)
        .bind(calendar_auth::sha256(&data))
        .bind(&data)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

fn parse_uuid(value: &Value) -> Option<Uuid> {
    value.as_str().and_then(|v| Uuid::parse_str(v).ok())
}
