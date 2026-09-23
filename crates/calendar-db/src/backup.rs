//! Portable application backup/restore (docs/PRD.md section 20): a JSON
//! document covering tenants, users, memberships, calendars, ACLs, locations,
//! events (masters and RECURRENCE-ID exceptions with their real organizer),
//! tasks (same shape) and journals, attendees and alarms for both,
//! attachments, public shares, subscriptions, rules,
//! notification providers (encrypted config as-is, never decrypted),
//! notifications, the category registry, CardDAV address books and contacts,
//! push subscriptions, and the user reminder opt-out columns.
//!
//! Every table is exported and restored through one table-driven spec: the
//! same column list drives the export query (as a single `json_agg`) and the
//! restore insert (via `jsonb_populate_record`, which casts each JSON value to
//! the column's type: timestamps, intervals, bytea (hex), arrays and jsonb all
//! round-trip). PostgreSQL-native pg_dump remains the full-fidelity path; this
//! is the portable application layer.

use serde_json::{Value, json};
use sqlx::PgPool;

use super::DbError;

/// Backup format version. Version 2 added the tables the v1 export dropped;
/// the importer still accepts v1 documents (missing keys are filled from
/// per-column defaults or NULL).
const FORMAT_VERSION: u32 = 2;

/// JSON stand-in for a column default, applied on import when the document
/// omits the key (e.g. v1 backups predating the column) or has it null.
enum JsonDefault {
    Bool(bool),
    Int(i64),
    Str(&'static str),
    List(&'static [&'static str]),
}

impl JsonDefault {
    fn to_value(&self) -> Value {
        match self {
            Self::Bool(b) => Value::Bool(*b),
            Self::Int(i) => Value::from(*i),
            Self::Str(s) => Value::from(*s),
            Self::List(items) => Value::Array(items.iter().map(|s| Value::from(*s)).collect()),
        }
    }
}

/// One dumped/restored table. Export and import share `columns`: export
/// selects exactly those columns; import inserts exactly those columns.
struct TableSpec {
    /// JSON envelope key; also the table name.
    table: &'static str,
    columns: &'static [&'static str],
    /// Optional WHERE clause applied on export only.
    filter: &'static str,
    /// Column defaults for documents that predate the column.
    defaults: &'static [(&'static str, JsonDefault)],
}

/// In restore (and export) dependency order: parents before children. Contacts
/// precede events because event_attendees.contact_id references contacts.
const TABLES: &[TableSpec] = &[
    TableSpec {
        table: "users",
        columns: &[
            "id",
            "username",
            "email",
            "display_name",
            "password_hash",
            "is_admin",
            "timezone",
            "disabled_at",
            "notify_email",
            "notify_sms",
            "notify_push",
        ],
        filter: "",
        defaults: &[
            ("notify_email", JsonDefault::Bool(true)),
            ("notify_sms", JsonDefault::Bool(true)),
            ("notify_push", JsonDefault::Bool(true)),
        ],
    },
    TableSpec {
        table: "tenants",
        columns: &["id", "slug", "name", "is_personal"],
        filter: "",
        defaults: &[("is_personal", JsonDefault::Bool(false))],
    },
    TableSpec {
        table: "tenant_members",
        columns: &["tenant_id", "user_id", "role"],
        filter: "",
        defaults: &[],
    },
    TableSpec {
        table: "calendars",
        columns: &[
            "id",
            "tenant_id",
            "slug",
            "name",
            "description",
            "color",
            "timezone",
            "order_index",
            "components",
            "created_by",
            "source_url",
            "source_etag",
            "source_synced_at",
        ],
        filter: "WHERE deleted_at IS NULL",
        defaults: &[
            ("order_index", JsonDefault::Int(0)),
            ("components", JsonDefault::List(&["VEVENT"])),
        ],
    },
    TableSpec {
        table: "locations",
        columns: &[
            "id",
            "provider",
            "provider_place_id",
            "display_name",
            "formatted_address",
            "street_address",
            "locality",
            "administrative_area",
            "postal_code",
            "country",
            "latitude",
            "longitude",
            "website",
            "phone",
            "provider_metadata",
        ],
        filter: "",
        defaults: &[],
    },
    TableSpec {
        table: "address_books",
        columns: &["id", "tenant_id", "owner_user_id", "slug", "name"],
        filter: "WHERE deleted_at IS NULL",
        defaults: &[],
    },
    TableSpec {
        table: "contacts",
        columns: &[
            "id",
            "address_book_id",
            "uid",
            "kind",
            "full_name",
            "given_name",
            "family_name",
            "org",
            "title",
            "street_address",
            "locality",
            "region",
            "postal_code",
            "country",
            "photo",
            "photo_mime",
            "raw_vcard",
            "etag",
        ],
        filter: "WHERE address_book_id IN (SELECT id FROM address_books WHERE deleted_at IS NULL)",
        defaults: &[
            ("kind", JsonDefault::Str("individual")),
            ("full_name", JsonDefault::Str("")),
            ("etag", JsonDefault::Str("")),
        ],
    },
    TableSpec {
        table: "contact_emails",
        columns: &["id", "contact_id", "email", "kind", "is_primary"],
        filter: "",
        defaults: &[("is_primary", JsonDefault::Bool(false))],
    },
    TableSpec {
        table: "contact_tels",
        columns: &[
            "id",
            "contact_id",
            "number",
            "kind",
            "is_mobile",
            "is_primary",
        ],
        filter: "",
        defaults: &[
            ("is_mobile", JsonDefault::Bool(false)),
            ("is_primary", JsonDefault::Bool(false)),
        ],
    },
    TableSpec {
        table: "contact_group_members",
        columns: &[
            "group_contact_id",
            "raw_member",
            "member_contact_id",
            "member_user_id",
        ],
        filter: "",
        defaults: &[],
    },
    // Masters and RECURRENCE-ID exceptions share the events table; the
    // importer inserts masters first so master_event_id FKs resolve.
    TableSpec {
        table: "events",
        columns: &[
            "id",
            "calendar_id",
            "uid",
            "master_event_id",
            "recurrence_id",
            "recurrence_id_date",
            "starts_at",
            "ends_at",
            "start_date",
            "end_date",
            "duration",
            "tzid",
            "all_day",
            "floating",
            "rrule",
            "rdate",
            "exdate",
            "summary",
            "description_html",
            "description_text",
            "url",
            "status",
            "priority",
            "class",
            "transp",
            "categories",
            "location_id",
            "organizer_user_id",
            "organizer_email",
            "organizer_name",
            "sequence",
            "etag",
            "created_by",
            "href",
        ],
        filter: "WHERE deleted_at IS NULL",
        defaults: &[
            ("all_day", JsonDefault::Bool(false)),
            ("floating", JsonDefault::Bool(false)),
            ("categories", JsonDefault::List(&[])),
            ("rdate", JsonDefault::List(&[])),
            ("exdate", JsonDefault::List(&[])),
            ("summary", JsonDefault::Str("")),
            ("sequence", JsonDefault::Int(0)),
            ("etag", JsonDefault::Str("")),
            // v1 documents had no organizer at all; keep the old stand-in.
            ("organizer_email", JsonDefault::Str("restored@localhost")),
        ],
    },
    TableSpec {
        table: "event_attendees",
        columns: &[
            "id",
            "event_id",
            "user_id",
            "email",
            "display_name",
            "telephone",
            "role",
            "partstat",
            "rsvp",
            "schedule_status",
            "contact_id",
        ],
        filter: "WHERE event_id IN (SELECT id FROM events WHERE deleted_at IS NULL)",
        defaults: &[
            ("role", JsonDefault::Str("REQ-PARTICIPANT")),
            ("partstat", JsonDefault::Str("NEEDS-ACTION")),
        ],
    },
    TableSpec {
        table: "event_alarms",
        columns: &[
            "id",
            "event_id",
            "action",
            "related",
            "offset_interval",
            "trigger_at",
            "description",
            "summary",
            "recipient_emails",
            "notify_channels",
        ],
        filter: "WHERE event_id IN (SELECT id FROM events WHERE deleted_at IS NULL)",
        defaults: &[(
            "notify_channels",
            JsonDefault::List(&["in_app", "email", "sms", "push"]),
        )],
    },
    TableSpec {
        table: "attachments",
        columns: &[
            "id",
            "event_id",
            "filename",
            "content_type",
            "byte_size",
            "sha256",
            "data",
            "created_at",
        ],
        filter: "WHERE event_id IN (SELECT id FROM events WHERE deleted_at IS NULL)",
        defaults: &[],
    },
    // Masters and RECURRENCE-ID exceptions share the tasks table; the importer
    // inserts masters first so master_task_id FKs resolve. origin_id (the
    // delivered-copy link, like events) is deliberately not carried: it is a
    // scheduling-runtime pointer, not user data.
    TableSpec {
        table: "tasks",
        columns: &[
            "id",
            "calendar_id",
            "uid",
            "master_task_id",
            "recurrence_id",
            "recurrence_id_date",
            "starts_at",
            "start_date",
            "due_at",
            "due_date",
            "duration",
            "tzid",
            "floating",
            "completed_at",
            "rrule",
            "rdate",
            "exdate",
            "summary",
            "description_html",
            "description_text",
            "url",
            "location",
            "status",
            "percent_complete",
            "priority",
            "class",
            "categories",
            "parent_uid",
            "sort_order",
            "extra_props",
            "organizer_user_id",
            "organizer_email",
            "organizer_name",
            "sequence",
            "etag",
            "created_by",
            "href",
        ],
        filter: "WHERE deleted_at IS NULL",
        defaults: &[
            ("floating", JsonDefault::Bool(false)),
            ("categories", JsonDefault::List(&[])),
            ("rdate", JsonDefault::List(&[])),
            ("exdate", JsonDefault::List(&[])),
            ("summary", JsonDefault::Str("")),
            ("sequence", JsonDefault::Int(0)),
            ("etag", JsonDefault::Str("")),
        ],
    },
    TableSpec {
        table: "task_attendees",
        columns: &[
            "id",
            "task_id",
            "user_id",
            "contact_id",
            "email",
            "display_name",
            "telephone",
            "role",
            "partstat",
            "rsvp",
            "schedule_status",
        ],
        filter: "WHERE task_id IN (SELECT id FROM tasks WHERE deleted_at IS NULL)",
        defaults: &[
            ("role", JsonDefault::Str("REQ-PARTICIPANT")),
            ("partstat", JsonDefault::Str("NEEDS-ACTION")),
        ],
    },
    TableSpec {
        table: "task_alarms",
        columns: &[
            "id",
            "task_id",
            "action",
            "related",
            "offset_interval",
            "trigger_at",
            "description",
            "summary",
            "recipient_emails",
            "notify_channels",
        ],
        filter: "WHERE task_id IN (SELECT id FROM tasks WHERE deleted_at IS NULL)",
        defaults: &[(
            "notify_channels",
            JsonDefault::List(&["in_app", "email", "sms", "push"]),
        )],
    },
    // Journals: one row per resource; the floating/undated columns ride along
    // (an undated journal is a note with no DTSTART at all).
    TableSpec {
        table: "journals",
        columns: &[
            "id",
            "calendar_id",
            "uid",
            "href",
            "starts_at",
            "start_date",
            "tzid",
            "floating",
            "summary",
            "description_html",
            "description_text",
            "url",
            "status",
            "class",
            "categories",
            "extra_props",
            "sequence",
            "etag",
            "created_by",
        ],
        filter: "WHERE deleted_at IS NULL",
        defaults: &[
            ("floating", JsonDefault::Bool(false)),
            ("categories", JsonDefault::List(&[])),
            ("summary", JsonDefault::Str("")),
            ("sequence", JsonDefault::Int(0)),
            ("etag", JsonDefault::Str("")),
        ],
    },
    TableSpec {
        table: "calendar_acl",
        columns: &[
            "calendar_id",
            "principal_user_id",
            "capability",
            "can_manage_acl",
        ],
        filter: "",
        defaults: &[("can_manage_acl", JsonDefault::Bool(false))],
    },
    TableSpec {
        table: "public_shares",
        columns: &[
            "id",
            "calendar_id",
            "event_id",
            "token_hash",
            "allows_caldav",
            "created_by",
            "expires_at",
            "revoked_at",
            "last_accessed_at",
        ],
        filter: "WHERE event_id IS NULL OR event_id IN (SELECT id FROM events WHERE deleted_at IS NULL)",
        defaults: &[("allows_caldav", JsonDefault::Bool(false))],
    },
    TableSpec {
        table: "subscriptions",
        columns: &["id", "user_id", "share_id", "color", "order_index"],
        filter: "",
        defaults: &[("order_index", JsonDefault::Int(0))],
    },
    TableSpec {
        table: "rules",
        columns: &[
            "id",
            "tenant_id",
            "calendar_id",
            "name",
            "enabled",
            "trigger_type",
            "conditions",
            "actions",
            "position",
            "created_by",
        ],
        filter: "",
        defaults: &[
            ("enabled", JsonDefault::Bool(true)),
            ("conditions", JsonDefault::List(&[])),
            ("actions", JsonDefault::List(&[])),
            ("position", JsonDefault::Int(0)),
        ],
    },
    // config_encrypted stays byte-for-byte as exported: restoring never
    // decrypts (decryption needs the same environment key it was saved with).
    TableSpec {
        table: "notification_providers",
        columns: &[
            "id",
            "tenant_id",
            "kind",
            "name",
            "config_encrypted",
            "enabled",
        ],
        filter: "",
        defaults: &[("enabled", JsonDefault::Bool(true))],
    },
    TableSpec {
        table: "notifications",
        columns: &[
            "id",
            "user_id",
            "channel",
            "title",
            "body",
            "data",
            "dedupe_key",
            "read_at",
            "sent_at",
            "send_attempts",
            "send_error",
            "created_at",
        ],
        filter: "",
        defaults: &[("send_attempts", JsonDefault::Int(0))],
    },
    TableSpec {
        table: "categories",
        columns: &[
            "id",
            "tenant_id",
            "calendar_id",
            "slug",
            "name",
            "color",
            "sort_order",
            "created_by",
        ],
        filter: "",
        defaults: &[("sort_order", JsonDefault::Int(0))],
    },
    TableSpec {
        table: "push_subscriptions",
        columns: &["id", "user_id", "endpoint", "p256dh", "auth", "user_agent"],
        filter: "",
        defaults: &[],
    },
];

/// Exports the whole application state as a JSON document.
pub async fn export(pool: &PgPool) -> Result<Value, DbError> {
    let mut document = json!({"format": "calendar-server-backup", "version": FORMAT_VERSION});
    for spec in TABLES {
        let columns = spec.columns.join(", ");
        let sql = format!(
            "SELECT COALESCE(json_agg(s), '[]'::json)::jsonb FROM \
                 (SELECT {columns} FROM {} {}) s",
            spec.table, spec.filter
        );
        document[spec.table] = sqlx::query_scalar::<_, Value>(&sql).fetch_one(pool).await?;
    }
    Ok(document)
}

/// Restores into an empty database, keeping original UUIDs.
pub async fn import(pool: &PgPool, document: &Value) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    for spec in TABLES {
        let empty: Vec<Value> = Vec::new();
        let rows: &[Value] = document
            .get(spec.table)
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        if matches!(spec.table, "events" | "tasks") {
            // Masters first, then live RECURRENCE-ID exceptions, so the
            // self-referencing master FK resolves within the batch.
            let master_key = if spec.table == "events" {
                "master_event_id"
            } else {
                "master_task_id"
            };
            let (masters, exceptions): (Vec<Value>, Vec<Value>) = rows
                .iter()
                .cloned()
                .partition(|row| row.get(master_key).is_none_or(Value::is_null));
            restore_table(&mut tx, spec, &masters).await?;
            restore_table(&mut tx, spec, &exceptions).await?;
        } else {
            restore_table(&mut tx, spec, rows).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Inserts one table's rows from their JSON form. `jsonb_populate_record`
/// casts every JSON value to the table's column type; only the spec's columns
/// are inserted, so generated columns (events.is_exception/search_vector) and
/// timestamp/ctag bookkeeping stay untouched.
async fn restore_table(
    tx: &mut sqlx::Transaction<'static, sqlx::Postgres>,
    spec: &TableSpec,
    rows: &[Value],
) -> Result<(), DbError> {
    let columns = spec.columns.join(", ");
    let table = spec.table;
    let sql = format!(
        "INSERT INTO {table} ({columns}) SELECT {columns} \
             FROM jsonb_populate_record(NULL::{table}, $1) \
         ON CONFLICT DO NOTHING",
    );
    for row in rows {
        let mut row = row.clone();
        for (key, default) in spec.defaults {
            if row.get(*key).is_none_or(Value::is_null) {
                row[*key] = default.to_value();
            }
        }
        sqlx::query(&sql).bind(row).execute(&mut **tx).await?;
    }
    Ok(())
}

#[cfg(test)]
mod backup_tests {
    use super::*;
    use sqlx::postgres::types::PgInterval;
    use uuid::Uuid;

    fn uid(seed: u128) -> Uuid {
        Uuid::from_u128(seed)
    }

    /// Seeds a representative dataset: owner ACL, recurring event with an
    /// exception override, an email attendee and an SMS-only attendee, an
    /// alarm, an attachment, a location, a share + subscription, a rule, a
    /// provider (encrypted config), a notification, a category, an address
    /// book with a contact and its email/tel, tenant membership, and per-user
    /// notify prefs. Returns the event/row ids for assertions.
    async fn seed(pool: &PgPool) {
        let sql = format!(
            "INSERT INTO users (id, username, email, display_name, password_hash, is_admin, \
                 timezone, notify_email) \
             VALUES ('{u}', 'alice', 'alice@example.com', 'Alice', 'argon2id$hash', false, \
                 'Europe/Oslo', false); \
             INSERT INTO tenants (id, slug, name, is_personal) \
             VALUES ('{t}', 'alice', 'Alice', true); \
             INSERT INTO tenant_members (tenant_id, user_id, role) VALUES ('{t}', '{u}', 'owner'); \
             INSERT INTO calendars (id, tenant_id, slug, name, description, color, timezone) \
             VALUES ('{c}', '{t}', 'personal', 'Personal', 'Mine', '#3366cc', 'Europe/Oslo'); \
             INSERT INTO calendar_acl (calendar_id, principal_user_id, capability, can_manage_acl) \
             VALUES ('{c}', '{u}', 'owner', true); \
             INSERT INTO locations (id, provider, provider_place_id, display_name, \
                 formatted_address, street_address, locality, administrative_area, postal_code, \
                 country, latitude, longitude, website, phone, provider_metadata) \
             VALUES ('{l}', 'google', 'place-1', 'City Hall', '1 Main St, Springfield', \
                 '1 Main St', 'Springfield', 'IL', '62701', 'US', 39.8, -89.65, \
                 'https://city.example', '+15550100', '{{\"hours\": \"9-5\"}}'); \
             INSERT INTO events (id, calendar_id, uid, starts_at, ends_at, tzid, rrule, summary, \
                 location_id, organizer_email, organizer_name, sequence, etag) \
             VALUES ('{e1}', '{c}', 'standup-1', '2026-09-21T07:00:00+00:00', \
                 '2026-09-21T07:30:00+00:00', 'Europe/Oslo', 'FREQ=WEEKLY', 'Standup', '{l}', \
                 'alice@example.com', 'Alice', 3, 'etag-1'); \
             INSERT INTO events (id, calendar_id, uid, master_event_id, recurrence_id, starts_at, \
                 ends_at, tzid, summary, organizer_email) \
             VALUES ('{e2}', '{c}', 'standup-1', '{e1}', '2026-09-28 07:00:00', \
                 '2026-09-28T08:00:00+00:00', '2026-09-28T08:30:00+00:00', 'Europe/Oslo', \
                 'Standup (moved)', 'alice@example.com'); \
             INSERT INTO event_attendees (id, event_id, email, display_name, role, partstat, rsvp) \
             VALUES ('{a1}', '{e1}', 'bob@example.com', 'Bob', 'REQ-PARTICIPANT', 'ACCEPTED', \
                 true); \
             INSERT INTO event_attendees (id, event_id, email, telephone, role, partstat) \
             VALUES ('{a2}', '{e2}', NULL, '+15550101', 'OPT-PARTICIPANT', 'TENTATIVE'); \
             INSERT INTO event_alarms (id, event_id, action, related, offset_interval, \
                 description, notify_channels) \
             VALUES ('{al}', '{e1}', 'DISPLAY', 'START', '-00:15:00', 'Standup soon', \
                 '{{email,push}}'); \
             INSERT INTO attachments (id, event_id, filename, content_type, byte_size, sha256, \
                 data) \
             VALUES ('{at}', '{e1}', 'notes.txt', 'text/plain', 5, '0123456789abcdef'::bytea, \
                 'hello'); \
             INSERT INTO tasks (id, calendar_id, uid, href, due_at, summary, description_text, \
                 status, percent_complete, priority, class, categories, extra_props, rrule, \
                 organizer_email, sequence, etag) \
             VALUES ('{tk1}', '{c}', 'write-report', 'write-report.ics', \
                 '2026-09-22T17:00:00+00:00', 'Write the report', 'quarterly numbers', \
                 'IN-PROCESS', 40, 5, 'PUBLIC', '{{work}}', \
                 '[{{\"name\": \"X-TEST\", \"value\": \"1\"}}]', 'FREQ=WEEKLY', \
                 'alice@example.com', 2, 'tetag-1'); \
             INSERT INTO tasks (id, calendar_id, uid, master_task_id, recurrence_id, due_at, \
                 summary) \
             VALUES ('{tk2}', '{c}', 'write-report', '{tk1}', '2026-09-29 17:00:00', \
                 '2026-09-29T18:00:00+00:00', 'Write the report (moved)'); \
             INSERT INTO tasks (id, calendar_id, uid, summary, status, parent_uid, sort_order) \
             VALUES ('{tk3}', '{c}', 'collect-numbers', 'Collect the numbers', 'NEEDS-ACTION', \
                 'write-report', 1); \
             INSERT INTO task_attendees (id, task_id, email, display_name, role, partstat, rsvp) \
             VALUES ('{ta1}', '{tk1}', 'carol@example.com', 'Carol', 'REQ-PARTICIPANT', \
                 'TENTATIVE', true); \
             INSERT INTO task_alarms (id, task_id, action, related, offset_interval, description, \
                 notify_channels) \
             VALUES ('{tal}', '{tk1}', 'DISPLAY', 'END', '-01:00:00', 'Report due soon', \
                 '{{email,sms}}'); \
             INSERT INTO journals (id, calendar_id, uid, summary, description_text, status, \
                 class, extra_props) \
             VALUES ('{jn1}', '{c}', 'note-1', 'Meeting notes', 'discussed the budget', \
                 'FINAL', 'PUBLIC', '[{{\"name\": \"X-NOTE\", \"value\": \"keep\"}}]'); \
             INSERT INTO journals (id, calendar_id, uid, start_date, summary, status, categories) \
             VALUES ('{jn2}', '{c}', 'note-2', '2026-09-20', 'Day one', 'DRAFT', '{{work}}'); \
             INSERT INTO public_shares (id, calendar_id, token_hash, allows_caldav, created_by) \
             VALUES ('{s}', '{c}', 'aabbcc'::bytea, true, '{u}'); \
             INSERT INTO subscriptions (id, user_id, share_id, color, order_index) \
             VALUES ('{sb}', '{u}', '{s}', '#ff0000', 2); \
             INSERT INTO rules (id, tenant_id, calendar_id, name, trigger_type) \
             VALUES ('{r}', '{t}', '{c}', 'Flag work', 'event_created'); \
             INSERT INTO notification_providers (id, tenant_id, kind, name, config_encrypted) \
             VALUES ('{p}', '{t}', 'smtp', 'Main', 'cipherbytes'); \
             INSERT INTO notifications (id, user_id, channel, title, body) \
             VALUES ('{n}', '{u}', 'in_app', 'Hi', 'Body'); \
             INSERT INTO categories (id, tenant_id, slug, name, color, sort_order, created_by) \
             VALUES ('{k}', '{t}', 'work', 'Work', '#0066ff', 1, '{u}'); \
             INSERT INTO address_books (id, tenant_id, owner_user_id, slug, name) \
             VALUES ('{b}', '{t}', '{u}', 'contacts', 'Contacts'); \
             INSERT INTO contacts (id, address_book_id, uid, full_name, raw_vcard, etag) \
             VALUES ('{ct}', '{b}', 'c-1', 'Bob', 'BEGIN:VCARD\\nEND:VCARD', 'ce1'); \
             INSERT INTO contact_emails (id, contact_id, email, is_primary) \
             VALUES ('{ce}', '{ct}', 'bob@example.com', true); \
             INSERT INTO contact_tels (id, contact_id, number, is_mobile, is_primary) \
             VALUES ('{ct2}', '{ct}', '+15550101', true, true);",
            u = uid(0x11),
            t = uid(0x12),
            c = uid(0x13),
            l = uid(0x14),
            e1 = uid(0x15),
            e2 = uid(0x16),
            a1 = uid(0x0161),
            a2 = uid(0x0162),
            al = uid(0x17),
            at = uid(0x18),
            s = uid(0x19),
            sb = uid(0x1a),
            r = uid(0x1b),
            p = uid(0x1c),
            n = uid(0x1d),
            k = uid(0x1e),
            b = uid(0x1f),
            ct = uid(0x20),
            ce = uid(0x21),
            ct2 = uid(0x22),
            tk1 = uid(0x23),
            tk2 = uid(0x24),
            tk3 = uid(0x25),
            ta1 = uid(0x26),
            tal = uid(0x27),
            jn1 = uid(0x28),
            jn2 = uid(0x29)
        );
        for statement in sql.split(';').filter(|s| !s.trim().is_empty()) {
            sqlx::query(statement).execute(pool).await.unwrap();
        }
    }

    async fn create_database(admin: &PgPool, base_url: &str, name: &str) -> PgPool {
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(admin)
            .await
            .unwrap();
        let url = format!("{}/{}", base_url.rsplit_once('/').unwrap().0, name);
        let pool = super::super::connect(&url, 4).await.unwrap();
        super::super::migrate(&pool).await.unwrap();
        pool
    }

    async fn drop_database(admin: &PgPool, name: &str) {
        let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
            .execute(admin)
            .await;
    }

    /// Full round trip: seed DB A, export, restore into empty DB B, and assert
    /// the state that was previously lost or mangled survives: owner ACL,
    /// attendee partstat (including an SMS-only attendee), the live exception
    /// row with its real organizer, the location link, alarm trigger and
    /// channels, attachment bytes, provider encrypted config, user prefs.
    #[tokio::test]
    async fn backup_round_trip_preserves_full_state() {
        let Some(base_url) = std::env::var("CALENDAR_DB_TEST_URL").ok() else {
            eprintln!("skipping: CALENDAR_DB_TEST_URL is not set");
            return;
        };
        let admin = super::super::connect(&base_url, 2).await.unwrap();
        let suffix = Uuid::new_v4().simple();
        let name_a = format!("calbak_a_{suffix}");
        let name_b = format!("calbak_b_{suffix}");
        let db_a = create_database(&admin, &base_url, &name_a).await;
        let db_b = create_database(&admin, &base_url, &name_b).await;

        seed(&db_a).await;
        let document = export(&db_a).await.unwrap();
        assert_eq!(document["format"], "calendar-server-backup");
        assert_eq!(document["version"], FORMAT_VERSION);
        assert_eq!(document["calendar_acl"].as_array().unwrap().len(), 1);
        assert_eq!(
            document["notification_providers"].as_array().unwrap().len(),
            1
        );
        assert_eq!(document["tasks"].as_array().unwrap().len(), 3);
        assert_eq!(document["journals"].as_array().unwrap().len(), 2);
        assert_eq!(document["task_attendees"].as_array().unwrap().len(), 1);
        assert_eq!(document["task_alarms"].as_array().unwrap().len(), 1);
        import(&db_b, &document).await.unwrap();

        // (user, tenant, calendar, location, events, ...) ids from the seed.
        let (u, c, l, e1, e2) = (uid(0x11), uid(0x13), uid(0x14), uid(0x15), uid(0x16));

        // Owner ACL is restored, so the calendar is reachable again.
        let (capability, can_manage): (String, bool) = sqlx::query_as(
            "SELECT capability, can_manage_acl FROM calendar_acl
             WHERE calendar_id = $1 AND principal_user_id = $2",
        )
        .bind(c)
        .bind(u)
        .fetch_one(&db_b)
        .await
        .unwrap();
        assert_eq!((capability.as_str(), can_manage), ("owner", true));

        // Tenant membership survives.
        let role: String = sqlx::query_scalar(
            "SELECT role FROM tenant_members WHERE tenant_id = $1 AND user_id = $2",
        )
        .bind(uid(0x12))
        .bind(u)
        .fetch_one(&db_b)
        .await
        .unwrap();
        assert_eq!(role, "owner");

        // Attendee partstat and the SMS-only attendee identity survive.
        let partstat: String = sqlx::query_scalar(
            "SELECT partstat FROM event_attendees WHERE event_id = $1 AND email = 'bob@example.com'",
        )
        .bind(e1)
        .fetch_one(&db_b)
        .await
        .unwrap();
        assert_eq!(partstat, "ACCEPTED");
        let (sms_partstat, telephone): (String, String) = sqlx::query_as(
            "SELECT partstat, telephone FROM event_attendees WHERE event_id = $1 AND email IS NULL",
        )
        .bind(e2)
        .fetch_one(&db_b)
        .await
        .unwrap();
        assert_eq!(
            (sms_partstat.as_str(), telephone.as_str()),
            ("TENTATIVE", "+15550101")
        );

        // The live exception row keeps its master link, RECURRENCE-ID and the
        // real organizer (not the old 'restored@localhost' placeholder).
        let (master, recurrence_id, organizer): (Uuid, chrono::NaiveDateTime, String) =
            sqlx::query_as(
                "SELECT master_event_id, recurrence_id, organizer_email::text
                 FROM events WHERE id = $1",
            )
            .bind(e2)
            .fetch_one(&db_b)
            .await
            .unwrap();
        assert_eq!(master, e1);
        assert_eq!(
            recurrence_id,
            chrono::NaiveDateTime::new(
                chrono::NaiveDate::from_ymd_opt(2026, 9, 28).unwrap(),
                chrono::NaiveTime::from_hms_opt(7, 0, 0).unwrap(),
            )
        );
        assert_eq!(organizer, "alice@example.com");

        // Location link and location payload survive.
        let (location_id, display_name): (Uuid, String) = sqlx::query_as(
            "SELECT e.location_id, lo.display_name FROM events e
             JOIN locations lo ON lo.id = e.location_id WHERE e.id = $1",
        )
        .bind(e1)
        .fetch_one(&db_b)
        .await
        .unwrap();
        assert_eq!((location_id, display_name.as_str()), (l, "City Hall"));

        // Alarm trigger data and notify channels survive exactly.
        let (offset, channels): (PgInterval, Vec<String>) = sqlx::query_as(
            "SELECT offset_interval, notify_channels FROM event_alarms WHERE id = $1",
        )
        .bind(uid(0x17))
        .fetch_one(&db_b)
        .await
        .unwrap();
        assert_eq!(offset.microseconds, -900_000_000);
        assert_eq!(channels, vec!["email".to_string(), "push".to_string()]);

        // Attachments (bytea through the JSON path) survive byte-for-byte.
        let data: Vec<u8> = sqlx::query_scalar("SELECT data FROM attachments WHERE id = $1")
            .bind(uid(0x18))
            .fetch_one(&db_b)
            .await
            .unwrap();
        assert_eq!(data, b"hello");

        // Provider config stays encrypted, byte-for-byte.
        let config: Vec<u8> =
            sqlx::query_scalar("SELECT config_encrypted FROM notification_providers WHERE id = $1")
                .bind(uid(0x1c))
                .fetch_one(&db_b)
                .await
                .unwrap();
        assert_eq!(config, b"cipherbytes");

        // The task master keeps due, status, percent, categories and its
        // extra_props verbatim (the private-data escape hatch).
        let (due, status, percent, cats, extras): (
            chrono::DateTime<chrono::Utc>,
            String,
            i16,
            Vec<String>,
            serde_json::Value,
        ) = sqlx::query_as(
            "SELECT due_at, status, percent_complete, categories, extra_props
             FROM tasks WHERE id = $1",
        )
        .bind(uid(0x23))
        .fetch_one(&db_b)
        .await
        .unwrap();
        assert_eq!(due.to_rfc3339(), "2026-09-22T17:00:00+00:00");
        assert_eq!(
            (status.as_str(), percent, cats),
            ("IN-PROCESS", 40, vec!["work".to_string()])
        );
        assert_eq!(
            extras,
            serde_json::json!([{"name": "X-TEST", "value": "1"}])
        );

        // The task's RECURRENCE-ID override reattaches to its master.
        let (master, recurrence_id): (Uuid, chrono::NaiveDateTime) =
            sqlx::query_as("SELECT master_task_id, recurrence_id FROM tasks WHERE id = $1")
                .bind(uid(0x24))
                .fetch_one(&db_b)
                .await
                .unwrap();
        assert_eq!(master, uid(0x23));
        assert_eq!(
            recurrence_id,
            chrono::NaiveDateTime::new(
                chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap(),
                chrono::NaiveTime::from_hms_opt(17, 0, 0).unwrap(),
            )
        );

        // The subtask keeps its raw parent_uid chain and manual order.
        let (parent, sort_order): (String, i64) =
            sqlx::query_as("SELECT parent_uid, sort_order FROM tasks WHERE id = $1")
                .bind(uid(0x25))
                .fetch_one(&db_b)
                .await
                .unwrap();
        assert_eq!((parent.as_str(), sort_order), ("write-report", 1));

        // Task attendee and alarm (related END = DUE) survive exactly.
        let partstat: String = sqlx::query_scalar(
            "SELECT partstat FROM task_attendees WHERE task_id = $1 AND email = 'carol@example.com'",
        )
        .bind(uid(0x23))
        .fetch_one(&db_b)
        .await
        .unwrap();
        assert_eq!(partstat, "TENTATIVE");
        let (offset, channels): (PgInterval, Vec<String>) = sqlx::query_as(
            "SELECT offset_interval, notify_channels FROM task_alarms WHERE id = $1",
        )
        .bind(uid(0x27))
        .fetch_one(&db_b)
        .await
        .unwrap();
        assert_eq!(offset.microseconds, -3_600_000_000);
        assert_eq!(channels, vec!["email".to_string(), "sms".to_string()]);

        // The undated journal stays undated (a note) with its extra_props;
        // the dated one keeps its DATE-valued DTSTART.
        let (starts_at, start_date, jstatus, jextras): (
            Option<chrono::DateTime<chrono::Utc>>,
            Option<chrono::NaiveDate>,
            String,
            serde_json::Value,
        ) = sqlx::query_as(
            "SELECT starts_at, start_date, status, extra_props FROM journals WHERE uid = 'note-1'",
        )
        .fetch_one(&db_b)
        .await
        .unwrap();
        assert_eq!(
            (starts_at.is_none(), start_date.is_none(), jstatus.as_str()),
            (true, true, "FINAL")
        );
        assert_eq!(
            jextras,
            serde_json::json!([{"name": "X-NOTE", "value": "keep"}])
        );
        let day: chrono::NaiveDate =
            sqlx::query_scalar("SELECT start_date FROM journals WHERE uid = 'note-2'")
                .fetch_one(&db_b)
                .await
                .unwrap();
        assert_eq!(day, chrono::NaiveDate::from_ymd_opt(2026, 9, 20).unwrap());

        // User prefs and share/subscription/rule/category/contact counts.
        let (tz, notify_email): (Option<String>, bool) =
            sqlx::query_as("SELECT timezone, notify_email FROM users WHERE id = $1")
                .bind(u)
                .fetch_one(&db_b)
                .await
                .unwrap();
        assert_eq!((tz.as_deref(), notify_email), (Some("Europe/Oslo"), false));
        for (table, expected) in [
            ("public_shares", 1),
            ("subscriptions", 1),
            ("rules", 1),
            ("notifications", 1),
            ("categories", 1),
            ("address_books", 1),
            ("contacts", 1),
            ("contact_emails", 1),
            ("contact_tels", 1),
            ("calendars", 1),
            ("locations", 1),
            ("events", 2),
            ("event_attendees", 2),
            ("event_alarms", 1),
            ("attachments", 1),
            ("tasks", 3),
            ("task_attendees", 1),
            ("task_alarms", 1),
            ("journals", 2),
            ("tenant_members", 1),
        ] {
            let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(&db_b)
                .await
                .unwrap();
            assert_eq!(count, expected, "table {table}");
        }

        drop_database(&admin, &name_a).await;
        drop_database(&admin, &name_b).await;
    }
}
